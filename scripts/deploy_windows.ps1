<#
.SYNOPSIS
Prepare a source checkout for local Windows execution.

.DESCRIPTION
Creates or reuses an isolated .venv, installs locked dependencies when
requirements.lock exists, builds the Rust release workspace, and runs the
strict application doctor. The script never accepts or persists API keys.

.EXAMPLE
.\scripts\deploy_windows.ps1

#>

[CmdletBinding()]
param(
    [string]$Python = "python"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$utf8 = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = $utf8
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8

$scriptDir = Split-Path -Parent $PSCommandPath
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $scriptDir ".."))
$pythonVersionFile = Join-Path $repoRoot ".python-version"
$venvDir = Join-Path $repoRoot ".venv"
$venvPython = Join-Path $venvDir "Scripts\python.exe"
$exampleSettings = Join-Path $repoRoot "config\settings.example.yaml"
$genericSettings = Join-Path $repoRoot "config\settings.yaml"
$windowsSettings = Join-Path $repoRoot "config\settings.windows.yaml"
$lockFile = Join-Path $repoRoot "requirements.lock"
$requirementsFile = Join-Path $repoRoot "requirements.txt"
$installedMode = -not [string]::IsNullOrWhiteSpace(
    $env:DAILY_REPORT_INSTALL_ROOT
)
$env:PYTHONDONTWRITEBYTECODE = "1"

if (-not (Test-Path -LiteralPath $pythonVersionFile -PathType Leaf)) {
    throw "Missing Python version file: $pythonVersionFile"
}
$expectedPythonVersion = (
    Get-Content -LiteralPath $pythonVersionFile -Raw
).Trim()
if ($expectedPythonVersion -notmatch '^\d+\.\d+\.\d+$') {
    throw "Invalid Python version in $pythonVersionFile"
}

function Invoke-CheckedCommand {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [Parameter(Mandatory = $true)]
        [string]$FilePath,
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    Write-Host "==> $Label" -ForegroundColor Cyan
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Label failed with exit code $LASTEXITCODE"
    }
}

function Assert-CPythonVersion {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [Parameter(Mandatory = $true)]
        [string]$FilePath
    )

    $repair = (
        "Install CPython $expectedPythonVersion and pass its executable " +
        "with -Python. Existing .venv directories are not removed automatically."
    )
    try {
        $probeOutput = @(
            & $FilePath "-c" (
                "import platform; " +
                "print(platform.python_implementation() + ' ' + " +
                "platform.python_version())"
            ) 2>&1
        )
        $probeExitCode = $LASTEXITCODE
    }
    catch {
        throw (
            "${Label} could not be inspected. " +
            "Expected: CPython $expectedPythonVersion. " +
            "Actual: unavailable. Repair: $repair"
        )
    }

    $actualRuntime = ($probeOutput -join "`n").Trim()
    if ($probeExitCode -ne 0) {
        throw (
            "${Label} version probe failed with exit code $probeExitCode. " +
            "Expected: CPython $expectedPythonVersion. " +
            "Actual: $actualRuntime. Repair: $repair"
        )
    }
    if ($actualRuntime -cne "CPython $expectedPythonVersion") {
        throw (
            "${Label} version mismatch. " +
            "Expected: CPython $expectedPythonVersion. " +
            "Actual: $actualRuntime. Repair: $repair"
        )
    }

    Write-Host "==> ${Label}: $actualRuntime" -ForegroundColor Cyan
}

Assert-CPythonVersion -Label "Creator Python" -FilePath $Python
$venvPathExisted = Test-Path -LiteralPath $venvDir
$venvExisted = Test-Path -LiteralPath $venvPython -PathType Leaf
if ($venvPathExisted -and -not $venvExisted) {
    throw (
        "Existing .venv is incomplete. " +
        "Expected: CPython $expectedPythonVersion at $venvPython. " +
        "Actual: Python executable is missing. " +
        "Repair: inspect or recreate .venv manually; it was not changed."
    )
}
if ($venvExisted) {
    Assert-CPythonVersion -Label "Existing .venv Python" -FilePath $venvPython
}

