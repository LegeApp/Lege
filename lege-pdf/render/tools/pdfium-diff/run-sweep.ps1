#Requires -Version 5.1
<#
.SYNOPSIS
    Runs the full differential PDF sweep over all configured corpus roots.

.DESCRIPTION
    PowerShell port of run-sweep.sh.

    The pdfium-diff tool writes pdfium-diff-out/results.csv beneath its current
    working directory. Existing file|page keys are retained by the tool, so the
    sweep remains resumable and newly added corpus rows are additive.

.PARAMETER LibPdfium
    Path to the Pdfium shared library. When omitted, the script looks for
    pdfium.dll or libpdfium.dll on Windows, and libpdfium.so elsewhere, beneath:
        <CORPUS_ROOT>\Rust-projects\pdfium-port-plan

.PARAMETER Scale
    Rendering scale passed to pdfium-diff. Default: 2.0

.PARAMETER WorkDir
    Directory from which pdfium-diff is run. Its output will be written to:
        <WorkDir>\pdfium-diff-out\results.csv

.PARAMETER DryRun
    Render one known-good fixture (or -DryRunFile) through the complete
    pdfium-diff path. Results go to a timestamped directory beneath WorkDir,
    never to the main sweep results.

.PARAMETER DryRunFile
    Optional PDF to use with -DryRun. The file must exist. When omitted, the
    script prefers pdf.js's basicapi.pdf and otherwise selects the first PDF
    found in the configured corpus roots.

.EXAMPLE
    .\run-sweep.ps1

.EXAMPLE
    .\run-sweep.ps1 'D:\Rust-projects\pdfium-port-plan\pdfium.dll' 2.0 'D:\sweep-work'

.EXAMPLE
    $env:CORPUS_ROOT = 'D:'
    .\run-sweep.ps1
#>

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [AllowEmptyString()]
    [string] $LibPdfium,

    [Parameter(Position = 1)]
    [ValidateNotNullOrEmpty()]
    [string] $Scale = '2.0',

    [Parameter(Position = 2)]
    [ValidateNotNullOrEmpty()]
    [string] $WorkDir = (Get-Location).Path,

    [switch] $DryRun,

    [ValidateNotNullOrEmpty()]
    [string] $DryRunFile
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Write-Stderr {
    param(
        [Parameter(Mandatory)]
        [string] $Message
    )

    [Console]::Error.WriteLine($Message)
}

$isWindowsPlatform =
    $PSVersionTable.PSEdition -eq 'Desktop' -or
    $env:OS -eq 'Windows_NT'

# On Windows, "D:" by itself can mean the drive's current directory rather than
# its root. Normalize a bare drive designator to "D:\" before joining paths.
$Root = if ([string]::IsNullOrWhiteSpace($env:CORPUS_ROOT)) {
    if ($isWindowsPlatform) {
        'D:\'
    }
    else {
        '/mnt/Samsung980_1TB'
    }
}
else {
    $env:CORPUS_ROOT
}

if ($Root -match '^[A-Za-z]:$') {
    $Root += [System.IO.Path]::DirectorySeparatorChar
}

$Root = [System.IO.Path]::GetFullPath($Root)

$ProjectDir = Join-Path -Path $Root -ChildPath (
    Join-Path -Path 'Rust-projects' -ChildPath 'pdfium-port-plan'
)

if ([string]::IsNullOrWhiteSpace($LibPdfium)) {
    if ($isWindowsPlatform) {
        $libraryCandidates = @(
            (Join-Path -Path $ProjectDir -ChildPath 'pdfium.dll'),
            (Join-Path -Path $ProjectDir -ChildPath 'libpdfium.dll')
        )
    }
    else {
        $libraryCandidates = @(
            (Join-Path -Path $ProjectDir -ChildPath 'libpdfium.so')
        )
    }

    $LibPdfium = $libraryCandidates |
        Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
        Select-Object -First 1

    # Retain a useful missing-file error even when no candidate exists.
    if ([string]::IsNullOrWhiteSpace($LibPdfium)) {
        $LibPdfium = $libraryCandidates[0]
    }
}
else {
    $LibPdfium = [System.IO.Path]::GetFullPath($LibPdfium)
}

$parsedScale = 0.0
$validScale = [double]::TryParse(
    $Scale,
    [System.Globalization.NumberStyles]::Float,
    [System.Globalization.CultureInfo]::InvariantCulture,
    [ref] $parsedScale
)

if (-not $validScale -or $parsedScale -le 0.0) {
    throw "Scale must be a positive number using '.' as the decimal separator; received '$Scale'."
}

$Corpora = @(
    (Join-Path -Path $Root -ChildPath 'Pol was right again'),
    (Join-Path -Path $Root -ChildPath 'to-sort'),
    (Join-Path -Path $ProjectDir -ChildPath 'renderer-corpus')
)

