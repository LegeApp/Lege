param(
    [Parameter()][string]$Configuration = "Release",
    [Parameter()][string]$OutputDir = ".\final-run",
    [Parameter()][switch]$CleanOutput = $false
)

Write-Host ""
Write-Host "=====================================" -ForegroundColor Cyan
Write-Host "  Building Complete Lege Application" -ForegroundColor Cyan
Write-Host "=====================================" -ForegroundColor Cyan
Write-Host ""

# Output directory handling
if (Test-Path $OutputDir) {
    if ($CleanOutput) {
        Write-Host "[CLEAN] Cleaning previous build (per -CleanOutput)..." -ForegroundColor Yellow
        Remove-Item $OutputDir -Recurse -Force
        Write-Host "   -> Previous build cleaned" -ForegroundColor Green
        Write-Host ""
    } else {
        Write-Host "[INFO] Output directory exists; preserving existing files (use -CleanOutput to remove)." -ForegroundColor Cyan
        Write-Host ""
    }
}

# 1. Build Rust CLI
Write-Host "[BUILD] Building Rust CLI (lege.exe)..." -ForegroundColor Yellow
$buildOutput = cargo build --release --bin lege 2>&1
if ($LASTEXITCODE -ne 0) { 
    Write-Host "   ERROR: Rust CLI build failed!" -ForegroundColor Red
    Write-Host $buildOutput -ForegroundColor Red
    exit $LASTEXITCODE 
}
Write-Host "   -> Rust CLI built successfully" -ForegroundColor Green
Write-Host ""

# 2. Build Rust FFI 
Write-Host "[BUILD] Building Rust FFI (reduced_lege_ffi.dll)..." -ForegroundColor Yellow
$ffiOutput = cargo build --release --manifest-path lege-ffi\Cargo.toml 2>&1
if ($LASTEXITCODE -ne 0) { 
    Write-Host "   ERROR: Rust FFI build failed!" -ForegroundColor Red
    Write-Host $ffiOutput -ForegroundColor Red
    exit $LASTEXITCODE 
}
Write-Host "   -> Rust FFI built successfully" -ForegroundColor Green
Write-Host ""

# 3. Build C# GUI
Write-Host "[BUILD] Building C# GUI (DocBrakeGUI.exe)..." -ForegroundColor Yellow
$tempGuiDir = "temp-gui-build"
$guiOutput = dotnet publish GUI\DocBrakeGUI -c $Configuration -r win-x64 --self-contained -o $tempGuiDir --verbosity quiet 2>&1
if ($LASTEXITCODE -ne 0) { 
    Write-Host "   ERROR: C# GUI build failed!" -ForegroundColor Red
    Write-Host $guiOutput -ForegroundColor Red
    exit $LASTEXITCODE 
}
Write-Host "   -> C# GUI built successfully" -ForegroundColor Green
Write-Host ""

# 4. Create unified output directory (preserve existing files unless -CleanOutput)
Write-Host "[DEPLOY] Creating unified deployment..." -ForegroundColor Yellow
if (-not (Test-Path $OutputDir)) {
    New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
}

# Copy CLI executable
if (Test-Path "target\release\lege.exe") {
    Copy-Item "target\release\lege.exe" $OutputDir
    Write-Host "   -> CLI executable copied (lege.exe)" -ForegroundColor Green
} else {
    Write-Host "   WARNING: CLI executable not found!" -ForegroundColor Yellow
}

# Copy FFI DLL
if (Test-Path "target\release\reduced_lege_ffi.dll") {
    Copy-Item "target\release\reduced_lege_ffi.dll" $OutputDir
    Write-Host "   -> FFI DLL copied (reduced_lege_ffi.dll)" -ForegroundColor Green
} else {
    Write-Host "   WARNING: FFI DLL not found!" -ForegroundColor Yellow
}

# Copy GUI files (excluding any duplicate FFI DLL)
if (Test-Path $tempGuiDir) {
    Get-ChildItem $tempGuiDir | Where-Object { $_.Name -ne "reduced_lege_ffi.dll" } | Copy-Item -Destination $OutputDir -Recurse -Force
    Write-Host "   -> GUI files copied (DocBrakeGUI.exe + dependencies)" -ForegroundColor Green
} else {
    Write-Host "   WARNING: GUI build directory not found!" -ForegroundColor Yellow
}

# Copy additional dependencies if they exist
$dependenciesCopied = 0

# Look for common dependency locations
$depPaths = @(
    "pdfium.dll", 
    "libpdfium.dll",
    "lib\libpdfium.so",
    "vcruntime*.dll"
)

foreach ($depPath in $depPaths) {
    $files = Get-ChildItem $depPath -ErrorAction SilentlyContinue
    if ($files) {
        $files | Copy-Item -Destination $OutputDir -ErrorAction SilentlyContinue
        $dependenciesCopied += $files.Count
    }
}

# Look for ONNX models directory
if (Test-Path "onnx-models") {
    Copy-Item "onnx-models\*" $OutputDir -Recurse -ErrorAction SilentlyContinue
    Write-Host "   -> ONNX models copied" -ForegroundColor Green
    $dependenciesCopied++
} elseif (Test-Path "models") {
    Copy-Item "models\*" $OutputDir -Recurse -ErrorAction SilentlyContinue
    Write-Host "   -> Models copied" -ForegroundColor Green
    $dependenciesCopied++
}

if ($dependenciesCopied -gt 0) {
    Write-Host "   -> Additional dependencies copied ($dependenciesCopied items)" -ForegroundColor Green
} else {
    Write-Host "   INFO: No additional dependencies found to copy" -ForegroundColor Cyan
    Write-Host "         You may need to manually copy: pdfium.dll, ONNX models, vcruntime DLLs" -ForegroundColor Cyan
}

# Cleanup temporary directory
if (Test-Path $tempGuiDir) {
    Remove-Item $tempGuiDir -Recurse -Force
}

Write-Host ""
Write-Host "[SUCCESS] Build Complete!" -ForegroundColor Green
Write-Host ""
Write-Host "Output Location: " -NoNewline -ForegroundColor Cyan
Write-Host (Resolve-Path $OutputDir).Path -ForegroundColor White
Write-Host ""
Write-Host "Ready to run:" -ForegroundColor Cyan
Write-Host "   CLI Mode: " -NoNewline -ForegroundColor White
Write-Host "lege.exe" -ForegroundColor Yellow
Write-Host "   GUI Mode: " -NoNewline -ForegroundColor White  
Write-Host "DocBrakeGUI.exe" -ForegroundColor Yellow
Write-Host ""

# Show directory contents
Write-Host "Contents of output directory:" -ForegroundColor Cyan
Get-ChildItem $OutputDir | Select-Object Name, Length | Format-Table -AutoSize | Out-String | Write-Host -ForegroundColor Gray

Write-Host "Note: Existing files in the output directory were preserved. Use -CleanOutput to remove them before a build if desired." -ForegroundColor DarkGray

Write-Host "=====================================" -ForegroundColor Cyan