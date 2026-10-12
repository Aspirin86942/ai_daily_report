[CmdletBinding()]
param([string]$ProjectRoot = (Join-Path $PSScriptRoot '..'), [switch]$Apply)
. (Join-Path $PSScriptRoot 'daily_task_common.ps1')
$plan = Get-ReportTaskPlan -ProjectRoot $ProjectRoot
Remove-ReportDailyTask -Plan $plan -Apply:$Apply | ConvertTo-Json -Depth 6