# Fail loudly if a corpus root is missing rather than silently sweeping less.
$missingCorpora = @(
    $Corpora | Where-Object {
        -not (Test-Path -LiteralPath $_ -PathType Container)
    }
)

foreach ($corpus in $missingCorpora) {
    Write-Stderr "MISSING corpus root: $corpus"
}

if ($missingCorpora.Count -ne 0) {
    Write-Stderr "aborting: fix the roots above (CORPUS_ROOT=$Root)"
    exit 1
}

if (-not (Test-Path -LiteralPath $LibPdfium -PathType Leaf)) {
    Write-Stderr "MISSING libpdfium: $LibPdfium"
    exit 1
}

$ScriptDir = $PSScriptRoot

Write-Stderr 'building release pdfium-diff...'
Push-Location -LiteralPath $ScriptDir
try {
    & cargo build --release

    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed with exit code $LASTEXITCODE."
    }
}
finally {
    Pop-Location
}

$binaryCandidates = if ($isWindowsPlatform) {
    @(
        (Join-Path -Path $ScriptDir -ChildPath 'target\release\pdfium-diff.exe'),
        (Join-Path -Path $ScriptDir -ChildPath 'target\release\pdfium-diff')
    )
}
else {
    @(
        (Join-Path -Path $ScriptDir -ChildPath 'target/release/pdfium-diff'),
        (Join-Path -Path $ScriptDir -ChildPath 'target/release/pdfium-diff.exe')
    )
}

$Bin = $binaryCandidates |
    Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
    Select-Object -First 1

if ([string]::IsNullOrWhiteSpace($Bin)) {
    Write-Stderr 'MISSING built executable. Checked:'
    foreach ($candidate in $binaryCandidates) {
        Write-Stderr "  $candidate"
    }
    exit 1
}

# The tool always writes ./pdfium-diff-out in its CWD, so run it from the
# selected work directory.
$null = New-Item -ItemType Directory -Path $WorkDir -Force
$WorkDir = (Resolve-Path -LiteralPath $WorkDir).Path

$outputDir = Join-Path -Path $WorkDir -ChildPath 'pdfium-diff-out'
$resultsPath = Join-Path -Path $outputDir -ChildPath 'results.csv'
$runTargets = $Corpora

if ($DryRun) {
    if (-not [string]::IsNullOrWhiteSpace($DryRunFile)) {
        $candidate = [System.IO.Path]::GetFullPath($DryRunFile)
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "DryRunFile does not exist: $candidate"
        }
        if ([System.IO.Path]::GetExtension($candidate) -ine '.pdf') {
            throw "DryRunFile must name a .pdf file: $candidate"
        }
        $dryRunTarget = $candidate
    }
    else {
        # This fixture is small, unencrypted, and is present in the checked-in
        # pdf.js corpus. Prefer it over arbitrary corpus files, which may be
        # encrypted or intentionally malformed regression inputs.
        $preferredTarget = Join-Path -Path $ProjectDir -ChildPath 'renderer-corpus\upstream-pdfjs-source\test\pdfs\basicapi.pdf'
        if (Test-Path -LiteralPath $preferredTarget -PathType Leaf) {
            $dryRunTarget = $preferredTarget
        }
        else {
            $dryRunTarget = $null
            foreach ($corpus in $Corpora) {
                $firstPdf = Get-ChildItem -LiteralPath $corpus -Recurse -File -Filter '*.pdf' |
                    Select-Object -First 1
                if ($null -ne $firstPdf) {
                    $dryRunTarget = $firstPdf.FullName
                    break
                }
            }
            if ($null -eq $dryRunTarget) {
                throw 'Dry run could not find a PDF in any configured corpus root.'
            }
        }
    }

    # pdfium-diff fixes its output directory relative to its CWD. A unique
    # directory prevents the smoke test from adding rows to the real sweep or
    # reusing stale rows from a prior smoke test.
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $WorkDir = Join-Path -Path $WorkDir -ChildPath "pdfium-diff-dry-run-$stamp"
    $null = New-Item -ItemType Directory -Path $WorkDir -Force
    $resultsPath = Join-Path -Path $WorkDir -ChildPath 'pdfium-diff-out\results.csv'
    $runTargets = @($dryRunTarget)
}

if ($DryRun) {
    Write-Stderr "dry run -> $resultsPath   lib=$LibPdfium scale=$Scale"
    Write-Stderr "test PDF: $dryRunTarget"
}
else {
    Write-Stderr "sweep -> $resultsPath   lib=$LibPdfium scale=$Scale"
    Write-Stderr 'roots:'
    foreach ($corpus in $Corpora) {
        Write-Stderr "  $corpus"
    }
}

$nativeExitCode = 1
Push-Location -LiteralPath $WorkDir
try {
    & $Bin $LibPdfium $Scale @runTargets
    $nativeExitCode = $LASTEXITCODE
}
finally {
    Pop-Location
}

exit $nativeExitCode
