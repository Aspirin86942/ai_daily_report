<#
.SYNOPSIS
Install a verified Windows package side by side and atomically select it.

.DESCRIPTION
Run this trusted bootstrap from a source checkout or previously verified
installation. It never reads from or writes to the developer checkout config.
Shared settings must already exist below <install-root>\shared\config.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ArchivePath,
    [Parameter(Mandatory = $true)]
    [string]$InstallRoot,
    [string]$Python = 'python',
    [string]$VerifyScript = (Join-Path $PSScriptRoot 'verify_windows_package.ps1')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$utf8 = [Text.UTF8Encoding]::new($false)

if (-not [IO.Path]::IsPathFullyQualified($InstallRoot)) {
    throw 'InstallRoot must be absolute'
}
$root = [IO.Path]::GetFullPath($InstallRoot)
$releases = Join-Path $root 'releases'
$shared = Join-Path $root 'shared'
$configDir = Join-Path $shared 'config'
$dataDir = Join-Path $shared 'data'
$reportsDir = Join-Path $dataDir 'reports'
$dbDir = Join-Path $dataDir 'db'
$logDir = Join-Path $shared 'logs'
$stagingParent = Join-Path $root '.staging'
foreach ($directory in @(
    $root, $releases, $configDir, $dataDir, $reportsDir, $dbDir, $logDir, $stagingParent
)) {
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
}
$root = (Resolve-Path -LiteralPath $root).Path
$rootPrefix = $root.TrimEnd('\') + '\'

if (
    -not (Test-Path -LiteralPath (Join-Path $configDir 'settings.windows.yaml') -PathType Leaf) -and
    -not (Test-Path -LiteralPath (Join-Path $configDir 'settings.yaml') -PathType Leaf)
) {
    throw 'Shared config must already contain settings.windows.yaml or settings.yaml'
}
if (-not (Test-Path -LiteralPath $VerifyScript -PathType Leaf)) {
    throw "Trusted verifier does not exist: $VerifyScript"
}

function Write-AtomicUtf8Json {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)]$Value
    )
    $temporary = "$LiteralPath.$([guid]::NewGuid().ToString('N')).tmp"
    [IO.File]::WriteAllText(
        $temporary,
        (($Value | ConvertTo-Json -Depth 10) + "`n"),
        $utf8
    )
    if (Test-Path -LiteralPath $LiteralPath -PathType Leaf) {
        $backup = "$LiteralPath.$([guid]::NewGuid().ToString('N')).bak"
        try {
            [IO.File]::Replace($temporary, $LiteralPath, $backup, $true)
        }
        finally {
            if (Test-Path -LiteralPath $backup) {
                Remove-Item -LiteralPath $backup -Force
            }
        }
    }
    else {
        [IO.File]::Move($temporary, $LiteralPath)
    }
}

function Copy-AtomicFile {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )
    $temporary = "$Destination.$([guid]::NewGuid().ToString('N')).tmp"
    Copy-Item -LiteralPath $Source -Destination $temporary
    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        $backup = "$Destination.$([guid]::NewGuid().ToString('N')).bak"
        try {
            [IO.File]::Replace($temporary, $Destination, $backup, $true)
        }
        finally {
            if (Test-Path -LiteralPath $backup) {
                Remove-Item -LiteralPath $backup -Force
            }
        }
    }
    else {
        [IO.File]::Move($temporary, $Destination)
    }
}

function Read-CurrentPointer {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    if (-not (Test-Path -LiteralPath $LiteralPath -PathType Leaf)) {
        return $null
    }
    try {
        $pointer = Get-Content -LiteralPath $LiteralPath -Raw -Encoding UTF8 |
            ConvertFrom-Json -Depth 10
    }
    catch {
        throw 'Existing current.json is invalid'
    }
    if (
        $pointer.schema_version -ne 'ai_daily_current_v1' -or
        [string]$pointer.release_version -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$' -or
        [string]$pointer.release_path -cne "releases/$($pointer.release_version)"
    ) {
        throw 'Existing current.json failed schema validation'
    }
    return $pointer
}

