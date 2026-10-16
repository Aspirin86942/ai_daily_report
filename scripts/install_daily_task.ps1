[CmdletBinding()]
param([string]$ProjectRoot = (Join-Path $PSScriptRoot '..'), [string]$ConfigDirectory, [switch]$Apply)
. (Join-Path $PSScriptRoot 'daily_task_common.ps1')
$plan = Get-ReportTaskPlan -ProjectRoot $ProjectRoot -ConfigDirectory $ConfigDirectory
if ($Apply) {
    if (-not (Test-Path -LiteralPath $plan.work_dir -PathType Container)) { throw 'TASK_WORK_DIRECTORY_UNAVAILABLE' }
    & $plan.program --config-dir $plan.config_dir doctor --strict | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'TASK_STRICT_DOCTOR_FAILED' }
}
Install-ReportDailyTask -Plan $plan -Apply:$Apply | ConvertTo-Json -Depth 12
