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
$unicodeWork = 'work-'+[char]0x5DE5+[char]0x4F5C
$work = (Join-Path $root $unicodeWork).Replace('\','/')
[IO.Directory]::CreateDirectory($work) | Out-Null
$data = (Join-Path $root 'data').Replace('\','/')
$settings = "paths:`n  work_dir: '$work'`n  data_dir: '$data'`n  db_dir: '$data/db'`n  reports_dir: '$data/reports'`nschedule:`n  task_name: 'Rust isolated contract test'`nllm:`n  context_window_tokens: 100000`n  verified_max_output_tokens: 8192`n"
[IO.File]::WriteAllText((Join-Path $root 'config/settings.yaml'), $settings, [Text.UTF8Encoding]::new($false))
$savedEncoding = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = [Text.Encoding]::GetEncoding(936)
    $plan = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory (Join-Path $root 'config')
    $encodingRestored = [Console]::OutputEncoding.CodePage -eq 936
}
finally { [Console]::OutputEncoding = $savedEncoding }
function Assert-Test([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
Assert-Test ($plan.work_dir.Replace('\','/') -ceq $work) 'Task plan corrupted a Unicode work path under CP936'
Assert-Test $encodingRestored 'Task plan did not restore the caller console encoding'
$dailyFields = Get-ReportTaskXmlFields -Xml $plan.xml
Assert-Test ($dailyFields['calendar_count'] -eq 3) 'Daily plan must have three calendar triggers'
Assert-Test ($dailyFields['calendar_times'] -eq '09:00;09:30;10:30') ('Daily trigger times: ' + $dailyFields['calendar_times'])
# Windows omits default-valued elements on export; the comparison must accept that for every trigger.
$normalizedDailyXml = $plan.xml -replace '(<StartBoundary>[^<]+</StartBoundary>)<Enabled>true</Enabled>', '$1'
Assert-Test (@(Get-ReportTaskDifferences -Plan $plan -Xml $normalizedDailyXml).Count -eq 0) 'Windows omission of calendar trigger defaults was rejected'
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
$healthPlan = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory (Join-Path $root 'config') -Kind health
Assert-Test ($healthPlan.kind -eq 'health') 'Health plan kind missing'
Assert-Test ($healthPlan.task_name -like '* Health') 'Health task name missing'
$healthFields = Get-ReportTaskXmlFields -Xml $healthPlan.xml
Assert-Test ($healthFields['calendar_times'] -eq '11:00;23:00') ('Health trigger times: ' + $healthFields['calendar_times'])
Assert-Test ($healthFields['/t:Task/t:Actions/t:Exec/t:Command'] -match 'powershell') 'Health action must use PowerShell'
Assert-Test ($healthFields['/t:Task/t:Actions/t:Exec/t:Arguments'] -match 'health_notify.ps1') 'Health action must call health_notify.ps1'
$script:task = $null; $script:taskXml = ''; $script:registered = 0; $script:badReadback = $false
Install-ReportDailyTask -Plan $healthPlan -Apply | Out-Null
Assert-Test ($script:registered -eq 1) 'Health task registration failed without credential check'
$healthConfig = Join-Path $root 'health-config'
[IO.Directory]::CreateDirectory($healthConfig) | Out-Null
$healthWork = (Join-Path $root 'work').Replace('\','/')
$healthData = (Join-Path $root 'health-data').Replace('\','/')
[IO.File]::WriteAllText((Join-Path $healthConfig 'settings.yaml'), "paths:`n  work_dir: '$healthWork'`n  data_dir: '$healthData'`n  reports_dir: '$healthData/reports'`n  db_dir: '$healthData/db'`n  log_dir: '$healthData/logs'`n", [Text.UTF8Encoding]::new($false))
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $repo 'scripts/health_notify.ps1') -Program $Program -ConfigDirectory $healthConfig -NoNotify | Out-Null
$healthFile = Join-Path $healthData ('logs/health-' + (Get-Date -Format 'yyyy-MM-dd') + '.json')
Assert-Test (Test-Path -LiteralPath $healthFile) 'Health file was not written'
$healthJson = Get-Content -LiteralPath $healthFile -Raw -Encoding UTF8 | ConvertFrom-Json
Assert-Test ($healthJson.status -ne 'healthy') 'Empty data directory should not be healthy'
Assert-Test (@($healthJson.tasks).Count -eq 2) 'Health report must include two task readbacks'
$healthState = Join-Path $healthData 'logs/health-notify-state.json'
Assert-Test (Test-Path -LiteralPath $healthState) 'Health notify state was not written'
Assert-Test (@($healthJson.checks | Where-Object { $_.level -ne 'ok' }).Count -gt 0) 'Health report should list failing checks'
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $repo 'scripts/health_notify.ps1') -Program $Program -ConfigDirectory $healthConfig -NoNotify | Out-Null
$healthJson2 = Get-Content -LiteralPath $healthFile -Raw -Encoding UTF8 | ConvertFrom-Json
Assert-Test (@($healthJson2.tasks).Count -eq 2) 'Repeated health run must stay readable'
[ordered]@{ status = 'passed'; scheduler = 'function doubles only'; real_tasks_changed = 0; checks = 26; pointer_rollback_preserved_data = $true } | ConvertTo-Json | Tee-Object -FilePath (Join-Path $root 'result.json')
