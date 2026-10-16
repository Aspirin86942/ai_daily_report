[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$SourceDatabase,
    [Parameter(Mandatory = $true)][string]$ArchiveDirectory,
    [string]$Program = (Join-Path $PSScriptRoot '..\ai-daily-report.exe')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
& $Program archive-scanner --source $SourceDatabase --archive-dir $ArchiveDirectory | Out-String
if ($LASTEXITCODE -ne 0) { throw 'Scanner database archival failed' }
