[CmdletBinding()]
param(
    [string]$PayloadDirectory,
    [string]$OutputDirectory,
    [string]$WorkingDirectory,
    [string]$ArchivePath,
    [string]$SevenZip = 'C:\Program Files\7-Zip\7z.exe',
    [string]$TensorRTDir = 'D:\TensorRT',
    [string]$CudaDir = 'D:\cuda',
    [string]$OpenCVDir = 'D:\tools\vcpkg\installed\x64-windows\share\opencv4',
    [string]$OpenCVBin = 'D:\tools\vcpkg\installed\x64-windows\bin',
    [string]$VCRuntimeDir,
    [switch]$SkipPayloadBuild,
    [switch]$SkipSmokeInstall,
    [switch]$KeepArchive
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$documentRoot = Split-Path -Parent $PSScriptRoot
$workspaceRoot = Split-Path -Parent $documentRoot
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'

if (-not $WorkingDirectory) {
    $WorkingDirectory = Join-Path $workspaceRoot ".agent\scratch\ocr-installer\build-$stamp"
}
New-Item -ItemType Directory -Path $WorkingDirectory -Force | Out-Null
$WorkingDirectory = (Resolve-Path -LiteralPath $WorkingDirectory).Path

if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $WorkingDirectory 'dist'
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path

if (-not $PayloadDirectory) {
    $PayloadDirectory = Join-Path $WorkingDirectory 'payload'
    if ($SkipPayloadBuild) {
        throw '-SkipPayloadBuild requires -PayloadDirectory.'
    }
    $packageArguments = @{
        OutputDirectory = $PayloadDirectory
        TensorRTDir = $TensorRTDir
        CudaDir = $CudaDir
        OpenCVDir = $OpenCVDir
        OpenCVBin = $OpenCVBin
    }
    if ($VCRuntimeDir) {
        $packageArguments.VCRuntimeDir = $VCRuntimeDir
    }
    & (Join-Path $PSScriptRoot 'package_windows_tensorrt.ps1') @packageArguments
    if ($LASTEXITCODE -ne 0) {
        throw "TensorRT payload staging failed with exit code $LASTEXITCODE"
    }
}

if (-not (Test-Path -LiteralPath $PayloadDirectory -PathType Container)) {
    throw "Payload directory does not exist: $PayloadDirectory"
}
$PayloadDirectory = (Resolve-Path -LiteralPath $PayloadDirectory).Path

$requiredPayload = @(
    'lege-ocr.exe',
    'package-manifest.json',
    'tensorrt\bin\turboocr-text.exe',
    'tensorrt\models\det_tiny.onnx',
    'tensorrt\models\rec_tiny.onnx',
    'tensorrt\models\keys_tiny.txt',
    'tensorrt\runtime\nvinfer_10.dll',
    'tensorrt\runtime\nvinfer_builder_resource_10.dll',
    'tensorrt\runtime\nvonnxparser_10.dll',
    'licenses\LICENSE'
)
foreach ($relativePath in $requiredPayload) {
    $candidate = Join-Path $PayloadDirectory $relativePath
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        throw "Installer payload is incomplete; missing $relativePath"
    }
}

if (-not (Test-Path -LiteralPath $SevenZip -PathType Leaf)) {
    $sevenZipCommand = Get-Command 7z.exe -ErrorAction SilentlyContinue
    if (-not $sevenZipCommand) {
        throw "7-Zip was not found at $SevenZip or on PATH."
    }
    $SevenZip = $sevenZipCommand.Source
}
$SevenZip = (Resolve-Path -LiteralPath $SevenZip).Path

$archiveWasExplicit = [bool]$ArchivePath
if (-not $ArchivePath) {
    $ArchivePath = Join-Path $WorkingDirectory 'lege-document-ocr-payload.7z'
}
$archiveParent = Split-Path -Parent $ArchivePath
if ($archiveParent) {
    New-Item -ItemType Directory -Path $archiveParent -Force | Out-Null
}
if (Test-Path -LiteralPath $ArchivePath -PathType Leaf) {
    Remove-Item -LiteralPath $ArchivePath -Force
}
$ArchivePath = [IO.Path]::GetFullPath($ArchivePath)

