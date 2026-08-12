[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('Switch', 'Rollback')][string]$Mode,
    [Parameter(Mandatory = $true)][string]$PointerPath,
    [string]$ReleaseVersion = '',
    [string]$ScannerDatabasePath = '',
    [switch]$Apply,
    [string]$Python = (Join-Path $PSScriptRoot '..\.venv\Scripts\python.exe')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $Apply) { throw 'Release pointer changes require -Apply' }
$tool = Join-Path $PSScriptRoot 'windows_release.py'
if ($Mode -eq 'Switch') {
    if (-not $ReleaseVersion -or -not $ScannerDatabasePath) {
        throw 'Switch requires ReleaseVersion and ScannerDatabasePath'
    }
    & $Python $tool pointer-switch --pointer $PointerPath `
        --release-version $ReleaseVersion --scanner-db-path $ScannerDatabasePath --apply
}
else {
    & $Python $tool pointer-rollback --pointer $PointerPath --apply
}
if ($LASTEXITCODE -ne 0) { throw 'Release pointer update failed' }
