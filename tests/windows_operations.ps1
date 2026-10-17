[CmdletBinding()]
param([string]$Program = (Join-Path $PSScriptRoot '../rust/target/debug/ai-daily-report.exe'), [string]$OutputDirectory = (Join-Path $PSScriptRoot '../.artifacts/rust-migration/windows-operations'))
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
. (Join-Path $repo 'scripts/daily_task_common.ps1')
$root = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $root) { throw 'Use a fresh isolated output directory' }
[IO.Directory]::CreateDirectory((Join-Path $root 'config')) | Out-Null
[IO.Directory]::CreateDirectory((Join-Path $root 'work')) | Out-Null
$work = (Join-Path $root 'work').Replace('\','/')
$data = (Join-Path $root 'data').Replace('\','/')
$settings = "paths:`n  work_dir: '$work'`n  data_dir: '$data'`n  db_dir: '$data/db'`n  reports_dir: '$data/reports'`nschedule:`n  task_name: 'Rust isolated contract test'`nllm:`n  context_window_tokens: 100000`n  verified_max_output_tokens: 8192`n"
[IO.File]::WriteAllText((Join-Path $root 'config/settings.yaml'), $settings, [Text.UTF8Encoding]::new($false))
$plan = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory (Join-Path $root 'config')
function Assert-Test([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
# All scheduler calls are local function doubles. No task is registered on Windows.
$script:task = $null
$script:taskXml = ''
$script:registered = 0
$script:disabled = 0
$script:badReadback = $false
function Get-ScheduledTask { param($TaskPath) if ($null -ne $script:task) { $script:task } }
function Export-ScheduledTask { param($TaskName,$TaskPath) $script:taskXml }
function Register-ScheduledTask { param($TaskName,$TaskPath,$Xml,[switch]$Force)
    $script:registered++; $script:task = [pscustomobject]@{ TaskName = $TaskName }; $script:taskXml = $Xml
    if ($script:badReadback) { $script:taskXml = $Xml.Replace('IgnoreNew','Parallel') }
}
function Disable-ScheduledTask { param($TaskName,$TaskPath) $script:disabled++ }
function Unregister-ScheduledTask { param($TaskName,$TaskPath,[switch]$Confirm) $script:task = $null }
$preview = Install-ReportDailyTask -Plan $plan
Assert-Test (-not $preview.Applied -and $script:registered -eq 0) 'Preview registered a task'
$plan.credential_present = $false
$blocked = $false
try { Install-ReportDailyTask -Plan $plan -Apply | Out-Null } catch { $blocked = $_.Exception.Message -eq 'TASK_CREDENTIAL_NOT_VISIBLE' }
Assert-Test $blocked 'Missing credential was not blocked'
$plan.credential_present = $true
$plan.model_check_verified = $true
$result = Install-ReportDailyTask -Plan $plan -Apply
Assert-Test ($result.Action -eq 'registered' -and $script:registered -eq 1) 'Registration contract failed'
$result = Install-ReportDailyTask -Plan $plan -Apply
Assert-Test ($result.Action -eq 'unchanged' -and $script:registered -eq 1) 'Repeated registration was not idempotent'
$script:taskXml = $plan.xml.Replace($plan.owner_marker, 'other-owner')
$blocked = $false
try { Remove-ReportDailyTask -Plan $plan -Apply | Out-Null } catch { $blocked = $_.Exception.Message -eq 'TASK_NAME_BELONGS_TO_ANOTHER_PROJECT' }
Assert-Test $blocked 'Foreign task was accepted'
$script:taskXml = $plan.xml.Replace('IgnoreNew','Parallel'); $script:badReadback = $true
$blocked = $false
try { Install-ReportDailyTask -Plan $plan -Apply | Out-Null } catch { $blocked = $_.Exception.Message -eq 'TASK_READBACK_MISMATCH_DISABLED' }
Assert-Test ($blocked -and $script:disabled -eq 1) 'Mismatched readback was not disabled'
$script:taskXml = $plan.xml; Remove-ReportDailyTask -Plan $plan -Apply | Out-Null
Assert-Test ($null -eq $script:task) 'Owned task removal failed'
$pointer = Join-Path $root 'current.json'
$database = Join-Path $root 'retained-database.sqlite3'; [IO.File]::WriteAllText($database,'retained data')
$hash = (Get-FileHash -LiteralPath $database).Hash
$pointerScript = Join-Path $repo 'scripts/update_release_pointer.ps1'
& $pointerScript -Mode Switch -PointerPath $pointer -ReleaseVersion old -ScannerDatabasePath shared/data/db/old.sqlite3 -Apply | Out-Null
& $pointerScript -Mode Switch -PointerPath $pointer -ReleaseVersion new -ScannerDatabasePath shared/data/db/scan_index_rust_v3.sqlite3 -Apply | Out-Null
& $pointerScript -Mode Rollback -PointerPath $pointer -Apply | Out-Null
$result = Get-Content -LiteralPath $pointer -Raw -Encoding UTF8 | ConvertFrom-Json
Assert-Test ($result.current.release_version -eq 'old' -and $result.previous.release_version -eq 'new') 'Rollback did not swap pointers'
Assert-Test ((Get-FileHash -LiteralPath $database).Hash -eq $hash) 'Rollback changed database bytes'
$unsafe = $false
try { & $pointerScript -Mode Switch -PointerPath $pointer -ReleaseVersion x -ScannerDatabasePath shared/data/db/../../outside.sqlite3 -Apply | Out-Null } catch { $unsafe = $true }
Assert-Test $unsafe 'Pointer path escape was accepted'
$collectPlan = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory (Join-Path $root 'config') -Kind collect
$normalizedCollectXml = $collectPlan.xml.Replace('<StopAtDurationEnd>false</StopAtDurationEnd>', '')
Assert-Test (@(Get-ReportTaskDifferences -Plan $collectPlan -Xml $normalizedCollectXml).Count -eq 0) 'Windows omitted repetition default was rejected'
$wrongIntervalXml = $normalizedCollectXml.Replace('<Interval>PT30M</Interval>', '<Interval>PT60M</Interval>')
Assert-Test (@(Get-ReportTaskDifferences -Plan $collectPlan -Xml $wrongIntervalXml).Count -gt 0) 'Wrong collection interval was accepted'
[ordered]@{ status = 'passed'; scheduler = 'function doubles only'; real_tasks_changed = 0; checks = 12; pointer_rollback_preserved_data = $true } | ConvertTo-Json | Tee-Object -FilePath (Join-Path $root 'result.json')
