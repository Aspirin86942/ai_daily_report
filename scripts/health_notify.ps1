[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Program,
    [Parameter(Mandatory = $true)][string]$ConfigDirectory,
    [switch]$NoNotify
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Keep this file ASCII-only (Windows PowerShell 5.1 decodes BOM-less scripts with the
# local code page). All UTF-8 handling below is explicit through this encoder.
$utf8 = New-Object System.Text.UTF8Encoding($false)
$zone = [TimeZoneInfo]::FindSystemTimeZoneById('China Standard Time')

# Write the target file atomically: temp file in the same directory, then replace.
function Write-AtomicUtf8 {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Text)
    $full = [IO.Path]::GetFullPath($Path)
    $dir = [IO.Path]::GetDirectoryName($full)
    [IO.Directory]::CreateDirectory($dir) | Out-Null
    $temporary = Join-Path $dir ('.' + [IO.Path]::GetFileName($full) + '.' + [guid]::NewGuid().ToString('N') + '.tmp')
    try {
        [IO.File]::WriteAllText($temporary, $Text, $utf8)
        if (-not [IO.File]::Exists($full)) {
            [IO.File]::Move($temporary, $full)
            return
        }
        # Preserve the previous result if atomic replacement is unavailable or denied.
        # In 5.1 a plain $null becomes an empty path, so use NullString explicitly.
        [IO.File]::Replace($temporary, $full, [NullString]::Value)
    }
    finally {
        # Never leave this script's own temp file behind.
        if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
    }
}

$anomalies = New-Object System.Collections.Generic.List[string]
$temp = Join-Path ([IO.Path]::GetTempPath()) ('ai-daily-report-health-' + [guid]::NewGuid().ToString('N') + '.json')
$report = $null
$originalReport = $null
$code = 0
$expectedCode = -1
$executionStatus = 'succeeded'
$executionErrorFile = $null
try {
    # Real program: --scheduled writes the scheduled log, --output writes the report
    # atomically to this script's own temp file. The pipe is only used to wait for the
    # process to exit; stdout is never parsed (it can be decoded with the local code page).
    try {
        $null = & $Program --config-dir $ConfigDirectory health --scheduled --output $temp | Out-String
        # The program exit code is evidence only; the health verdict comes from the file.
        if (Test-Path -Path Variable:LASTEXITCODE) { $code = [int]$LASTEXITCODE }
    }
    catch {
        throw ('HEALTH_PROGRAM_FAILED: ' + $_.Exception.Message)
    }
    if (-not (Test-Path -LiteralPath $temp)) { throw 'HEALTH_OUTPUT_MISSING' }
    $text = [IO.File]::ReadAllText($temp, $utf8)
    $report = $text | ConvertFrom-Json
    if ($null -eq $report -or -not ($report.PSObject.Properties.Name -contains 'status')) {
        throw 'HEALTH_OUTPUT_INVALID'
    }
    # Keep the untouched program result: the execution-failure record must never
    # pretend the health verdict changed.
    $originalReport = $text | ConvertFrom-Json

    # The program writes the report before append_scheduled_log, so a write
    # failure can leave a healthy report behind while the process exits 1.
    # The verdict stays as reported; the exit code is checked independently.
    switch ([string]$report.status) {
        'healthy' { $expectedCode = 0 }
        'warning' { $expectedCode = 1 }
        default { $expectedCode = 2 }
    }
    if ($code -ne $expectedCode) {
        $executionStatus = 'failed'
        $anomalies.Add('HEALTH_PROGRAM_FAILED')
    }

    # Read back the supported daily and health tasks. This is read-only: the script never
    # registers, modifies or removes a scheduled task.
    $names = New-Object System.Collections.Generic.List[string]
    foreach ($property in @('daily', 'health')) {
        if (-not ($report.task_names.PSObject.Properties.Name -contains $property)) {
            $anomalies.Add('TASK_NAME_MISSING:' + $property)
            continue
        }
        $name = [string]$report.task_names.$property
        if ($name.Length -eq 0) {
            $anomalies.Add('TASK_NAME_EMPTY:' + $property)
            continue
        }
        $names.Add($name)
    }
    $tasks = @()
    $healthName = ''
    if ($report.task_names.PSObject.Properties.Name -contains 'health') { $healthName = [string]$report.task_names.health }
    foreach ($name in $names) {
        $expectedEnabled = $true
        $task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
        if ($null -eq $task) {
            $tasks += [pscustomobject]@{ name = $name; exists = $false; expected_enabled = $expectedEnabled; state = ''; last_result = $null; next_run_shanghai = '' }
            continue
        }
        $taskState = ''
        if ($task.PSObject.Properties.Name -contains 'State') { $taskState = [string]$task.State }
        if ($expectedEnabled -and $taskState -eq 'Disabled') { $anomalies.Add('TASK_DISABLED:' + $name) }
        $info = Get-ScheduledTaskInfo -TaskName $name -ErrorAction SilentlyContinue
        $last = $null
        $next = ''
        if ($null -ne $info) {
            if ($info.PSObject.Properties.Name -contains 'LastTaskResult') { $last = $info.LastTaskResult }
            if ($info.PSObject.Properties.Name -contains 'NextRunTime' -and $info.NextRunTime.Year -gt 2000) {
                $next = [TimeZoneInfo]::ConvertTime($info.NextRunTime, $zone).ToString('yyyy-MM-dd HH:mm:ss')
            }
        }
        $tasks += [pscustomobject]@{ name = $name; exists = $true; expected_enabled = $expectedEnabled; state = $taskState; last_result = $last; next_run_shanghai = $next }
    }
    if ($report.PSObject.Properties.Name -contains 'tasks') {
        $report.PSObject.Properties.Remove('tasks')
    }
    $report | Add-Member -NotePropertyName tasks -NotePropertyValue $tasks -Force

    # Rust already wrote the native health file for the Shanghai date of generated_at;
    # merge the task readback into that content instead of replacing it.
    $generated = [DateTimeOffset]::Parse([string]$report.generated_at)
    $date = [TimeZoneInfo]::ConvertTime($generated, $zone).ToString('yyyy-MM-dd')
    $logDir = [string]$report.log_dir
    [IO.Directory]::CreateDirectory($logDir) | Out-Null
    $healthFile = Join-Path $logDir ('health-' + $date + '.json')
    $target = $report
    if (Test-Path -LiteralPath $healthFile) {
        try { $target = [IO.File]::ReadAllText($healthFile, $utf8) | ConvertFrom-Json }
        catch { $anomalies.Add('HEALTH_NATIVE_FILE_INVALID') }
    }
    else {
        # A missing native file is an anomaly, but the notification path stays usable.
        $anomalies.Add('HEALTH_NATIVE_FILE_MISSING')
    }
    if ($target.PSObject.Properties.Name -contains 'tasks') {
        $target.PSObject.Properties.Remove('tasks')
    }
    $target | Add-Member -NotePropertyName tasks -NotePropertyValue $tasks -Force
    # Execution evidence is additive: status/checks/target_date stay exactly as the
    # program reported them, only the run evidence is attached.
    $target | Add-Member -NotePropertyName execution_status -NotePropertyValue $executionStatus -Force
    $target | Add-Member -NotePropertyName program_exit_code -NotePropertyValue $code -Force
    $target | Add-Member -NotePropertyName expected_program_exit_code -NotePropertyValue $expectedCode -Force
    Write-AtomicUtf8 -Path $healthFile -Text ($target | ConvertTo-Json -Depth 12)

    # A mismatched exit code is its own execution failure. Keep a separate record
    # with the untouched program report so the healthy verdict stays auditable.
    if ($executionStatus -eq 'failed') {
        $executionErrorFile = Join-Path $logDir 'health-execution-error.json'
        try {
            Write-AtomicUtf8 -Path $executionErrorFile -Text ([ordered]@{
                date = $date
                code = 'HEALTH_PROGRAM_FAILED'
                message = 'health program exit code does not match the reported status'
                program_exit_code = $code
                expected_program_exit_code = $expectedCode
                health_file = $healthFile
                report = $originalReport
            } | ConvertTo-Json -Depth 12)
        }
        catch { $anomalies.Add('HEALTH_EXECUTION_ERROR_WRITE_FAILED') }
    }

    $failing = @($report.checks | Where-Object { $_.level -ne 'ok' })
    # The health task itself is excluded from failure judgement: it is running (0x41301)
    # and this script may exit non-zero by design.
    $checkable = @($tasks | Where-Object { $_.name -ne $healthName })
    $missing = @($checkable | Where-Object { $_.expected_enabled -and -not $_.exists })
    $badResult = @($checkable | Where-Object {
        $_.expected_enabled -and $_.exists -and $null -ne $_.last_result -and $_.last_result -ne 0 -and $_.last_result -ne 267009
    })
    $stateFile = Join-Path $logDir 'health-notify-state.json'
    $state = $null
    if (Test-Path -LiteralPath $stateFile) {
        try {
            $state = [IO.File]::ReadAllText($stateFile, $utf8) | ConvertFrom-Json
            if ($null -eq $state -or -not ($state.PSObject.Properties.Name -contains 'date') -or -not ($state.PSObject.Properties.Name -contains 'codes')) {
                throw 'NOTIFY_STATE_INVALID'
            }
        }
        catch { $state = $null; $anomalies.Add('NOTIFY_STATE_INVALID') }
    }
    $notify = ($report.status -ne 'healthy') -or ($missing.Count -gt 0) -or ($badResult.Count -gt 0) -or ($anomalies.Count -gt 0)
    $badCodes = @($badResult | ForEach-Object { 'TASK_LAST_RESULT:' + $_.name + ':' + $_.last_result })
    $codes = (($failing | ForEach-Object {
        if ($_.code -eq 'HISTORICAL_REPORT_ATTENTION') {
            foreach ($item in $_.evidence.reports) { 'HISTORICAL_REPORT_ATTENTION:' + $item.target_date }
        } else { $_.code }
    }) + ($missing | ForEach-Object { 'TASK_MISSING' }) + $badCodes + $anomalies | Sort-Object -Unique) -join ','
    $already = $null -ne $state -and $state.date -eq $date -and $state.codes -eq $codes

    $notified = $false
    if ($notify -and -not $already) {
        # Notification is an add-on step: any failure here must preserve the health file
        # and be recorded separately instead of pretending the run was healthy.
        try {
            if (-not $NoNotify) {
                Add-Type -AssemblyName System.Windows.Forms
                Add-Type -AssemblyName System.Drawing
                $icon = New-Object System.Windows.Forms.NotifyIcon
                $icon.Icon = [System.Drawing.SystemIcons]::Warning
                $icon.Visible = $true
                $messages = @($failing | ForEach-Object { $_.message })
                if ($missing.Count -gt 0) { $messages += ('Missing tasks: ' + (($missing | ForEach-Object { $_.name }) -join ', ')) }
                foreach ($anomaly in $anomalies) { $messages += $anomaly }
                $summaryText = ($messages -join '; ')
                if ($summaryText.Length -gt 400) { $summaryText = $summaryText.Substring(0, 400) + '...' }
                $icon.ShowBalloonTip(10000, 'AI Daily Report health alert', ('Target ' + $report.target_date + ': ' + $summaryText), [System.Windows.Forms.ToolTipIcon]::Warning)
                Start-Sleep -Seconds 6
                $icon.Dispose()
                $notified = $true
            }
            Write-AtomicUtf8 -Path $stateFile -Text ([ordered]@{ date = $date; codes = $codes; status = $report.status; target_date = $report.target_date } | ConvertTo-Json)
            Add-Content -LiteralPath (Join-Path $logDir 'health-notify.log') -Value ($date + ' ' + $codes) -Encoding UTF8
        }
        catch {
            $errorFile = Join-Path $logDir 'health-notify-error.json'
            try {
                Write-AtomicUtf8 -Path $errorFile -Text ([ordered]@{ date = $date; code = 'NOTIFY_FAILED'; message = $_.Exception.Message; health_file = $healthFile; target_date = $report.target_date } | ConvertTo-Json -Depth 4)
            }
            catch { }
            [ordered]@{ status = [string]$report.status; target_date = [string]$report.target_date; health_file = $healthFile; tasks = @($tasks).Count; notified = $false; execution_status = $executionStatus; program_exit_code = $code; expected_program_exit_code = $expectedCode; execution_error_file = $executionErrorFile; anomalies = @($anomalies) } | ConvertTo-Json -Depth 5
            # A broken notification component is independent of the health verdict.
            exit 3
        }
    }

    [ordered]@{ status = [string]$report.status; target_date = [string]$report.target_date; health_file = $healthFile; tasks = @($tasks).Count; notified = $notified; execution_status = $executionStatus; program_exit_code = $code; expected_program_exit_code = $expectedCode; execution_error_file = $executionErrorFile; anomalies = @($anomalies) } | ConvertTo-Json -Depth 5
}
finally {
    # Remove only the temp file this script created.
    if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue }
}

# 4 = the program's own execution did not match its reported verdict; the report
#     itself is preserved, so this code is reserved for an execution contract break.
if ($null -eq $report -or $executionStatus -eq 'failed') { exit 4 }
# Native warnings and scheduler anomalies both require scheduler attention.
if ($notify) { exit 1 }
exit 0
