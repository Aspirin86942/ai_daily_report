[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Program,
    [Parameter(Mandatory = $true)][string]$ConfigDirectory,
    [switch]$NoNotify
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$text = & $Program --config-dir $ConfigDirectory health --scheduled | Out-String
$report = $text | ConvertFrom-Json
if ($null -eq $report -or -not ($report.PSObject.Properties.Name -contains 'status')) {
    throw 'HEALTH_OUTPUT_INVALID'
}
$zone = [TimeZoneInfo]::FindSystemTimeZoneById('China Standard Time')
$names = @($report.task_names.daily, $report.task_names.collect, $report.task_names.health)
$tasks = @()
foreach ($name in $names) {
    $task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
    if ($null -eq $task) {
        $tasks += [pscustomobject]@{ name = $name; exists = $false; state = ''; last_result = $null; next_run_shanghai = '' }
        continue
    }
    $state = ''
    if ($task.PSObject.Properties.Name -contains 'State') { $state = [string]$task.State }
    $info = Get-ScheduledTaskInfo -TaskName $name -ErrorAction SilentlyContinue
    $last = $null
    $next = ''
    if ($null -ne $info) {
        if ($info.PSObject.Properties.Name -contains 'LastTaskResult') { $last = $info.LastTaskResult }
        if ($info.PSObject.Properties.Name -contains 'NextRunTime' -and $info.NextRunTime.Year -gt 2000) {
            $next = [TimeZoneInfo]::ConvertTime($info.NextRunTime, $zone).ToString('yyyy-MM-dd HH:mm:ss')
        }
    }
    $tasks += [pscustomobject]@{ name = $name; exists = $true; state = $state; last_result = $last; next_run_shanghai = $next }
}
$report | Add-Member -NotePropertyName tasks -NotePropertyValue $tasks

$date = Get-Date -Format 'yyyy-MM-dd'
$logDir = [string]$report.log_dir
[IO.Directory]::CreateDirectory($logDir) | Out-Null
$healthFile = Join-Path $logDir ('health-' + $date + '.json')
$report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $healthFile -Encoding UTF8

$failing = @($report.checks | Where-Object { $_.level -ne 'ok' })
$missing = @($tasks | Where-Object { -not $_.exists })
$badResult = @($tasks | Where-Object { $_.exists -and $null -ne $_.last_result -and $_.last_result -ne 0 })
$notify = ($report.status -ne 'healthy') -or ($missing.Count -gt 0) -or ($badResult.Count -gt 0)
$codes = (($failing | ForEach-Object { $_.code }) + ($missing | ForEach-Object { 'TASK_MISSING' }) | Sort-Object -Unique) -join ','
$stateFile = Join-Path $logDir 'health-notify-state.json'
$already = $false
if (Test-Path -LiteralPath $stateFile) {
    $state = Get-Content -LiteralPath $stateFile -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($state.date -eq $date -and $state.codes -eq $codes) { $already = $true }
}
if ($notify -and -not $already) {
    $messages = @($failing | ForEach-Object { $_.message })
    if ($missing.Count -gt 0) { $messages += ('Missing tasks: ' + (($missing | ForEach-Object { $_.name }) -join ', ')) }
    $summary = ($messages -join '; ')
    if ($summary.Length -gt 400) { $summary = $summary.Substring(0, 400) + '...' }
    if (-not $NoNotify) {
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        $icon = New-Object System.Windows.Forms.NotifyIcon
        $icon.Icon = [System.Drawing.SystemIcons]::Warning
        $icon.Visible = $true
        $icon.ShowBalloonTip(10000, 'AI Daily Report health alert', ('Target ' + $report.target_date + ': ' + $summary), [System.Windows.Forms.ToolTipIcon]::Warning)
        Start-Sleep -Seconds 6
        $icon.Dispose()
    }
    [ordered]@{ date = $date; codes = $codes; status = $report.status; target_date = $report.target_date } | ConvertTo-Json | Set-Content -LiteralPath $stateFile -Encoding UTF8
    Add-Content -LiteralPath (Join-Path $logDir 'health-notify.log') -Value ($date + ' ' + $codes)
}
if ($notify) { exit 1 }
exit 0
