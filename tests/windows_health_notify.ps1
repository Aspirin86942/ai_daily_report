[CmdletBinding()]
param(
    [string]$Program,
    [string]$OutputDirectory,
    [string]$HealthScript
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# ASCII-only source (Windows PowerShell 5.1 decodes BOM-less scripts with the local code page).
$here = if ([string]::IsNullOrEmpty($PSScriptRoot)) { Split-Path -Parent $MyInvocation.MyCommand.Path } else { $PSScriptRoot }
$repo = Split-Path -Parent $here
if ([string]::IsNullOrEmpty($Program)) { $Program = Join-Path $repo 'rust/target/debug/ai-daily-report.exe' }
if ([string]::IsNullOrEmpty($OutputDirectory)) { $OutputDirectory = Join-Path $repo '.artifacts/reliability-closure/windows-health-notify' }
if ([string]::IsNullOrEmpty($HealthScript)) { $script = Join-Path $repo 'scripts/health_notify.ps1' } else { $script = [IO.Path]::GetFullPath($HealthScript) }
$Program = [IO.Path]::GetFullPath($Program)
$root = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $root) { throw 'Use a fresh isolated output directory' }
[IO.Directory]::CreateDirectory($root) | Out-Null

$utf8 = New-Object System.Text.UTF8Encoding($false)
$utf8Strict = New-Object System.Text.UTF8Encoding($false, $true)
$zone = [TimeZoneInfo]::FindSystemTimeZoneById('China Standard Time')
$checks = 0
$blockers = New-Object System.Collections.Generic.List[string]

