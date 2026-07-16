<#
.SYNOPSIS
Run the atomically selected installed release from any caller directory.
#>

[CmdletBinding()]
param(
    [string]$InstallRoot = $PSScriptRoot,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CommandArgs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not [IO.Path]::IsPathFullyQualified($InstallRoot)) {
    throw 'InstallRoot must be absolute'
}
$root = [IO.Path]::GetFullPath($InstallRoot)
if (-not (Test-Path -LiteralPath $root -PathType Container)) {
    throw "InstallRoot does not exist: $root"
}
$root = (Resolve-Path -LiteralPath $root).Path
$currentPath = Join-Path $root 'current.json'
if (-not (Test-Path -LiteralPath $currentPath -PathType Leaf)) {
    throw "Missing current.json: $currentPath"
}
try {
    $current = Get-Content -LiteralPath $currentPath -Raw -Encoding UTF8 |
        ConvertFrom-Json -Depth 10
}
catch {
    throw 'current.json is not valid UTF-8 JSON'
}
foreach ($property in @('schema_version', 'release_version', 'release_path')) {
    if ($null -eq $current.PSObject.Properties[$property]) {
        throw "current.json is missing $property"
    }
}
$version = [string]$current.release_version
if (
    $current.schema_version -cne 'ai_daily_current_v1' -or
    $version -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$' -or
    [string]$current.release_path -cne "releases/$version"
) {
    throw 'current.json failed schema validation'
}

$releasesRoot = [IO.Path]::GetFullPath((Join-Path $root 'releases'))
$release = [IO.Path]::GetFullPath((Join-Path $root (
    ([string]$current.release_path).Replace('/', '\')
)))
$releasePrefix = $releasesRoot.TrimEnd('\') + '\'
if (-not $release.StartsWith($releasePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'current.json release path escaped releases/'
}
if (-not (Test-Path -LiteralPath $release -PathType Container)) {
    throw "Selected release does not exist: $release"
}
$release = (Resolve-Path -LiteralPath $release).Path
if (-not $release.StartsWith($releasePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Selected release resolves outside releases/'
}
$releaseItem = Get-Item -LiteralPath $release -Force
if ($releaseItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw 'Selected release must not be a reparse point'
}

$shared = [IO.Path]::GetFullPath((Join-Path $root 'shared'))
$runtimePaths = [ordered]@{
    DAILY_REPORT_INSTALL_ROOT = $root
    DAILY_REPORT_CONFIG_DIR = Join-Path $shared 'config'
    DAILY_REPORT_DATA_DIR = Join-Path $shared 'data'
    DAILY_REPORT_REPORTS_DIR = Join-Path $shared 'data\reports'
    DAILY_REPORT_DB_DIR = Join-Path $shared 'data\db'
    DAILY_REPORT_LOG_DIR = Join-Path $shared 'logs'
}
$sharedPrefix = $shared.TrimEnd('\') + '\'
$runtimeNames = @($runtimePaths.Keys)
foreach ($name in $runtimeNames) {
    $path = [IO.Path]::GetFullPath([string]$runtimePaths[$name])
    if (-not (Test-Path -LiteralPath $path -PathType Container)) {
        throw "Installed runtime directory is missing: $name"
    }
    $path = (Resolve-Path -LiteralPath $path).Path
    if (
        $name -ne 'DAILY_REPORT_INSTALL_ROOT' -and
        -not $path.StartsWith($sharedPrefix, [StringComparison]::OrdinalIgnoreCase)
    ) {
        throw "Installed runtime directory escaped shared/: $name"
    }
    $runtimePaths[$name] = $path
}

$python = Join-Path $release '.venv\Scripts\python.exe'
$main = Join-Path $release 'main.py'
if (-not (Test-Path -LiteralPath $python -PathType Leaf)) {
    throw "Selected release virtual environment is missing: $python"
}
if (-not (Test-Path -LiteralPath $main -PathType Leaf)) {
    throw "Selected release main.py is missing"
}

$saved = @{}
foreach ($name in $runtimeNames + @('PYTHONDONTWRITEBYTECODE')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$exitCode = 1
try {
    foreach ($name in $runtimeNames) {
        [Environment]::SetEnvironmentVariable(
            $name,
            [string]$runtimePaths[$name],
            'Process'
        )
    }
    $env:PYTHONDONTWRITEBYTECODE = '1'
    Push-Location $release
    try {
        & $python $main @CommandArgs
        $exitCode = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }
}
finally {
    foreach ($name in $saved.Keys) {
        [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
    }
}
exit $exitCode
