[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Pdf,

    [string]$OutputDirectory,
    [string]$TurboOcrRoot,
    [string]$TensorRTDir = 'D:\TensorRT',
    [string]$CudaDir = 'D:\cuda',
    [string]$OpenCVDir = 'D:\tools\vcpkg\installed\x64-windows\share\opencv4',
    [string]$OpenCVBin = 'D:\tools\vcpkg\installed\x64-windows\bin',
    [ValidateRange(1, 32)]
    [int]$RecognitionBatch = 8,
    [ValidateRange(1, 120000000)]
    [int]$MaxPagePixels = 12000000,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$documentRoot = Split-Path -Parent $PSScriptRoot
$workspaceRoot = Split-Path -Parent $documentRoot
if (-not $TurboOcrRoot) {
    $TurboOcrRoot = Join-Path $documentRoot 'turboocr'
}
if (-not $OutputDirectory) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $OutputDirectory = Join-Path $workspaceRoot ".agent\scratch\ocr-benchmark\tensorrt-smoke-$stamp"
}

$Pdf = (Resolve-Path -LiteralPath $Pdf).Path
$TurboOcrRoot = (Resolve-Path -LiteralPath $TurboOcrRoot).Path
foreach ($required in @($TensorRTDir, $CudaDir, $OpenCVDir, $OpenCVBin)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Required path does not exist: $required"
    }
}

if (-not $SkipBuild) {
    & (Join-Path $TurboOcrRoot 'scripts\build_windows_trt.ps1') `
        -TextPipeline `
        -TensorRTDir $TensorRTDir `
        -CudaDir $CudaDir `
        -OpenCVDir $OpenCVDir
    if ($LASTEXITCODE -ne 0) { throw "TurboOCR build failed with exit code $LASTEXITCODE" }
}

$worker = Join-Path $TurboOcrRoot 'build-windows-trt-text-ninja\turboocr-text.exe'
foreach ($required in @(
    $worker,
    (Join-Path $TurboOcrRoot 'models\det_tiny.onnx'),
    (Join-Path $TurboOcrRoot 'models\rec_tiny.onnx'),
    (Join-Path $TurboOcrRoot 'models\keys_tiny.txt')
)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Required TensorRT OCR file does not exist: $required"
    }
}

$oldPath = $env:PATH
$oldGraphs = $env:TURBO_OCR_CUDA_GRAPHS
$oldOptLevel = $env:TRT_OPT_LEVEL
try {
    $env:PATH = "$(Join-Path $TensorRTDir 'lib');$(Join-Path $TensorRTDir 'bin');$(Join-Path $CudaDir 'bin');$OpenCVBin;$env:PATH"
    $env:TURBO_OCR_CUDA_GRAPHS = '0'
    $env:TRT_OPT_LEVEL = '3'

    Push-Location $TurboOcrRoot
    try {
        $probeTimer = [Diagnostics.Stopwatch]::StartNew()
        & $worker --probe `
            --det .\models\det_tiny.onnx `
            --rec .\models\rec_tiny.onnx `
            --dict .\models\keys_tiny.txt `
            --rec-batch $RecognitionBatch
        $probeCode = $LASTEXITCODE
        $probeTimer.Stop()
    }
    finally {
        Pop-Location
    }
    if ($probeCode -ne 0) { throw "TensorRT inference probe failed with exit code $probeCode" }
    Write-Host ("TensorRT inference probe passed in {0:N3} s" -f $probeTimer.Elapsed.TotalSeconds)

    Push-Location $workspaceRoot
    try {
        cargo build -p lege-document-ocr-cli --release
        if ($LASTEXITCODE -ne 0) { throw "lege-document-ocr release build failed with exit code $LASTEXITCODE" }

        $jobTimer = [Diagnostics.Stopwatch]::StartNew()
        & .\target\release\lege-ocr.exe batch $Pdf `
            --output $OutputDirectory `
            --backend tensorrt-paddle `
            --tensorrt-ocr-root $TurboOcrRoot `
            --tensorrt-dll-dir (Join-Path $TensorRTDir 'lib') `
            --tensorrt-dll-dir (Join-Path $TensorRTDir 'bin') `
            --tensorrt-dll-dir (Join-Path $CudaDir 'bin') `
            --tensorrt-dll-dir $OpenCVBin `
            --tensorrt-rec-batch $RecognitionBatch `
            --force-ocr `
            --render-dpi 300 `
            --max-page-pixels $MaxPagePixels `
            --format json,text `
            --workers 1 `
            --force `
            --no-spellcheck
        $jobCode = $LASTEXITCODE
        $jobTimer.Stop()
    }
    finally {
        Pop-Location
    }
    if ($jobCode -ne 0) { throw "TensorRT document OCR test failed with exit code $jobCode" }
    Write-Host ("TensorRT document OCR passed in {0:N3} s; output: {1}" -f $jobTimer.Elapsed.TotalSeconds, $OutputDirectory)
}
finally {
    $env:PATH = $oldPath
    $env:TURBO_OCR_CUDA_GRAPHS = $oldGraphs
    $env:TRT_OPT_LEVEL = $oldOptLevel
}
