[CmdletBinding()]
param([string]$ProjectRoot = (Join-Path $PSScriptRoot '..'), [string]$ConfigDirectory, [ValidateSet("daily", "collect", "parse", "health")][string]$Kind = "daily", [switch]$Apply)
. (Join-Path $PSScriptRoot 'daily_task_common.ps1')
$plan = Get-ReportTaskPlan -ProjectRoot $ProjectRoot -ConfigDirectory $ConfigDirectory -Kind $Kind
if ($Apply) {
    Assert-ReportTaskContext -Plan $plan
    if ($Kind -ne 'parse' -and -not (Test-Path -LiteralPath $plan.work_dir -PathType Container)) { throw 'TASK_WORK_DIRECTORY_UNAVAILABLE' }
    if ($Kind -in @('collect','parse')) {
        $verification = & $plan.program --config-dir $plan.config_dir archive verify-collection --format json | Out-String
        if ($LASTEXITCODE -ne 0 -or ($verification | ConvertFrom-Json).status -ne 'verified') { throw 'TASK_ARCHIVE_VERIFICATION_FAILED' }
    } elseif ($Kind -eq 'daily') {
        & $plan.program --config-dir $plan.config_dir doctor --strict | Out-Host
        if ($LASTEXITCODE -ne 0) { throw 'TASK_STRICT_DOCTOR_FAILED' }
    }
}
Install-ReportDailyTask -Plan $plan -Apply:$Apply | ConvertTo-Json -Depth 12
