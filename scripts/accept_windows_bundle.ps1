[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$BundleDirectory,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [string]$Harness = (Join-Path $PSScriptRoot 'bundle-acceptance.exe')
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$bundle = (Resolve-Path -LiteralPath $BundleDirectory).Path
$runner = (Resolve-Path -LiteralPath $Harness).Path
$output = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $output) { throw 'Use a fresh isolated output directory' }
$previousBundle = $env:AI_DAILY_TEST_BUNDLE
$previousOutput = $env:AI_DAILY_ACCEPTANCE_OUTPUT
try {
    $env:AI_DAILY_TEST_BUNDLE = $bundle
    $env:AI_DAILY_ACCEPTANCE_OUTPUT = $output
    & $runner --ignored --exact standalone_bundle_runs_reports_recovery_and_scheduling_without_python --nocapture
    if ($LASTEXITCODE -ne 0) { throw 'Standalone bundle acceptance failed' }
    Get-Content -LiteralPath (Join-Path $output 'acceptance.json') -Raw -Encoding UTF8
} finally {
    $env:AI_DAILY_TEST_BUNDLE = $previousBundle
    $env:AI_DAILY_ACCEPTANCE_OUTPUT = $previousOutput
}
