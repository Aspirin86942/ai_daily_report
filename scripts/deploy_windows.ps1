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
$venvDir = Join-Path $repoRoot ".venv"
$venvPython = Join-Path $venvDir "Scripts\python.exe"
$exampleSettings = Join-Path $repoRoot "config\settings.example.yaml"
$genericSettings = Join-Path $repoRoot "config\settings.yaml"
$windowsSettings = Join-Path $repoRoot "config\settings.windows.yaml"
$lockFile = Join-Path $repoRoot "requirements.lock"
$requirementsFile = Join-Path $repoRoot "requirements.txt"

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

if (-not (Test-Path -LiteralPath $windowsSettings -PathType Leaf)) {
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

if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
    Invoke-CheckedCommand -Label "Create .venv" -FilePath $Python -Arguments @(
        "-m", "venv", $venvDir
    )
}

if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
    throw "Virtual environment Python was not created: $venvPython"
}

Invoke-CheckedCommand -Label "Validate Python 3.10+" `
    -FilePath $venvPython `
    -Arguments @(
        "-c",
        "import sys; raise SystemExit(0 if sys.version_info >= (3, 10) else 'Python 3.10+ is required')"
    )

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
