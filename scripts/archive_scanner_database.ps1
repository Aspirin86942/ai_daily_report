[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$SourceDatabase,
    [Parameter(Mandatory = $true)][string]$ArchiveDirectory,
    [string]$Python = (Join-Path $PSScriptRoot '..\.venv\Scripts\python.exe')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tool = Join-Path $PSScriptRoot 'windows_release.py'
& $Python $tool archive-db --source $SourceDatabase --archive-dir $ArchiveDirectory
if ($LASTEXITCODE -ne 0) { throw 'Scanner database archival failed' }