$stage = Join-Path $stagingParent ([guid]::NewGuid().ToString('N'))
$releaseDir = $null
$newReleaseCreated = $false
$environmentNames = @(
    'DAILY_REPORT_INSTALL_ROOT',
    'DAILY_REPORT_CONFIG_DIR',
    'DAILY_REPORT_DATA_DIR',
    'DAILY_REPORT_REPORTS_DIR',
    'DAILY_REPORT_DB_DIR',
    'DAILY_REPORT_LOG_DIR',
    'PYTHONDONTWRITEBYTECODE'
)
$savedEnvironment = @{}
foreach ($name in $environmentNames) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

try {
    $verified = & $VerifyScript -ArchivePath $ArchivePath -ExtractTo $stage
    if ($null -eq $verified -or $verified.status -ne 'ok') {
        throw 'Trusted package verification did not return success'
    }
    $version = [string]$verified.release_version
    $releaseDir = [IO.Path]::GetFullPath((Join-Path $releases $version))
    if (-not $releaseDir.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Release directory escaped InstallRoot'
    }
    if (Test-Path -LiteralPath $releaseDir) {
        throw "Release version already exists: $version"
    }
    $packageRoot = [string]$verified.package_root
    Move-Item -LiteralPath $packageRoot -Destination $releaseDir
    $newReleaseCreated = $true

    $env:DAILY_REPORT_INSTALL_ROOT = $root
    $env:DAILY_REPORT_CONFIG_DIR = (Resolve-Path -LiteralPath $configDir).Path
    $env:DAILY_REPORT_DATA_DIR = (Resolve-Path -LiteralPath $dataDir).Path
    $env:DAILY_REPORT_REPORTS_DIR = (Resolve-Path -LiteralPath $reportsDir).Path
    $env:DAILY_REPORT_DB_DIR = (Resolve-Path -LiteralPath $dbDir).Path
    $env:DAILY_REPORT_LOG_DIR = (Resolve-Path -LiteralPath $logDir).Path
    $env:PYTHONDONTWRITEBYTECODE = '1'

    $deploy = Join-Path $releaseDir 'scripts\deploy_windows.ps1'
    & pwsh -NoProfile -File $deploy -Python $Python
    if ($LASTEXITCODE -ne 0) {
        throw "Installed release doctor failed with exit code $LASTEXITCODE"
    }
    $releaseVerifier = Join-Path $releaseDir 'scripts\verify_windows_package.ps1'
    $recheck = & $releaseVerifier -PackageDirectory $releaseDir
    if ($null -eq $recheck -or $recheck.status -ne 'ok') {
        throw 'Installed release failed post-deploy integrity validation'
    }

    Copy-AtomicFile `
        -Source (Join-Path $releaseDir 'scripts\run_current_release.ps1') `
        -Destination (Join-Path $root 'run_current_release.ps1')
    Copy-AtomicFile `
        -Source (Join-Path $releaseDir 'scripts\rollback_windows_release.ps1') `
        -Destination (Join-Path $root 'rollback_windows_release.ps1')

    $currentPath = Join-Path $root 'current.json'
    $previous = Read-CurrentPointer -LiteralPath $currentPath
    $pointer = [ordered]@{
        schema_version = 'ai_daily_current_v1'
        release_version = $version
        release_path = "releases/$version"
        previous_release_version = if ($null -eq $previous) {
            $null
        } else {
            [string]$previous.release_version
        }
    }
    Write-AtomicUtf8Json -LiteralPath $currentPath -Value $pointer
    $newReleaseCreated = $false

    [pscustomobject]@{
        status = 'ok'
        install_root = $root
        release_version = $version
        release_dir = $releaseDir
        previous_release_version = $pointer.previous_release_version
    }
}
finally {
    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable(
            $name,
            $savedEnvironment[$name],
            'Process'
        )
    }
    if (Test-Path -LiteralPath $stage) {
        $resolvedStage = [IO.Path]::GetFullPath($stage)
        if (-not $resolvedStage.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Refusing to remove install staging outside InstallRoot'
        }
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
    if ($newReleaseCreated -and $releaseDir -and (Test-Path -LiteralPath $releaseDir)) {
        $resolvedRelease = [IO.Path]::GetFullPath($releaseDir)
        if (-not $resolvedRelease.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Refusing to remove failed release outside InstallRoot'
        }
        Remove-Item -LiteralPath $resolvedRelease -Recurse -Force
    }
}
