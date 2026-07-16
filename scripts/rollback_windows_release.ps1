<#
.SYNOPSIS
Validate the previous installed release and atomically select it.
#>

[CmdletBinding()]
param(
    [string]$InstallRoot = $PSScriptRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$utf8 = [Text.UTF8Encoding]::new($false)

if (-not [IO.Path]::IsPathFullyQualified($InstallRoot)) {
    throw 'InstallRoot must be absolute'
}
$root = [IO.Path]::GetFullPath($InstallRoot)
$currentPath = Join-Path $root 'current.json'
if (-not (Test-Path -LiteralPath $currentPath -PathType Leaf)) {
    throw 'Rollback requires current.json'
}
try {
    $current = Get-Content -LiteralPath $currentPath -Raw -Encoding UTF8 |
        ConvertFrom-Json -Depth 10
}
catch {
    throw 'current.json is not valid UTF-8 JSON'
}
if (
    $current.schema_version -cne 'ai_daily_current_v1' -or
    [string]$current.release_version -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$' -or
    [string]$current.release_path -cne "releases/$($current.release_version)" -or
    [string]$current.previous_release_version -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$'
) {
    throw 'current.json has no valid previous release'
}
$currentVersion = [string]$current.release_version
$previousVersion = [string]$current.previous_release_version
if ($previousVersion -ceq $currentVersion) {
    throw 'Previous release must differ from current release'
}
$releases = [IO.Path]::GetFullPath((Join-Path $root 'releases'))
$releasesPrefix = $releases.TrimEnd('\') + '\'
$currentRelease = [IO.Path]::GetFullPath((Join-Path $releases $currentVersion))
$previousRelease = [IO.Path]::GetFullPath((Join-Path $releases $previousVersion))
foreach ($release in @($currentRelease, $previousRelease)) {
    if (
        -not $release.StartsWith($releasesPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath $release -PathType Container)
    ) {
        throw 'Rollback release path escaped or is missing'
    }
    $item = Get-Item -LiteralPath $release -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'Rollback release must not be a reparse point'
    }
}

$trustedVerifier = Join-Path $currentRelease 'scripts\verify_windows_package.ps1'
if (-not (Test-Path -LiteralPath $trustedVerifier -PathType Leaf)) {
    throw 'Current verified release has no trusted verifier'
}
$verified = & $trustedVerifier -PackageDirectory $previousRelease
if ($null -eq $verified -or $verified.status -ne 'ok') {
    throw 'Previous release failed integrity validation'
}
if ([string]$verified.release_version -cne $previousVersion) {
    throw 'Previous release directory and manifest version differ'
}

$shared = Join-Path $root 'shared'
$runtimePaths = [ordered]@{
    DAILY_REPORT_INSTALL_ROOT = $root
    DAILY_REPORT_CONFIG_DIR = Join-Path $shared 'config'
    DAILY_REPORT_DATA_DIR = Join-Path $shared 'data'
    DAILY_REPORT_REPORTS_DIR = Join-Path $shared 'data\reports'
    DAILY_REPORT_DB_DIR = Join-Path $shared 'data\db'
    DAILY_REPORT_LOG_DIR = Join-Path $shared 'logs'
}
$sharedPrefix = [IO.Path]::GetFullPath($shared).TrimEnd('\') + '\'
$runtimeNames = @($runtimePaths.Keys)
foreach ($name in $runtimeNames) {
    $path = [IO.Path]::GetFullPath([string]$runtimePaths[$name])
    if (-not (Test-Path -LiteralPath $path -PathType Container)) {
        throw "Rollback shared directory is missing: $name"
    }
    $path = (Resolve-Path -LiteralPath $path).Path
    if (
        $name -ne 'DAILY_REPORT_INSTALL_ROOT' -and
        -not $path.StartsWith($sharedPrefix, [StringComparison]::OrdinalIgnoreCase)
    ) {
        throw "Rollback shared directory escaped shared/: $name"
    }
    $runtimePaths[$name] = $path
}

$python = Join-Path $previousRelease '.venv\Scripts\python.exe'
if (-not (Test-Path -LiteralPath $python -PathType Leaf)) {
    throw 'Previous release virtual environment is missing'
}
$saved = @{}
foreach ($name in $runtimeNames + @('PYTHONDONTWRITEBYTECODE')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    foreach ($name in $runtimeNames) {
        [Environment]::SetEnvironmentVariable(
            $name,
            [string]$runtimePaths[$name],
            'Process'
        )
    }
    $env:PYTHONDONTWRITEBYTECODE = '1'
    Push-Location $previousRelease
    try {
        & $python (Join-Path $previousRelease 'main.py') doctor --strict
        if ($LASTEXITCODE -ne 0) {
            throw 'Previous release strict doctor failed'
        }
    }
    finally { Pop-Location }
}
finally {
    foreach ($name in $saved.Keys) {
        [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
    }
}

$newPointer = [ordered]@{
    schema_version = 'ai_daily_current_v1'
    release_version = $previousVersion
    release_path = "releases/$previousVersion"
    previous_release_version = $currentVersion
}
$temporary = "$currentPath.$([guid]::NewGuid().ToString('N')).tmp"
[IO.File]::WriteAllText(
    $temporary,
    (($newPointer | ConvertTo-Json -Depth 10) + "`n"),
    $utf8
)
$backup = "$currentPath.$([guid]::NewGuid().ToString('N')).bak"
try {
    [IO.File]::Replace($temporary, $currentPath, $backup, $true)
}
finally {
    if (Test-Path -LiteralPath $backup) {
        Remove-Item -LiteralPath $backup -Force
    }
}

[pscustomobject]@{
    status = 'ok'
    release_version = $previousVersion
    previous_release_version = $currentVersion
}