Write-Host 'Compressing the complete OCR runtime with solid LZMA2...'
$compressionArguments = @(
    'a', '-t7z', $ArchivePath, '.\*',
    '-m0=LZMA2', '-mx=9', '-md=128m', '-mfb=273', '-ms=on', '-mmt=on'
)
Push-Location $PayloadDirectory
try {
    & $SevenZip @compressionArguments
    if ($LASTEXITCODE -ne 0) {
        throw "7-Zip compression failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

& $SevenZip t $ArchivePath
if ($LASTEXITCODE -ne 0) {
    throw "7-Zip archive verification failed with exit code $LASTEXITCODE"
}
$archiveListing = (& $SevenZip l -slt $ArchivePath | Out-String)
if ($archiveListing -notmatch '(?m)^Method = LZMA2') {
    throw 'The payload archive was not encoded with LZMA2.'
}

Push-Location $workspaceRoot
try {
    cargo build -p lege-document-ocr-installer-winsafe --release --bin lege-document-ocr-uninstaller
    if ($LASTEXITCODE -ne 0) {
        throw "Uninstaller build failed with exit code $LASTEXITCODE"
    }

    $uninstallerPath = Join-Path $workspaceRoot 'target\release\lege-document-ocr-uninstaller.exe'
    $previousPayload = $env:LEGE_OCR_INSTALLER_PAYLOAD
    $previousUninstaller = $env:LEGE_OCR_UNINSTALLER_PATH
    $previousOfficial = $env:LEGE_OCR_BUILD_INSTALLER
    try {
        $env:LEGE_OCR_INSTALLER_PAYLOAD = $ArchivePath
        $env:LEGE_OCR_UNINSTALLER_PATH = $uninstallerPath
        $env:LEGE_OCR_BUILD_INSTALLER = '1'
        cargo build -p lege-document-ocr-installer-winsafe --release --bin lege-document-ocr-installer
        if ($LASTEXITCODE -ne 0) {
            throw "Installer build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        $env:LEGE_OCR_INSTALLER_PAYLOAD = $previousPayload
        $env:LEGE_OCR_UNINSTALLER_PATH = $previousUninstaller
        $env:LEGE_OCR_BUILD_INSTALLER = $previousOfficial
    }
}
finally {
    Pop-Location
}

$builtInstaller = Join-Path $workspaceRoot 'target\release\lege-document-ocr-installer.exe'
$installerPath = Join-Path $OutputDirectory 'Lege-Document-OCR-Setup-x64.exe'
Copy-Item -LiteralPath $builtInstaller -Destination $installerPath -Force

if (-not $SkipSmokeInstall) {
    $smokeDirectory = Join-Path $WorkingDirectory 'smoke-install'
    if (Test-Path -LiteralPath $smokeDirectory) {
        throw "Smoke-install directory must not already exist: $smokeDirectory"
    }
    $installArguments = @(
        '--quiet',
        '--install-dir', ('"{0}"' -f $smokeDirectory),
        '--no-shortcuts',
        '--no-register'
    )
    $installStart = @{
        FilePath = $installerPath
        ArgumentList = $installArguments
        Wait = $true
        PassThru = $true
        WindowStyle = 'Hidden'
    }
    $installProcess = Start-Process @installStart
    if ($installProcess.ExitCode -ne 0) {
        throw "Installer smoke run failed with exit code $($installProcess.ExitCode)"
    }

    $installedManifestPath = Join-Path $smokeDirectory 'package-manifest.json'
    $installedManifest = Get-Content -Raw -LiteralPath $installedManifestPath | ConvertFrom-Json
    foreach ($file in $installedManifest.files) {
        $installedFile = Join-Path $smokeDirectory ($file.path.Replace('/', '\'))
        if (-not (Test-Path -LiteralPath $installedFile -PathType Leaf)) {
            throw "Installed manifest file is missing: $($file.path)"
        }
        $actualHash = (Get-FileHash -LiteralPath $installedFile -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $file.sha256) {
            throw "Installed manifest hash mismatch: $($file.path)"
        }
    }

    $installedUninstaller = Join-Path $smokeDirectory 'lege-document-ocr-uninstaller.exe'
    $uninstallArguments = @('--quiet', '--install-dir', ('"{0}"' -f $smokeDirectory))
    $uninstallStart = @{
        FilePath = $installedUninstaller
        ArgumentList = $uninstallArguments
        Wait = $true
        PassThru = $true
        WindowStyle = 'Hidden'
    }
    $uninstallProcess = Start-Process @uninstallStart
    if ($uninstallProcess.ExitCode -ne 0) {
        throw "Uninstaller smoke run failed with exit code $($uninstallProcess.ExitCode)"
    }
    for ($attempt = 0; $attempt -lt 150 -and (Test-Path -LiteralPath $smokeDirectory); $attempt++) {
        Start-Sleep -Milliseconds 200
    }
    if (Test-Path -LiteralPath $smokeDirectory) {
        throw "Safe uninstaller did not remove the smoke installation: $smokeDirectory"
    }
}

$payloadBytes = (Get-ChildItem -LiteralPath $PayloadDirectory -File -Recurse |
    Measure-Object -Property Length -Sum).Sum
$archiveBytes = (Get-Item -LiteralPath $ArchivePath).Length
$installerHash = (Get-FileHash -LiteralPath $installerPath -Algorithm SHA256).Hash.ToLowerInvariant()
$metadata = [ordered]@{
    schema = 1
    product = 'Lege Document OCR'
    architecture = 'windows-x86_64'
    pipeline = 'TensorRT primary; WinOCR fallback only without an NVIDIA driver'
    archive_format = '7z'
    compression = 'LZMA2'
    solid = $true
    dictionary = '128 MiB'
    payload_bytes = $payloadBytes
    archive_bytes = $archiveBytes
    installer_bytes = (Get-Item -LiteralPath $installerPath).Length
    installer_sha256 = $installerHash
    smoke_install = (-not $SkipSmokeInstall)
}
$metadata | ConvertTo-Json -Depth 3 |
    Set-Content -LiteralPath (Join-Path $OutputDirectory 'build-metadata.json') -Encoding UTF8
"$installerHash  Lege-Document-OCR-Setup-x64.exe" |
    Set-Content -LiteralPath (Join-Path $OutputDirectory 'SHA256SUMS.txt') -Encoding ASCII

if (-not $KeepArchive -and -not $archiveWasExplicit) {
    Remove-Item -LiteralPath $ArchivePath -Force
}

Write-Host ("Installer: {0}" -f $installerPath)
Write-Host ("Payload: {0:N2} GiB; LZMA2 archive: {1:N2} GiB; installer: {2:N2} GiB" -f ($payloadBytes / 1GB), ($archiveBytes / 1GB), ((Get-Item -LiteralPath $installerPath).Length / 1GB))
Write-Host ("SHA-256: {0}" -f $installerHash)
