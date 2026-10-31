[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Program,
    [Parameter(Mandatory=$true)][string]$OutputDirectory
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
. (Join-Path $repo 'scripts/daily_task_common.ps1')
$root = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $root) { throw 'Use a fresh isolated output directory' }
[IO.Directory]::CreateDirectory((Join-Path $root 'config')) | Out-Null
[IO.Directory]::CreateDirectory((Join-Path $root 'work')) | Out-Null
$settings = @{
    paths=@{work_dir=(Join-Path $root 'work');data_dir=(Join-Path $root 'data');db_dir=(Join-Path $root 'data/db');reports_dir=(Join-Path $root 'reports')}
    report=@{profile='personal'}
    archive=@{allow_work_dir_for_model=$true}
    schedule=@{task_name='Isolated Personal';execution_limit_minutes=6}
    llm=@{context_window_tokens=128000;verified_max_output_tokens=8192}
}
$settingsPath = Join-Path $root 'config/settings.yaml'
$utf8 = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText($settingsPath, ($settings | ConvertTo-Json -Depth 5), $utf8)
$script:definitions = @{}
function Get-ScheduledTask {
    param($TaskPath)
    foreach ($name in $script:definitions.Keys) { [pscustomobject]@{TaskName=$name;State='Ready'} }
}
function Export-ScheduledTask { param($TaskName,$TaskPath) $script:definitions[$TaskName] }
function Register-ScheduledTask { param($TaskName,$TaskPath,$Xml,[switch]$Force) $script:definitions[$TaskName]=$Xml }
function Disable-ScheduledTask { param($TaskName,$TaskPath) }
function Enable-ScheduledTask {
    param($TaskName,$TaskPath)
    $script:definitions[$TaskName]=$script:definitions[$TaskName].Replace('<Enabled>false</Enabled>','<Enabled>true</Enabled>')
}
$plans=@{}
foreach ($kind in @('daily','health')) {
    $plan=Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory (Join-Path $root 'config') -Kind $kind
    if ($plan.report_profile -ne 'personal') { throw 'Wrong profile' }
    $plan.credential_present=$true
    $plan.model_check_verified=$true
    Assert-ReportTaskContext -Plan $plan
    Install-ReportDailyTask -Plan $plan -Apply | Out-Null
    if (@(Get-ReportTaskDifferences -Plan $plan -Xml $script:definitions[$plan.task_name]).Count) { throw 'Readback mismatch' }
    $plans[$kind]=$plan
    [IO.File]::WriteAllText((Join-Path $root "$kind.xml"),$plan.xml,$utf8)
}
foreach ($kind in $plans.Keys) { Assert-ReportTaskContext -Plan $plans[$kind] }
if (-not $plans.daily.xml.Contains('PT6M')) { throw 'Wrong external deadline' }
$settings.report.profile='evidence'
[IO.File]::WriteAllText($settingsPath, ($settings | ConvertTo-Json -Depth 5), $utf8)
$rejected=$false
try { Assert-ReportTaskContext -Plan $plans.daily } catch { $rejected=$_.Exception.Message.StartsWith('TASK_EXECUTION_CONTEXT_MISMATCH') }
if (-not $rejected) { throw 'Mixed profile was accepted' }
$settings.report.profile='personal'
$settings.archive.allow_work_dir_for_model=$false
[IO.File]::WriteAllText($settingsPath, ($settings | ConvertTo-Json -Depth 5), $utf8)
$rejected=$false
try { Assert-ReportTaskContext -Plan $plans.daily } catch { $rejected=$_.Exception.Message.StartsWith('TASK_EXECUTION_CONTEXT_MISMATCH') }
if (-not $rejected) { throw 'Mixed model permissions were accepted' }
@{status='passed';real_tasks_changed=0;task_definitions=2;mixed_profile_rejected=$true;mixed_permissions_rejected=$true} |
    ConvertTo-Json | Tee-Object -FilePath (Join-Path $root 'result.json')
