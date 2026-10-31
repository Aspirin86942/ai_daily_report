[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$OutputDirectory)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $root) { throw 'Use a fresh isolated output directory' }
[IO.Directory]::CreateDirectory($root) | Out-Null
$utf8 = [Text.UTF8Encoding]::new($false)
$scriptPath = Join-Path $PSScriptRoot '../scripts/health_notify.ps1'
$stub = Join-Path $root 'stub.ps1'
$body = @'
param()
$out = $args[[Array]::IndexOf($args, '--output') + 1]
$config = $args[[Array]::IndexOf($args, '--config-dir') + 1]
$report = [ordered]@{
    status='healthy'; target_date='2026-10-26'; checks=@()
    task_names=@{daily='Scan Probe';health='Scan Probe Health'}
    log_dir=$config; generated_at='2026-10-27T01:00:00Z'
}
$json = $report | ConvertTo-Json -Depth 10
[IO.File]::WriteAllText($out,$json,[Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText((Join-Path $config 'health-2026-10-27.json'),$json,[Text.UTF8Encoding]::new($false))
exit 0
'@
[IO.File]::WriteAllText($stub,$body,$utf8)
$global:scanProbeStates = @{'Scan Probe'='Ready';'Scan Probe Health'='Running'}
function Get-ScheduledTask([string]$TaskName) {
    if ($global:scanProbeStates.ContainsKey($TaskName)) { [pscustomobject]@{ State=$global:scanProbeStates[$TaskName] } }
}
function Get-ScheduledTaskInfo([string]$TaskName) {
    [pscustomobject]@{LastTaskResult=0;NextRunTime=[datetime]'2026-10-28T09:00:00'}
}
$checks = 0
function Check([bool]$Pass,[string]$Message) { if (-not $Pass) { throw $Message }; $script:checks++ }
$summary = (& $scriptPath -Program $stub -ConfigDirectory $root -NoNotify | Out-String) | ConvertFrom-Json
Check ($LASTEXITCODE -eq 0) 'Daily and health tasks must pass'
Check (@($summary.anomalies).Count -eq 0) 'Unexpected disabled-task warning'
$readback = [IO.File]::ReadAllText($summary.health_file,$utf8) | ConvertFrom-Json
Check (@($readback.tasks).Count -eq 2) 'Only two task readbacks are required'
$global:scanProbeStates['Scan Probe'] = 'Disabled'
$summary = (& $scriptPath -Program $stub -ConfigDirectory $root -NoNotify | Out-String) | ConvertFrom-Json
Check ($LASTEXITCODE -eq 1) 'Disabled daily task must warn'
Check (@($summary.anomalies) -contains 'TASK_DISABLED:Scan Probe') 'Disabled daily task was not identified'
$global:scanProbeStates.Remove('Scan Probe')
$summary = (& $scriptPath -Program $stub -ConfigDirectory $root -NoNotify | Out-String) | ConvertFrom-Json
Check ($LASTEXITCODE -eq 1) 'Missing daily task must warn'
[ordered]@{status='passed';checks=$checks;production_tasks_changed=0} | ConvertTo-Json
exit 0
