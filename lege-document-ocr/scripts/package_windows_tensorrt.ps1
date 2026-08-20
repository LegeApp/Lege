[CmdletBinding()]
param(
    [string]$OutputDirectory,
    [string]$TurboOcrRoot,
    [string]$TensorRTDir = 'D:\TensorRT',
    [string]$CudaDir = 'D:\cuda',
    [string]$OpenCVDir = 'D:\tools\vcpkg\installed\x64-windows\share\opencv4',
    [string]$OpenCVBin = 'D:\tools\vcpkg\installed\x64-windows\bin',
    [string]$VCRuntimeDir,
    [ValidateRange(1, 32)]
    [int]$RecognitionBatch = 8,
    [switch]$SkipRustBuild,
    [switch]$SkipWorkerBuild,
    [switch]$SkipDoctor
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$documentRoot = Split-Path -Parent $PSScriptRoot
$workspaceRoot = Split-Path -Parent $documentRoot
if (-not $TurboOcrRoot) {
    $TurboOcrRoot = Join-Path $documentRoot 'turboocr'
}
if (-not $OutputDirectory) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $OutputDirectory = Join-Path $workspaceRoot ".agent\scratch\ocr-package\lege-ocr-windows-x64-$stamp"
}

function Resolve-RequiredDirectory {
    param([string]$Path, [string]$Label)
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        throw "$Label directory does not exist: $Path"
    }
    return (Resolve-Path -LiteralPath $Path).Path
}

function Copy-RequiredFile {
    param([string]$Source, [string]$DestinationDirectory)
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw "Required package file does not exist: $Source"
    }
    New-Item -ItemType Directory -Path $DestinationDirectory -Force | Out-Null
    Copy-Item -LiteralPath $Source -Destination $DestinationDirectory -Force
}