if ($installedMode) {
    $externalConfig = $env:DAILY_REPORT_CONFIG_DIR
    if ([string]::IsNullOrWhiteSpace($externalConfig)) {
        throw "Installed mode requires DAILY_REPORT_CONFIG_DIR"
    }
    $externalWindowsSettings = Join-Path $externalConfig "settings.windows.yaml"
    $externalGenericSettings = Join-Path $externalConfig "settings.yaml"
    if (
        -not (Test-Path -LiteralPath $externalWindowsSettings -PathType Leaf) -and
        -not (Test-Path -LiteralPath $externalGenericSettings -PathType Leaf)
    ) {
        throw "Installed mode requires an existing shared settings file"
    }
    Write-Host "Using existing shared configuration; no settings were copied."
} elseif (-not (Test-Path -LiteralPath $windowsSettings -PathType Leaf)) {
    if (Test-Path -LiteralPath $genericSettings -PathType Leaf) {
        Write-Host "Using existing config\settings.yaml; no Windows settings file was created."
    } elseif (-not (Test-Path -LiteralPath $exampleSettings -PathType Leaf)) {
        throw "Missing configuration template: $exampleSettings"
    } else {
        Copy-Item -LiteralPath $exampleSettings -Destination $windowsSettings
        Write-Host "Created config\settings.windows.yaml; set paths.work_dir before continuing." -ForegroundColor Yellow
    }
} else {
    Write-Host "Keeping existing config\settings.windows.yaml."
}

if (-not $venvPathExisted) {
    Invoke-CheckedCommand -Label "Create .venv" -FilePath $Python -Arguments @(
        "-m", "venv", $venvDir
    )
}

if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
    throw "Virtual environment Python was not created: $venvPython"
}
if (-not $venvExisted) {
    Assert-CPythonVersion -Label "Created .venv Python" -FilePath $venvPython
}

$dependencyFile = $requirementsFile
if (Test-Path -LiteralPath $lockFile -PathType Leaf) {
    $dependencyFile = $lockFile
}
if (-not (Test-Path -LiteralPath $dependencyFile -PathType Leaf)) {
    throw "Missing dependency file: $dependencyFile"
}

Invoke-CheckedCommand -Label "Install Python dependencies from $([System.IO.Path]::GetFileName($dependencyFile))" `
    -FilePath $venvPython `
    -Arguments @("-m", "pip", "install", "--requirement", $dependencyFile)
Invoke-CheckedCommand -Label "Validate Python dependencies" `
    -FilePath $venvPython `
    -Arguments @("-m", "pip", "check")

if ($installedMode) {
    foreach ($binary in @(
        (Join-Path $repoRoot "rust\target\release\ai-daily-scanner.exe"),
        (Join-Path $repoRoot "rust\target\release\ai-daily-office-parser.exe")
    )) {
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw "Installed package is missing verified Rust binary: $binary"
        }
    }
} else {
    if ($null -eq (Get-Command "cargo" -ErrorAction SilentlyContinue)) {
        throw "cargo is required for the Windows production deployment."
    }
    $manifest = Join-Path $repoRoot "rust\Cargo.toml"
    Invoke-CheckedCommand -Label "Build Rust workspace" `
        -FilePath "cargo" `
        -Arguments @(
            "build",
            "--manifest-path", $manifest,
            "--workspace",
            "--release",
            "--locked"
        )
}

Push-Location $repoRoot
try {
    Invoke-CheckedCommand -Label "Run deployment doctor" `
        -FilePath $venvPython `
        -Arguments @("main.py", "doctor", "--strict")
}
finally {
    Pop-Location
}

Write-Host "Deployment checks completed. This script accepted no API key and persisted none." -ForegroundColor Green
