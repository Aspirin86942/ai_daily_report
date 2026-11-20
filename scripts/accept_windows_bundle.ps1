[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$BundleDirectory,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [string]$Harness = (Join-Path $PSScriptRoot 'bundle-acceptance.exe')
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release_common.ps1')
$bundle = (Resolve-Path -LiteralPath $BundleDirectory).Path
$runner = (Resolve-Path -LiteralPath $Harness).Path
$output = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $output) { throw 'Use a fresh isolated output directory' }
Assert-ReleaseLocalPath $output
if ($output.StartsWith($bundle.TrimEnd('\','/') + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Acceptance output must be outside the bundle' }
Assert-ReleaseLocalPath (Join-Path $bundle 'manifest.json')
$manifestHash = (Get-FileHash -LiteralPath (Join-Path $bundle 'manifest.json') -Algorithm SHA256).Hash.ToLowerInvariant()
$manifest = Test-RustReleaseBundle $bundle
$cases = [ordered]@{
    standalone_bundle_runs_reports_recovery_and_scheduling_without_python = 'bundle'
    standalone_personal_scan_publishes_once_without_collection = 'bundle-scan'
    standalone_model_service_switch_preserves_config_on_failure = 'bundle-model-services'
}
New-Item -ItemType Directory -Path $output -ErrorAction Stop | Out-Null
$previousBundle = $env:AI_DAILY_TEST_BUNDLE
$previousOutput = $env:AI_DAILY_ACCEPTANCE_OUTPUT
try {
    $env:AI_DAILY_TEST_BUNDLE = $bundle
    $env:AI_DAILY_ACCEPTANCE_OUTPUT = Join-Path $output 'bundle'
    $results = foreach ($name in $cases.Keys) {
        $log = Join-Path $output ($name + '.log')
        & $runner --ignored --exact $name --nocapture | Out-File -LiteralPath $log -Encoding utf8
        if ($LASTEXITCODE -ne 0) { throw "Bundle acceptance failed: $name ($log)" }
        # A Rust harness returns zero even when --exact matches no tests.
        $relative = $cases[$name] + '/acceptance.json'
        $path = Join-Path $output $relative
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing acceptance result: $name" }
        $result = Get-Content -LiteralPath $path -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($result.status -cne 'passed' -or $result.remote_model_calls -ne 0 -or $result.production_tasks_changed -ne 0 -or
            $result.local_http_requests -lt 1) { throw "Invalid acceptance result: $name" }
        if ($cases[$name] -eq 'bundle-model-services') {
            if ($result.missing_key_preserved_config -ne $true -or $result.readback_failure_restored_config -ne $true) {
                throw "Model service rollback evidence missing: $name"
            }
        } else {
            $observed = ([string]$result.bundle).Replace('\\?\UNC\','\\').Replace('\\?\','')
            $expected = $bundle.Replace('\\?\UNC\','\\').Replace('\\?\','')
            if ($observed -ine $expected -or -not @($result.checks).Count -or @($result.checks | Where-Object { $_.passed -ne $true }).Count) {
                throw "Bundle command evidence mismatch: $name"
            }
            if ($cases[$name] -eq 'bundle' -and ($result.python_environment_inherited -ne $false -or $result.real_daily_windows -ne 0)) {
                throw "Standalone runtime evidence mismatch: $name"
            }
            if ($cases[$name] -eq 'bundle-scan' -and $result.report_items -lt 1) { throw "Missing scan publication evidence: $name" }
        }
        [ordered]@{ test = $name; status = $result.status; result = $relative; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant(); local_http_requests = $result.local_http_requests }
    }
    Assert-ReleaseLocalPath (Join-Path $bundle 'manifest.json')
    if ((Get-FileHash -LiteralPath (Join-Path $bundle 'manifest.json') -Algorithm SHA256).Hash.ToLowerInvariant() -cne $manifestHash) { throw 'Release manifest changed during acceptance' }
    $null = Get-VerifiedReleaseHashes $bundle $manifest
    $summary = [ordered]@{ status = 'passed'; release_version = $manifest.release_version; manifest_sha256 = $manifestHash;
        remote_model_calls = 0; production_tasks_changed = 0; cases = @($results) }
    Write-ReleaseJson -Path (Join-Path $output 'acceptance.json') -Value $summary
    $summary | ConvertTo-Json -Depth 6
} finally {
    $env:AI_DAILY_TEST_BUNDLE = $previousBundle
    $env:AI_DAILY_ACCEPTANCE_OUTPUT = $previousOutput
}