function Assert-Test([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
    $script:checks++
}

function New-IsolatedConfig([string]$Directory, [string]$TaskName) {
    $config = Join-Path $Directory 'config'
    [IO.Directory]::CreateDirectory($config) | Out-Null
    [IO.Directory]::CreateDirectory((Join-Path $Directory 'work')) | Out-Null
    $work = (Join-Path $Directory 'work').Replace('\', '/')
    $data = (Join-Path $Directory 'data').Replace('\', '/')
    $settings = "paths:`n  work_dir: '$work'`n  data_dir: '$data'`n  reports_dir: '$data/reports'`n  db_dir: '$data/db'`n  log_dir: '$data/logs'`nschedule:`n  task_name: '$TaskName'`n"
    [IO.File]::WriteAllText((Join-Path $config 'settings.yaml'), $settings, $utf8)
    return [pscustomobject]@{ config = $config; data = $data; task = $TaskName }
}

function Read-HealthFile([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    $text = $utf8Strict.GetString($bytes)
    return [pscustomobject]@{ bytes = $bytes; text = $text; json = ($text | ConvertFrom-Json) }
}

function Get-HealthFileName([string]$GeneratedAt) {
    $generated = [DateTimeOffset]::Parse($GeneratedAt)
    return ('health-' + [TimeZoneInfo]::ConvertTime($generated, $zone).ToString('yyyy-MM-dd') + '.json')
}

function Get-HealthFilePath([string]$LogDir, [string]$GeneratedAt) {
    return (Join-Path $LogDir (Get-HealthFileName $GeneratedAt))
}

function Get-HealthTempFiles {
    return @(Get-ChildItem -LiteralPath ([IO.Path]::GetTempPath()) -Filter 'ai-daily-report-health-*' -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName })
}

# The execution-contract stub writes both the --output report and the native
# log_dir health file like the real program, then returns the requested exit
# code. A non-zero exit after a healthy report reproduces the scheduled-log
# write failure that happens after the report has already been written.
function New-HealthStub([string]$Path, [string]$Status, [int]$ExitCode, [bool]$FailScheduledLog, [string]$HistoricalDate = '') {
    $stubText = @'
param()
$out = $null
$configDir = $null
for ($i = 0; $i -lt $args.Count; $i++) {
    if ([string]$args[$i] -eq '--output') { $out = [string]$args[$i + 1] }
    if ([string]$args[$i] -eq '--config-dir') { $configDir = [string]$args[$i + 1] }
}
if ([string]::IsNullOrEmpty($out)) { Write-Output 'stub: missing --output'; exit 2 }
$settings = [IO.File]::ReadAllText((Join-Path $configDir 'settings.yaml'))
$logDir = [regex]::Match($settings, "log_dir:\s*'([^']+)'").Groups[1].Value
$task = [regex]::Match($settings, "task_name:\s*'([^']+)'").Groups[1].Value
$message = -join @([char]0x4E2D, [char]0x6587, [char]0x5065, [char]0x5EB7, [char]0x6D88, [char]0x606F)
$level = 'ok'
if ('__STATUS__' -ne 'healthy') { $level = 'error' }
$report = [ordered]@{
    status = '__STATUS__'
    target_date = '2026-10-18'
    checks = @(
        [ordered]@{ code = 'STUB_CHECK'; level = $level; message = $message; evidence = [ordered]@{ target_date = '2026-10-18' } }
    )
    task_names = [ordered]@{ daily = $task; health = ($task + ' Health') }
    log_dir = $logDir
    generated_at = '2026-10-18T16:30:00+00:00'
}
$utf8 = New-Object System.Text.UTF8Encoding($false)
if ('__HISTORICAL__') {
    $report.checks += [ordered]@{ code = 'HISTORICAL_REPORT_ATTENTION'; level = 'error'; message = 'Historical report needs recovery'; evidence = @{ reports = @(@{ target_date = '__HISTORICAL__' }) } }
}
[IO.Directory]::CreateDirectory($logDir) | Out-Null
$json = $report | ConvertTo-Json -Depth 10
[IO.File]::WriteAllText($out, $json, $utf8)
$zone = [TimeZoneInfo]::FindSystemTimeZoneById('China Standard Time')
$generated = [DateTimeOffset]::Parse('2026-10-18T16:30:00+00:00')
$name = 'health-' + [TimeZoneInfo]::ConvertTime($generated, $zone).ToString('yyyy-MM-dd') + '.json'
[IO.File]::WriteAllText((Join-Path $logDir $name), $json, $utf8)
if (__FAILLOG__ -eq 1) {
    # Simulate append_scheduled_log failing after the report was written: a
    # directory blocks the scheduled jsonl path, then the process exits 1.
    $blocked = Join-Path $logDir 'scheduled-2026-10-18.jsonl'
    [IO.Directory]::CreateDirectory($blocked) | Out-Null
    try { [IO.File]::AppendAllText($blocked, '{}') | Out-Null } catch { Write-Output ('scheduled log write failed: ' + $_.Exception.Message) }
    exit 1
}
exit __EXIT__
'@
    $stubText = $stubText.Replace('__STATUS__', $Status).Replace('__FAILLOG__', $(if ($FailScheduledLog) { '1' } else { '0' })).Replace('__EXIT__', [string]$ExitCode)
    $stubText = $stubText.Replace('__HISTORICAL__', $HistoricalDate)
    [IO.File]::WriteAllText($Path, $stubText, $utf8)
}

# ---------------------------------------------------------------------------
# Phase A: script logic checks against a documented stub program.
# The stub is NOT the production binary; it only proves the PowerShell contract
# (explicit UTF-8 file read, task merge, atomic write, notification failure).
# The stub source is ASCII-only because Windows PowerShell 5.1 decodes BOM-less
# scripts with the local code page; Chinese text is built from code points.
# ---------------------------------------------------------------------------
$chinese = -join @([char]0x4E2D, [char]0x6587, [char]0x5065, [char]0x5EB7, [char]0x6D88, [char]0x606F)
$stub = Join-Path $root 'stub-health-program.ps1'
$stubBody = @'
param()
$out = $null
for ($i = 0; $i -lt $args.Count; $i++) {
    if ([string]$args[$i] -eq '--output') { $out = [string]$args[$i + 1] }
}
$configDir = $null
for ($i = 0; $i -lt $args.Count; $i++) {
    if ([string]$args[$i] -eq '--config-dir') { $configDir = [string]$args[$i + 1] }
}
if ([string]::IsNullOrEmpty($out)) { Write-Output 'stub: missing --output'; exit 2 }
$settings = [IO.File]::ReadAllText((Join-Path $configDir 'settings.yaml'))
$logDir = [regex]::Match($settings, "log_dir:\s*'([^']+)'").Groups[1].Value
$task = [regex]::Match($settings, "task_name:\s*'([^']+)'").Groups[1].Value
$message = -join @([char]0x4E2D, [char]0x6587, [char]0x5065, [char]0x5EB7, [char]0x6D88, [char]0x606F)
$report = [ordered]@{
    status = 'unhealthy'
    target_date = '2026-10-18'
    checks = @(
        [ordered]@{ code = 'REPORT_NOT_PUBLISHED'; level = 'error'; message = $message; evidence = [ordered]@{ target_date = '2026-10-18' } }
    )
    task_names = [ordered]@{ daily = $task; health = ($task + ' Health') }
    log_dir = $logDir
    generated_at = '2026-10-18T16:30:00+00:00'
}
$utf8 = New-Object System.Text.UTF8Encoding($false)
[IO.Directory]::CreateDirectory($logDir) | Out-Null
[IO.File]::WriteAllText($out, ($report | ConvertTo-Json -Depth 10), $utf8)
Write-Output '{"status":"unhealthy"}'
exit 2
'@
[IO.File]::WriteAllText($stub, $stubBody, $utf8)

# Phase A.1: UTF-8 file contract, two task readbacks, temp cleanup.
$logic = New-IsolatedConfig (Join-Path $root 'logic') 'Reliability Closure Health Probe'
$tempsBefore = Get-HealthTempFiles
$stdout = & $script -Program $stub -ConfigDirectory $logic.config -NoNotify | Out-String
$exitCode = $LASTEXITCODE
Assert-Test ($exitCode -eq 1) ('Unhealthy stub result must exit 1, got ' + $exitCode + ': ' + $stdout)
$summary = $stdout | ConvertFrom-Json
Assert-Test ($summary.PSObject.Properties.Name -contains 'health_file') 'Script summary must report the health file path'
$healthPath = [string]$summary.health_file
Assert-Test (Test-Path -LiteralPath $healthPath) 'Health file was not written'
Assert-Test ((Split-Path -Leaf $healthPath) -eq (Get-HealthFileName '2026-10-18T16:30:00+00:00')) 'Health file name must use the Shanghai date of generated_at'
$health = Read-HealthFile $healthPath
Assert-Test (-not ($health.bytes[0] -eq 0xEF -and $health.bytes[1] -eq 0xBB -and $health.bytes[2] -eq 0xBF)) 'Health file must be UTF-8 without BOM'
Assert-Test (-not $health.text.Contains([char]0xFFFD)) 'Health file contains invalid UTF-8 replacement characters'
Assert-Test ([string]$health.json.checks[0].message -eq $chinese) 'Chinese health text must survive the UTF-8 file round trip'
Assert-Test (@($health.json.tasks).Count -eq 2) ('Health report must include two task readbacks, got ' + @($health.json.tasks).Count)
$taskNames = @($health.json.tasks | ForEach-Object { [string]$_.name })
Assert-Test ($taskNames[0] -eq 'Reliability Closure Health Probe') 'Daily task name must come from configuration'
Assert-Test ($taskNames[1] -eq 'Reliability Closure Health Probe Health') 'Health task name missing'
Assert-Test ($health.json.task_names.health -eq 'Reliability Closure Health Probe Health') 'task_names.health must be reported by the program'
Assert-Test (@($health.json.tasks | Where-Object { $_.exists }).Count -eq 0) 'Isolated task names must not exist in the scheduler'
$logDir = [string]$health.json.log_dir
$tempsAfter = Get-HealthTempFiles
$leaked = @($tempsAfter | Where-Object { $tempsBefore -notcontains $_ })
Assert-Test ($leaked.Count -eq 0) ('Script leaked its temporary file: ' + ($leaked -join ', '))
Assert-Test (@(Get-ChildItem -LiteralPath $logDir -Filter '*.tmp' -ErrorAction SilentlyContinue).Count -eq 0) 'Atomic replace left temporary files in log_dir'

# Phase A.2: repeated run stays readable and idempotent.
$stdout2 = & $script -Program $stub -ConfigDirectory $logic.config -NoNotify | Out-String
Assert-Test ($LASTEXITCODE -eq 1) 'Repeated unhealthy run must keep exit code 1'
$health2 = Read-HealthFile ([string]($stdout2 | ConvertFrom-Json).health_file)
Assert-Test (@($health2.json.tasks).Count -eq 2) 'Repeated health run must stay readable with two tasks'
$statePath = Join-Path $logDir 'health-notify-state.json'
Assert-Test (Test-Path -LiteralPath $statePath) 'Notification dedup state was not written'
$state = [IO.File]::ReadAllText($statePath, $utf8Strict) | ConvertFrom-Json
Assert-Test ($state.date.Length -eq 10) 'Notification state date is invalid'

# A damaged dedup state must not disable subsequent health notifications.
foreach ($damagedState in @('{malformed', '{"codes":"missing-date"}')) {
    [IO.File]::WriteAllText($statePath, $damagedState, $utf8)
    $repairStdout = & $script -Program $stub -ConfigDirectory $logic.config -NoNotify | Out-String
    Assert-Test ($LASTEXITCODE -eq 1) 'Damaged notification state must preserve the unhealthy exit contract'
    $repairSummary = $repairStdout | ConvertFrom-Json
    Assert-Test (@($repairSummary.anomalies) -contains 'NOTIFY_STATE_INVALID') 'Damaged state must be reported and repaired'
    $repairedState = [IO.File]::ReadAllText($statePath, $utf8Strict) | ConvertFrom-Json
    Assert-Test ([string]$repairedState.codes -like '*NOTIFY_STATE_INVALID*') 'Repair must persist a new valid dedup state'
    Assert-Test ((Read-HealthFile ([string]$repairSummary.health_file)).json.status -eq 'unhealthy') 'State repair must preserve the native verdict'
}
$stateLock = [IO.File]::Open($statePath, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
try {
    $lockedStdout = & $script -Program $stub -ConfigDirectory $logic.config -NoNotify | Out-String
    $lockedExit = $LASTEXITCODE
}
finally { $stateLock.Dispose() }
Assert-Test ($lockedExit -eq 3) 'Unreadable and unwritable state must follow the notification error path'
$lockedSummary = $lockedStdout | ConvertFrom-Json
Assert-Test (@($lockedSummary.anomalies) -contains 'NOTIFY_STATE_INVALID') 'Unreadable state must have an explicit anomaly'
$lockedError = [IO.File]::ReadAllText((Join-Path $logDir 'health-notify-error.json'), $utf8Strict) | ConvertFrom-Json
Assert-Test ($lockedError.code -eq 'NOTIFY_FAILED') 'Blocked state replacement must be recorded independently'

# Phase A.3: CP936 console must not corrupt the health file.
$cp936 = New-IsolatedConfig (Join-Path $root 'logic-cp936') 'Reliability Closure Health Probe CP936'
$command = 'chcp 936 >nul & powershell -NoProfile -ExecutionPolicy Bypass -File "' + $script + '" -Program "' + $stub + '" -ConfigDirectory "' + $cp936.config + '" -NoNotify'
$null = & cmd /c $command
Assert-Test ($LASTEXITCODE -eq 1) ('CP936 run must keep exit code 1, got ' + $LASTEXITCODE)
$cp936LogDir = Join-Path $cp936.data 'logs'
$cp936File = Get-ChildItem -LiteralPath $cp936LogDir -Filter 'health-*.json' | Where-Object { $_.Name -notlike '*state*' } | Select-Object -First 1
Assert-Test ($null -ne $cp936File) 'CP936 run did not write a health file'
$cp936Health = Read-HealthFile $cp936File.FullName
Assert-Test (-not $cp936Health.text.Contains([char]0xFFFD)) 'CP936 console corrupted the health file bytes'
Assert-Test ([string]$cp936Health.json.checks[0].message -eq $chinese) 'CP936 console run lost Chinese health text'
Assert-Test ($cp936Health.json.task_names.health -eq 'Reliability Closure Health Probe CP936 Health') 'CP936 run lost the health task name'
Assert-Test (@($cp936Health.json.tasks).Count -eq 2) 'CP936 run lost task readbacks'

# Phase A.4: notification component failure keeps the health file and exits 3.
$failure = New-IsolatedConfig (Join-Path $root 'logic-notify-failure') 'Reliability Closure Health Probe Notify'
$null = & $script -Program $stub -ConfigDirectory $failure.config -NoNotify | Out-String
$failureLogDir = Join-Path $failure.data 'logs'
$failureHealthFile = Get-ChildItem -LiteralPath $failureLogDir -Filter 'health-*.json' | Where-Object { $_.Name -notlike '*state*' } | Select-Object -First 1
Assert-Test ($null -ne $failureHealthFile) 'Baseline health file for the notification failure case is missing'
$beforeHealth = Read-HealthFile $failureHealthFile.FullName
$failureState = Join-Path $failureLogDir 'health-notify-state.json'
[IO.File]::WriteAllText($failureState, '{"date":"2000-01-01","codes":"FORCED"}', $utf8)
(Get-Item -LiteralPath $failureState).IsReadOnly = $true
try {
    $failureStdout = & $script -Program $stub -ConfigDirectory $failure.config -NoNotify | Out-String
    $failureExit = $LASTEXITCODE
}
finally {
    (Get-Item -LiteralPath $failureState).IsReadOnly = $false
}
Assert-Test ($failureExit -eq 3) ('Notification component failure must exit 3, got ' + $failureExit + ': ' + $failureStdout)
$failureSummary = $failureStdout | ConvertFrom-Json
Assert-Test ($failureSummary.notified -eq $false) 'Failed notification must report notified=false'
$failureHealthAfter = Read-HealthFile $failureHealthFile.FullName
Assert-Test (@($failureHealthAfter.json.tasks).Count -eq 2) 'Notification failure must keep the two-task health file readable'
Assert-Test ($failureHealthAfter.json.status -eq $beforeHealth.json.status) 'Notification failure must not alter the native health status'
$errorRecord = Join-Path $failureLogDir 'health-notify-error.json'
Assert-Test (Test-Path -LiteralPath $errorRecord) 'Notification failure must be recorded independently'
$errorJson = [IO.File]::ReadAllText($errorRecord, $utf8Strict) | ConvertFrom-Json
Assert-Test ($errorJson.code -eq 'NOTIFY_FAILED') 'Notification failure record must carry NOTIFY_FAILED'
Assert-Test ([string]$errorJson.health_file -eq $failureHealthFile.FullName) 'Notification failure record must point at the preserved health file'

# Phase A.5: a healthy report with a non-zero program exit is an execution
# failure, not a healthy scheduled run. The report is still written before the
# scheduled log fails, which is exactly the reported defect.
$exec = New-IsolatedConfig (Join-Path $root 'logic-execution-failure') 'Reliability Closure Health Probe Execution'
$execStub = Join-Path $root 'stub-health-log-write-failure.ps1'
New-HealthStub $execStub 'healthy' 1 $true
$execStdout = & $script -Program $execStub -ConfigDirectory $exec.config -NoNotify | Out-String
$execExit = $LASTEXITCODE
Assert-Test ($execExit -eq 4) ('Healthy report with program exit 1 must return 4, got ' + $execExit + ': ' + $execStdout)
$execSummary = $execStdout | ConvertFrom-Json
Assert-Test ([string]$execSummary.status -eq 'healthy') 'Execution failure must not rewrite the report verdict in the summary'
Assert-Test ([string]$execSummary.execution_status -eq 'failed') 'Execution failure must report execution_status=failed'
Assert-Test ([int]$execSummary.program_exit_code -eq 1) 'Summary must keep the actual program exit code 1'
Assert-Test ([int]$execSummary.expected_program_exit_code -eq 0) 'Summary must report the expected program exit code 0'
Assert-Test (@($execSummary.anomalies) -contains 'HEALTH_PROGRAM_FAILED') 'Summary anomalies must carry HEALTH_PROGRAM_FAILED'
$execLogDir = Join-Path $exec.data 'logs'
$execHealth = Read-HealthFile ([string]$execSummary.health_file)
Assert-Test ([string]$execHealth.json.status -eq 'healthy') 'Enhanced health file must preserve the original healthy verdict'
Assert-Test ([string]$execHealth.json.execution_status -eq 'failed') 'Enhanced health file must record execution_status=failed'
Assert-Test ([int]$execHealth.json.program_exit_code -eq 1) 'Enhanced health file must record the actual program exit code'
Assert-Test ([int]$execHealth.json.expected_program_exit_code -eq 0) 'Enhanced health file must record the expected program exit code'
Assert-Test (@($execHealth.json.tasks).Count -eq 2) 'Execution failure must keep the two-task health file readable'
$execErrorPath = Join-Path $execLogDir 'health-execution-error.json'
Assert-Test (Test-Path -LiteralPath $execErrorPath) 'Execution failure must be recorded in health-execution-error.json'
$execError = [IO.File]::ReadAllText($execErrorPath, $utf8Strict) | ConvertFrom-Json
Assert-Test ([string]$execError.code -eq 'HEALTH_PROGRAM_FAILED') 'Execution error record must carry HEALTH_PROGRAM_FAILED'
Assert-Test ([int]$execError.program_exit_code -eq 1) 'Execution error record must keep the actual program exit code'
Assert-Test ([int]$execError.expected_program_exit_code -eq 0) 'Execution error record must keep the expected program exit code'
Assert-Test ([string]$execError.report.status -eq 'healthy') 'Execution error record must preserve the original health verdict'
Assert-Test ($execError.report.target_date -eq '2026-10-18') 'Execution error record must preserve the original target date'
Assert-Test (@($execError.report.checks).Count -eq 1 -and [string]$execError.report.checks[0].code -eq 'STUB_CHECK') 'Execution error record must preserve the original health checks'
Assert-Test (-not (Test-Path -LiteralPath (Join-Path $execLogDir 'health-notify-error.json'))) 'Execution failure must not be reported as a notification failure'
$execState = [IO.File]::ReadAllText((Join-Path $execLogDir 'health-notify-state.json'), $utf8Strict) | ConvertFrom-Json
Assert-Test ([string]$execState.codes -like '*HEALTH_PROGRAM_FAILED*') 'Notification reason must carry HEALTH_PROGRAM_FAILED'

# Phase A.6: native execution succeeds; missing scheduled tasks still warn.
$healthy = New-IsolatedConfig (Join-Path $root 'logic-healthy-exit0') 'Reliability Closure Health Probe Healthy'
$healthyStub = Join-Path $root 'stub-health-healthy-exit0.ps1'
New-HealthStub $healthyStub 'healthy' 0 $false
$healthyStdout = & $script -Program $healthyStub -ConfigDirectory $healthy.config -NoNotify | Out-String
$healthyExit = $LASTEXITCODE
Assert-Test ($healthyExit -eq 1) ('Missing isolated tasks must warn despite healthy native output, got ' + $healthyExit + ': ' + $healthyStdout)
$healthySummary = $healthyStdout | ConvertFrom-Json
Assert-Test ([string]$healthySummary.execution_status -eq 'succeeded') 'Healthy run must report execution_status=succeeded'
Assert-Test ([int]$healthySummary.program_exit_code -eq 0) 'Healthy run must report the program exit code 0'
Assert-Test ([int]$healthySummary.expected_program_exit_code -eq 0) 'Healthy run must report the expected exit code 0'
Assert-Test (@($healthySummary.anomalies).Count -eq 0) 'Healthy run must not report anomalies'
$healthyLogDir = Join-Path $healthy.data 'logs'
$healthyHealth = Read-HealthFile ([string]$healthySummary.health_file)
Assert-Test ([string]$healthyHealth.json.status -eq 'healthy') 'Healthy run health file must stay healthy'
Assert-Test ([string]$healthyHealth.json.execution_status -eq 'succeeded') 'Healthy run health file must record execution_status=succeeded'
Assert-Test ([int]$healthyHealth.json.program_exit_code -eq 0) 'Healthy run health file must record the program exit code'
Assert-Test (@($healthyHealth.json.tasks).Count -eq 2) 'Healthy run health file must carry two task readbacks'
Assert-Test (-not (Test-Path -LiteralPath (Join-Path $healthyLogDir 'health-execution-error.json'))) 'Healthy run must not write an execution error record'

# Historical dates participate in deduplication even when the general code is unchanged.
$historical = New-IsolatedConfig (Join-Path $root 'logic-historical') 'Historical Health Probe'
$historicalStub = Join-Path $root 'stub-health-historical.ps1'
New-HealthStub $historicalStub 'unhealthy' 2 $false '2026-10-23'
$null = & $script -Program $historicalStub -ConfigDirectory $historical.config -NoNotify
Assert-Test ($LASTEXITCODE -eq 1) 'Historical issue must retain the unhealthy script result'
$historicalLog = Join-Path (Join-Path $historical.data 'logs') 'health-notify.log'
Assert-Test (@(Get-Content -LiteralPath $historicalLog).Count -eq 1) 'First historical issue must be logged'
$null = & $script -Program $historicalStub -ConfigDirectory $historical.config -NoNotify
Assert-Test (@(Get-Content -LiteralPath $historicalLog).Count -eq 1) 'Unchanged historical issue must be deduplicated within the day'
New-HealthStub $historicalStub 'unhealthy' 2 $false '2026-10-24'
$null = & $script -Program $historicalStub -ConfigDirectory $historical.config -NoNotify
Assert-Test (@(Get-Content -LiteralPath $historicalLog).Count -eq 2) 'A different unresolved date must produce a new notification record'
$historicalState = [IO.File]::ReadAllText((Join-Path (Join-Path $historical.data 'logs') 'health-notify-state.json'), $utf8Strict) | ConvertFrom-Json
Assert-Test ([string]$historicalState.codes -like '*HISTORICAL_REPORT_ATTENTION:2026-10-24*') 'Notification state must identify the historical target date'

# Phase A.7: warning/unhealthy keep the exit-1 script contract only when the
# program exit code matches the verdict mapping.
$warning = New-IsolatedConfig (Join-Path $root 'logic-warning-exit1') 'Reliability Closure Health Probe Warning'
$warningStub = Join-Path $root 'stub-health-warning-exit1.ps1'
New-HealthStub $warningStub 'warning' 1 $false
$warningStdout = & $script -Program $warningStub -ConfigDirectory $warning.config -NoNotify | Out-String
$warningExit = $LASTEXITCODE
Assert-Test ($warningExit -eq 1) ('Warning report with program exit 1 must keep exit 1, got ' + $warningExit + ': ' + $warningStdout)
$warningSummary = $warningStdout | ConvertFrom-Json
Assert-Test ([string]$warningSummary.execution_status -eq 'succeeded') 'Warning run must report execution_status=succeeded'
$mismatch = New-IsolatedConfig (Join-Path $root 'logic-unhealthy-exit1') 'Reliability Closure Health Probe Mismatch'
$mismatchStub = Join-Path $root 'stub-health-unhealthy-exit1.ps1'
New-HealthStub $mismatchStub 'unhealthy' 1 $false
$mismatchStdout = & $script -Program $mismatchStub -ConfigDirectory $mismatch.config -NoNotify | Out-String
$mismatchExit = $LASTEXITCODE
Assert-Test ($mismatchExit -eq 4) ('Unhealthy report with program exit 1 must return 4, got ' + $mismatchExit + ': ' + $mismatchStdout)
$mismatchSummary = $mismatchStdout | ConvertFrom-Json
Assert-Test ([string]$mismatchSummary.execution_status -eq 'failed') 'Mismatched exit code must report execution_status=failed'
Assert-Test (@($mismatchSummary.anomalies) -contains 'HEALTH_PROGRAM_FAILED') 'Mismatched exit code must add HEALTH_PROGRAM_FAILED'
$mismatchErrorPath = Join-Path (Join-Path $mismatch.data 'logs') 'health-execution-error.json'
Assert-Test (Test-Path -LiteralPath $mismatchErrorPath) 'Mismatched exit code must write the execution error record'
$mismatchError = [IO.File]::ReadAllText($mismatchErrorPath, $utf8Strict) | ConvertFrom-Json
Assert-Test ([string]$mismatchError.report.status -eq 'unhealthy') 'Execution error record must keep the unhealthy verdict'

# Phase A.8: the script must be read-only towards scheduled tasks.
Assert-Test (@(Get-ScheduledTask -TaskName 'Reliability Closure Health Probe' -ErrorAction SilentlyContinue).Count -eq 0) 'Isolated probe task must never be registered'
Assert-Test (@(Get-ScheduledTask -TaskName 'Reliability Closure Health Probe Execution' -ErrorAction SilentlyContinue).Count -eq 0) 'Execution probe task must never be registered'

# ---------------------------------------------------------------------------
# Phase B: real program acceptance for `health --scheduled --output <path>`,
# native persist, and the CP936 environment.
# ---------------------------------------------------------------------------
$realStatus = 'passed'
$realDetail = ''
$helpText = & $Program health --help 2>&1 | Out-String
if ($helpText.IndexOf('--output', [StringComparison]::Ordinal) -lt 0) {
    $realStatus = 'blocked'
    $realDetail = 'PROGRAM_MISSING_HEALTH_OUTPUT: health --output is not implemented yet'
    $blockers.Add($realDetail)
}
else {
    # B.1: the program alone must atomically persist the native health file.
    $native = New-IsolatedConfig (Join-Path $root 'real-native') 'Reliability Closure Health Probe Native'
    $nativeOut = Join-Path $root 'real-native-output.json'
    $nativeStdout = & $Program --config-dir $native.config health --scheduled --output $nativeOut 2>&1 | Out-String
    Assert-Test (@(1, 2) -contains $LASTEXITCODE) ('Direct program run must exit 1 or 2, got ' + $LASTEXITCODE)
    Assert-Test (Test-Path -LiteralPath $nativeOut) 'Program did not honour --output'
    $nativeJson = Read-HealthFile $nativeOut
    Assert-Test (-not $nativeJson.text.Contains([char]0xFFFD)) 'Program --output file is not valid UTF-8'
    Assert-Test ($nativeJson.json.status -ne 'healthy') 'Empty isolated data must not be healthy'
    $nativePath = Get-HealthFilePath ([string]$nativeJson.json.log_dir) ([string]$nativeJson.json.generated_at)
    Assert-Test (Test-Path -LiteralPath $nativePath) 'Program did not persist the native log_dir health file'
    $nativeLogFile = Read-HealthFile $nativePath
    Assert-Test ($nativeLogFile.json.status -eq $nativeJson.json.status) 'Native and --output health files disagree on status'
    Assert-Test (-not ($nativeLogFile.json.PSObject.Properties.Name -contains 'tasks')) 'Native persist must not contain the notification task readback'
    Assert-Test ($nativeLogFile.json.task_names.health -eq 'Reliability Closure Health Probe Native Health') 'Native persist must carry the health task name'
    $scheduledLog = Join-Path ([string]$nativeJson.json.log_dir) ('scheduled-' + (Get-HealthFileName ([string]$nativeJson.json.generated_at)).Substring(7, 10) + '.jsonl')
    Assert-Test (Test-Path -LiteralPath $scheduledLog) 'Scheduled run must still write its scheduled log entry'
    Assert-Test (@(Get-ChildItem -LiteralPath ([string]$nativeJson.json.log_dir) -Filter '*.tmp' -ErrorAction SilentlyContinue).Count -eq 0) 'Program left temporary files in log_dir'

    # B.2: full script run against the real program.
    $real = New-IsolatedConfig (Join-Path $root 'real-program') 'Reliability Closure Health Probe Real'
    $realStdout = & $script -Program $Program -ConfigDirectory $real.config -NoNotify | Out-String
    $realExit = $LASTEXITCODE
    Assert-Test (@(1, 2) -contains $realExit) ('Real unhealthy run must exit 1 or 2, got ' + $realExit + ': ' + $realStdout)
    $realSummary = $realStdout | ConvertFrom-Json
    $realFile = [string]$realSummary.health_file
    Assert-Test (Test-Path -LiteralPath $realFile) 'Real program run did not produce a health file'
    $realHealth = Read-HealthFile $realFile
    Assert-Test (-not $realHealth.text.Contains([char]0xFFFD)) 'Real program health file is not valid UTF-8'
    Assert-Test (@($realHealth.json.tasks).Count -eq 2) 'Real program health file must carry two task readbacks'
    Assert-Test ($realHealth.json.task_names.health -eq 'Reliability Closure Health Probe Real Health') 'Real program must report the health task name'
    Assert-Test ($realHealth.json.checks.Count -gt 0) 'Real program health file must carry checks'
    Assert-Test (-not (@($realSummary.anomalies) -contains 'HEALTH_NATIVE_FILE_MISSING')) 'Script must find the native file written by the program'
    $realLogDir = [string]$realHealth.json.log_dir
    Assert-Test (@(Get-ChildItem -LiteralPath $realLogDir -Filter '*.tmp' -ErrorAction SilentlyContinue).Count -eq 0) 'Real program run left temp files in log_dir'
    Assert-Test ((Split-Path -Leaf $realFile) -eq (Get-HealthFileName ([string]$realHealth.json.generated_at))) 'Real health file name must use the Shanghai date of generated_at'

    # B.3: CP936 console with the real program.
    $realCp936 = New-IsolatedConfig (Join-Path $root 'real-cp936') 'Reliability Closure Health Probe Real CP936'
    $realCommand = 'chcp 936 >nul & powershell -NoProfile -ExecutionPolicy Bypass -File "' + $script + '" -Program "' + $Program + '" -ConfigDirectory "' + $realCp936.config + '" -NoNotify'
    $null = & cmd /c $realCommand
    $realCp936Exit = $LASTEXITCODE
    Assert-Test (@(1, 2) -contains $realCp936Exit) ('Real CP936 run must exit 1 or 2, got ' + $realCp936Exit)
    $realCp936Log = Join-Path $realCp936.data 'logs'
    $realCp936File = Get-ChildItem -LiteralPath $realCp936Log -Filter 'health-2*.json' | Select-Object -First 1
    Assert-Test ($null -ne $realCp936File) 'Real CP936 run did not write a health file'
    $realCp936Health = Read-HealthFile $realCp936File.FullName
    Assert-Test (-not $realCp936Health.text.Contains([char]0xFFFD)) 'Real CP936 run corrupted the health file bytes'
    Assert-Test (@($realCp936Health.json.tasks).Count -eq 2) 'Real CP936 run lost task readbacks'
    Assert-Test ($realCp936Health.json.task_names.health -eq 'Reliability Closure Health Probe Real CP936 Health') 'Real CP936 run lost the health task name'
    Assert-Test (@($realCp936Health.json.checks | Where-Object { $_.level -ne 'ok' }).Count -gt 0) 'Real CP936 run must keep failing checks readable'
}

$status = if ($blockers.Count -gt 0) { 'passed_with_blocked_real_program_check' } else { 'passed' }
[ordered]@{
    status = $status
    checks = $checks
    logic_program = 'stub (not production)'
    real_program = $Program
    real_program_check = $realStatus
    real_program_detail = $realDetail
    real_tasks_changed = 0
    scheduler = 'read-only readback via Get-ScheduledTask'
    cp936_checked = $true
    notification_failure_exit_code = 3
    blockers = @($blockers)
} | ConvertTo-Json -Depth 5 | Tee-Object -FilePath (Join-Path $root 'result.json')

if ($blockers.Count -gt 0) { exit 5 }
exit 0
