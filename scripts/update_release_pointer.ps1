[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('Switch', 'Rollback')][string]$Mode,
    [Parameter(Mandatory = $true)][string]$PointerPath,
    [string]$ReleaseVersion = '',
    [string]$ScannerDatabasePath = '',
    [switch]$Apply
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release_common.ps1')
if (-not $Apply) { throw 'Release pointer changes require -Apply' }
$pointer = [IO.Path]::GetFullPath($PointerPath)
Assert-ReleaseLocalPath $pointer
function New-ReleaseReference([string]$Version, [string]$Database) {
    if ($Version -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$' -or $Version -in '.', '..') { throw 'Invalid release version' }
    if ($Database -notmatch '^shared/data/db/[A-Za-z0-9._/-]+\.sqlite3$' -or @($Database.Split('/') | Where-Object { $_ -in '.', '..', '' }).Count) { throw 'Scanner database must stay under shared/data/db' }
    return [ordered]@{ release_version = $Version; release_path = "releases/$Version"; scanner_db_path = $Database }
}
$existing = $null
$previousHash = $null
if (Test-Path -LiteralPath $pointer) {
    $previousHash = (Get-FileHash -LiteralPath $pointer -Algorithm SHA256).Hash
    $existing = Get-Content -LiteralPath $pointer -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($existing.schema_version -cne 'ai_daily_release_pointer_v1') { throw 'Invalid pointer schema' }
    foreach ($reference in @($existing.current, $existing.previous)) {
        if ($null -eq $reference) { continue }
        $expected = New-ReleaseReference $reference.release_version $reference.scanner_db_path
        if ($reference.release_path -cne $expected.release_path -or @($reference.PSObject.Properties).Count -ne 3) { throw 'Invalid pointer reference' }
    }
    if ($null -eq $existing.current) { throw 'Missing current release' }
}
if ($Mode -eq 'Switch') {
    $current = New-ReleaseReference $ReleaseVersion $ScannerDatabasePath
    $previous = if ($null -ne $existing) { $existing.current } else { $null }
    if ($null -ne $previous -and $previous.release_version -ceq $current.release_version -and $previous.scanner_db_path -ceq $current.scanner_db_path) {
        $existing | ConvertTo-Json -Depth 10
        return
    }
} else {
    if ($null -eq $existing -or $null -eq $existing.previous) { throw 'No previous release to restore' }
    $current = $existing.previous
    $previous = $existing.current
}
$value = [ordered]@{ schema_version = 'ai_daily_release_pointer_v1'; current = $current; previous = $previous }
Write-ReleaseJson -Path $pointer -Value $value -PreviousHash $previousHash
$value | ConvertTo-Json -Depth 10
