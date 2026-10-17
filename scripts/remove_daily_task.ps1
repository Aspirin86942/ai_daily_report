[CmdletBinding()]
param([string]$ProjectRoot = (Join-Path $PSScriptRoot '..'), [string]$ConfigDirectory, [ValidateSet("daily", "collect", "health")][string]$Kind = "daily", [switch]$Apply)
. (Join-Path $PSScriptRoot 'daily_task_common.ps1')
$plan = Get-ReportTaskPlan -ProjectRoot $ProjectRoot -ConfigDirectory $ConfigDirectory -Kind $Kind
Remove-ReportDailyTask -Plan $plan -Apply:$Apply | ConvertTo-Json -Depth 6
