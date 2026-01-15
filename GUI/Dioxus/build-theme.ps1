#!/usr/bin/env pwsh
# Lege GUI Theme Builder
# Builds the GUI with either the default Classic Windows theme or the alternate Cotinga Mystique theme.

param(
    [Parameter(Position = 0)]
    [ValidateSet('classic', 'cotinga', 'both')]
    [string]$Theme = 'classic',

    [switch]$Release,
    [switch]$DebugLogging,
    [switch]$Run
)

$buildMode = if ($Release) { "--release" } else { "" }
$debugFeature = if ($DebugLogging) { "debug-logging" } else { "" }

function Invoke-Build {
    param(
        [string]$ThemeFeature,
        [string]$DisplayName
    )

    Write-Host ""
    Write-Host "==> Building with theme: $DisplayName" -ForegroundColor Cyan

    $features = @()
    if ($ThemeFeature) { $features += $ThemeFeature }
    if ($debugFeature) { $features += $debugFeature }

    $featureArg = if ($features.Count -gt 0) {
        "--features `"$($features -join ',')`""
    } else {
        ""
    }

    $buildCmd = "cargo build $buildMode $featureArg".Trim()
    Write-Host "Command: $buildCmd" -ForegroundColor Gray
    Invoke-Expression $buildCmd

    if ($LASTEXITCODE -ne 0) {
        Write-Host "Build failed." -ForegroundColor Red
        exit $LASTEXITCODE
    }

    Write-Host "Build succeeded." -ForegroundColor Green

    if ($Run) {
        $runCmd = "cargo run $buildMode $featureArg".Trim()
        Write-Host "Launching: $runCmd" -ForegroundColor Yellow
        Invoke-Expression $runCmd
    }
}

Write-Host "========================================"
Write-Host "        Lege GUI Theme Builder          "
Write-Host "========================================"

switch ($Theme) {
    'classic' {
        Invoke-Build -ThemeFeature "" -DisplayName "Classic Windows (default)"
    }
    'cotinga' {
        Invoke-Build -ThemeFeature "cotinga-theme" -DisplayName "Cotinga Mystique"
    }
    'both' {
        if ($Run) {
            Write-Host ""
            Write-Host "Note: cannot run both builds automatically. Running is disabled." -ForegroundColor Yellow
        }

        Invoke-Build -ThemeFeature "" -DisplayName "Classic Windows (default)"

        $binDir = if ($Release) { "release" } else { "debug" }
        $exePath = Join-Path "target" $binDir | Join-Path -ChildPath "lege-gui.exe"

        $classicCopy = Join-Path "target" $binDir | Join-Path -ChildPath "lege-gui-classic.exe"
        if (Test-Path $exePath) {
            Copy-Item $exePath $classicCopy -Force
            Write-Host "Saved classic build to $classicCopy" -ForegroundColor Cyan
        }

        Invoke-Build -ThemeFeature "cotinga-theme" -DisplayName "Cotinga Mystique"

        $cotingaCopy = Join-Path "target" $binDir | Join-Path -ChildPath "lege-gui-cotinga.exe"
        if (Test-Path $exePath) {
            Copy-Item $exePath $cotingaCopy -Force
            Write-Host "Saved Cotinga build to $cotingaCopy" -ForegroundColor Cyan
        }

        Write-Host ""
        Write-Host "Both builds complete." -ForegroundColor Green
        Write-Host " Classic (default): $classicCopy" -ForegroundColor Gray
        Write-Host " Cotinga:          $cotingaCopy" -ForegroundColor Gray
    }
}

Write-Host ""
Write-Host "========================================"