function Find-VCRuntimeDirectory {
    if ($VCRuntimeDir) {
        return Resolve-RequiredDirectory $VCRuntimeDir 'Visual C++ runtime'
    }
    if ($env:VCToolsRedistDir) {
        $candidate = Join-Path $env:VCToolsRedistDir 'x64\Microsoft.VC143.CRT'
        if (Test-Path -LiteralPath $candidate -PathType Container) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    $visualStudioRoot = Join-Path $env:ProgramFiles 'Microsoft Visual Studio\2022'
    if (Test-Path -LiteralPath $visualStudioRoot -PathType Container) {
        $candidates = Get-ChildItem -LiteralPath $visualStudioRoot -Directory | ForEach-Object {
            $redistRoot = Join-Path $_.FullName 'VC\Redist\MSVC'
            if (Test-Path -LiteralPath $redistRoot -PathType Container) {
                Get-ChildItem -LiteralPath $redistRoot -Directory | ForEach-Object {
                    Join-Path $_.FullName 'x64\Microsoft.VC143.CRT'
                }
            }
        } | Where-Object { Test-Path -LiteralPath $_ -PathType Container }
        $selected = $candidates | Sort-Object -Descending | Select-Object -First 1
        if ($selected) {
            return (Resolve-Path -LiteralPath $selected).Path
        }
    }
    throw 'Visual C++ x64 app-local runtime was not found; pass -VCRuntimeDir.'
}

$TurboOcrRoot = Resolve-RequiredDirectory $TurboOcrRoot 'TurboOCR'
$TensorRTDir = Resolve-RequiredDirectory $TensorRTDir 'TensorRT'
$CudaDir = Resolve-RequiredDirectory $CudaDir 'CUDA'
$OpenCVDir = Resolve-RequiredDirectory $OpenCVDir 'OpenCV CMake'
$OpenCVBin = Resolve-RequiredDirectory $OpenCVBin 'OpenCV binary'
$VCRuntimeDir = Find-VCRuntimeDirectory

$outputParent = Split-Path -Parent $OutputDirectory
if ($outputParent) {
    New-Item -ItemType Directory -Path $outputParent -Force | Out-Null
}
if (Test-Path -LiteralPath $OutputDirectory) {
    $existing = Get-ChildItem -LiteralPath $OutputDirectory -Force | Select-Object -First 1
    if ($existing) {
        throw "Output directory must be empty: $OutputDirectory"
    }
} else {
    New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
}
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path

if (-not $SkipWorkerBuild) {
    Push-Location $TurboOcrRoot
    try {
        & (Join-Path $TurboOcrRoot 'scripts\build_windows_trt.ps1') `
            -TextPipeline `
            -TensorRTDir $TensorRTDir `
            -CudaDir $CudaDir `
            -OpenCVDir $OpenCVDir
        if ($LASTEXITCODE -ne 0) {
            throw "TurboOCR build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

if (-not $SkipRustBuild) {
    Push-Location $workspaceRoot
    try {
        cargo build -p lege-document-ocr-cli --release
        if ($LASTEXITCODE -ne 0) {
            throw "lege-ocr release build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

$tensorrtRoot = Join-Path $OutputDirectory 'tensorrt'
$runtimeDirectory = Join-Path $tensorrtRoot 'runtime'
$modelDirectory = Join-Path $tensorrtRoot 'models'
$binaryDirectory = Join-Path $tensorrtRoot 'bin'
$licenseDirectory = Join-Path $OutputDirectory 'licenses'

Copy-RequiredFile (Join-Path $workspaceRoot 'target\release\lege-ocr.exe') $OutputDirectory
Copy-RequiredFile (Join-Path $TurboOcrRoot 'build-windows-trt-text-ninja\turboocr-text.exe') $binaryDirectory
foreach ($model in @('det_tiny.onnx', 'rec_tiny.onnx', 'keys_tiny.txt')) {
    Copy-RequiredFile (Join-Path $TurboOcrRoot "models\$model") $modelDirectory
}

# The builder resource is intentionally included. It is dynamically loaded
# only for cold ONNX -> engine compilation, so ordinary import-table scans miss
# it even though a clean target machine cannot create its first engine without it.
foreach ($library in @(
    'nvinfer_10.dll',
    'nvinfer_builder_resource_10.dll',
    'nvinfer_plugin_10.dll',
    'nvinfer_vc_plugin_10.dll',
    'nvonnxparser_10.dll'
)) {
    Copy-RequiredFile (Join-Path $TensorRTDir "lib\$library") $runtimeDirectory
}
foreach ($library in @(
    'opencv_core4.dll',
    'opencv_imgproc4.dll',
    'opencv_imgcodecs4.dll',
    'jpeg62.dll',
    'libpng16.dll',
    'zlib1.dll'
)) {
    Copy-RequiredFile (Join-Path $OpenCVBin $library) $runtimeDirectory
}
Get-ChildItem -LiteralPath $VCRuntimeDir -Filter '*.dll' -File | ForEach-Object {
    Copy-RequiredFile $_.FullName $runtimeDirectory
}

Copy-RequiredFile (Join-Path $TurboOcrRoot 'LICENSE') $licenseDirectory
foreach ($package in @('opencv4', 'libjpeg-turbo', 'libpng', 'zlib')) {
    $copyright = Join-Path (Split-Path -Parent $OpenCVBin) "share\$package\copyright"
    if (Test-Path -LiteralPath $copyright -PathType Leaf) {
        Copy-Item -LiteralPath $copyright -Destination (Join-Path $licenseDirectory "$package.txt") -Force
    }
}

$files = Get-ChildItem -LiteralPath $OutputDirectory -File -Recurse | Sort-Object FullName | ForEach-Object {
    [ordered]@{
        path = $_.FullName.Substring($OutputDirectory.Length + 1).Replace('\', '/')
        bytes = $_.Length
        sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}
$manifest = [ordered]@{
    schema = 1
    product = 'Lege Document OCR'
    target = 'windows-x86_64'
    cuda_architectures = @('sm_89', 'compute_75_ptx')
    includes_cold_engine_builder = $true
    files = @($files)
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'package-manifest.json') -Encoding UTF8

if (-not $SkipDoctor) {
    Push-Location $OutputDirectory
    try {
        & (Join-Path $OutputDirectory 'lege-ocr.exe') doctor `
            --backend tensorrt-paddle `
            --tensorrt-rec-batch $RecognitionBatch `
            --json
        if ($LASTEXITCODE -ne 0) {
            throw "Packaged TensorRT doctor failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

$totalBytes = (Get-ChildItem -LiteralPath $OutputDirectory -File -Recurse | Measure-Object -Property Length -Sum).Sum
Write-Host ("Staged Lege OCR package: {0}" -f $OutputDirectory)
Write-Host ("Files: {0}; size: {1:N2} GiB" -f $files.Count, ($totalBytes / 1GB))
