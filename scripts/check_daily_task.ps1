[CmdletBinding()]
param([string]$ProjectRoot = (Join-Path $PSScriptRoot '..'), [string]$ConfigDirectory)
. (Join-Path $PSScriptRoot 'daily_task_common.ps1')
$plan = Get-ReportTaskPlan -ProjectRoot $ProjectRoot -ConfigDirectory $ConfigDirectory
$task = Get-ExistingReportTask -Plan $plan
if ($null -eq $task) {
    [pscustomobject]@{ Exists = $false; TaskName = $plan.task_name } | ConvertTo-Json
    exit 1
}
$xml = Export-ScheduledTask -TaskName $plan.task_name -TaskPath $plan.task_path
Assert-ReportTaskOwned -Plan $plan -Xml $xml
$differences = @(Get-ReportTaskDifferences -Plan $plan -Xml $xml)
$info = Get-ScheduledTaskInfo -TaskName $plan.task_name -TaskPath $plan.task_path
$zone = [TimeZoneInfo]::FindSystemTimeZoneById('China Standard Time')
[pscustomobject]@{
    Exists = $true; TaskName = $plan.task_name; State = [string]$task.State;
    MatchesPlan = ($differences.Count -eq 0); Differences = $differences;
    NextRunShanghai = [TimeZoneInfo]::ConvertTime($info.NextRunTime, $zone).ToString('yyyy-MM-dd HH:mm:ss');
    LastRunShanghai = [TimeZoneInfo]::ConvertTime($info.LastRunTime, $zone).ToString('yyyy-MM-dd HH:mm:ss');
    LastTaskResult = $info.LastTaskResult; MissedRuns = $info.NumberOfMissedRuns;
    CredentialVisibleInInspector = $plan.credential_present
} | ConvertTo-Json -Depth 10
if ($differences.Count -ne 0) { exit 1 }
