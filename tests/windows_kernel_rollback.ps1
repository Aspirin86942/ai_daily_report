[CmdletBinding()]
param([Parameter(Mandatory)][string]$Program,[Parameter(Mandatory)][string]$OutputDirectory)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
. (Join-Path $repo 'scripts/daily_task_common.ps1')
$tool = Join-Path $repo 'scripts/kernel_task_rollback.ps1'
$root = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $root) { throw 'Use a fresh isolated output directory' }
$oldRoot = Join-Path $root 'previous'
$newRoot = Join-Path $root 'candidate'
$config = Join-Path $root 'config'
$backup = Join-Path $root 'rollback'
foreach ($directory in @($oldRoot,$newRoot,$config,(Join-Path $root 'work'))) {
    [IO.Directory]::CreateDirectory($directory) | Out-Null
}
foreach ($directory in @($oldRoot,$newRoot)) {
    $exe = Join-Path $directory 'ai-daily-report.exe'
    Copy-Item -LiteralPath $Program -Destination $exe
    $entry = @{path='ai-daily-report.exe';size=(Get-Item -LiteralPath $exe).Length;sha256=(Get-FileHash -LiteralPath $exe).Hash.ToLowerInvariant()}
    [IO.File]::WriteAllText((Join-Path $directory 'manifest.json'),(@{files=@($entry)} | ConvertTo-Json),[Text.UTF8Encoding]::new($false))
}
$settings = @{paths=@{work_dir=(Join-Path $root 'work');data_dir=(Join-Path $root 'data');db_dir=(Join-Path $root 'db');reports_dir=(Join-Path $root 'reports')}}
[IO.File]::WriteAllText((Join-Path $config 'settings.yaml'),($settings | ConvertTo-Json),[Text.UTF8Encoding]::new($false))

# These doubles shadow all Task Scheduler operations used by the rollback tool.
$global:kernelV2RollbackTestDefinitions = @{}
$global:kernelV2RollbackTestStates = @{}
$global:kernelV2RollbackTestFailOn = ''
$global:kernelV2RollbackTestEdits = 0
function New-Object([string]$ComObject) {
    if ($ComObject -cne 'Schedule.Service') { throw 'Unexpected COM object' }
    $service = [pscustomobject]@{}
    $service | Add-Member ScriptMethod Connect {}
    $service | Add-Member ScriptMethod GetFolder { param($path) return $this }
    $service | Add-Member ScriptMethod GetTask { param($name) return $this }
    $service | Add-Member ScriptMethod GetSecurityDescriptor { param($scope) return 'synthetic-descriptor' }
    return $service
}
function Get-ScheduledTask([string]$TaskPath) {
    foreach ($name in $global:kernelV2RollbackTestStates.Keys) { [pscustomobject]@{TaskName=$name;State=$global:kernelV2RollbackTestStates[$name]} }
}
function Export-ScheduledTask([string]$TaskPath,[string]$TaskName) { return $global:kernelV2RollbackTestDefinitions[$TaskName] }
function Disable-ScheduledTask([string]$TaskPath,[string]$TaskName) { $global:kernelV2RollbackTestStates[$TaskName]='Disabled' }
function Enable-ScheduledTask([string]$TaskPath,[string]$TaskName) { $global:kernelV2RollbackTestStates[$TaskName]='Ready' }
function New-ScheduledTaskAction([string]$Execute,[string]$Argument,[string]$WorkingDirectory) {
    return [pscustomobject]@{Execute=$Execute;Argument=$Argument;WorkingDirectory=$WorkingDirectory}
}
function Set-ScheduledTask([string]$TaskPath,[string]$TaskName,$Action) {
    if ($global:kernelV2RollbackTestFailOn -ceq $TaskName) { throw 'Injected action replacement failure' }
    $doc = [xml]$global:kernelV2RollbackTestDefinitions[$TaskName]
    $node = $doc.Task.Actions.Exec
    $node.Command=$Action.Execute
    $node.Arguments=$Action.Argument
    $node.WorkingDirectory=$Action.WorkingDirectory
    $global:kernelV2RollbackTestDefinitions[$TaskName]=$doc.OuterXml
    $global:kernelV2RollbackTestEdits++
}
# Initialize a real, isolated report database for the cross-version rollback lock.
& (Join-Path $newRoot 'ai-daily-report.exe') --config-dir $config list | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Could not initialize isolated report database' }
$previousPlans = @()
$candidatePlans = @()
foreach ($kind in @('daily','health')) {
    $plan = Get-ReportTaskPlan $oldRoot $config $kind
    $previousPlans += $plan
    $candidatePlans += Get-ReportTaskPlan $newRoot $config $kind
    $global:kernelV2RollbackTestDefinitions[$plan.task_name]=$plan.xml
    $global:kernelV2RollbackTestStates[$plan.task_name]='Ready'
}
& $tool -BackupDirectory $backup -PreviousRoot $oldRoot -CandidateRoot $newRoot -ConfigDirectory $config | Out-Null
foreach ($plan in $candidatePlans) {
    $oldDoc = [xml]$global:kernelV2RollbackTestDefinitions[$plan.task_name]
    $nextDoc = [xml]$plan.xml
    $null=$oldDoc.Task.Actions.ReplaceChild($oldDoc.ImportNode($nextDoc.Task.Actions.Exec,$true),$oldDoc.Task.Actions.Exec)
    $global:kernelV2RollbackTestDefinitions[$plan.task_name]=$oldDoc.OuterXml
}
foreach ($version in @(1,2)) {
    $legacy = Join-Path $root "receipt-v$version"
    [IO.Directory]::CreateDirectory($legacy) | Out-Null
    foreach ($kind in @('daily','health')) { Copy-Item -LiteralPath (Join-Path $backup "$kind.xml") -Destination $legacy }
    $receipt = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $backup 'task-rollback.json') | ConvertFrom-Json
    $receipt.schema = $version
    $fields = @('target_sha256','recovery_scripts')
    if ($version -eq 1) { $fields += @('candidate_config','candidate_exe_sha256','candidate_manifest_sha256') }
    foreach ($field in $fields) { $receipt.PSObject.Properties.Remove($field) }
    [IO.File]::WriteAllText((Join-Path $legacy 'task-rollback.json'),($receipt | ConvertTo-Json -Depth 16),[Text.UTF8Encoding]::new($false))
    & $tool -Restore -BackupDirectory $legacy -VerifierDirectory $oldRoot | Out-Null
}
# The captured kit must work after the entire generated candidate directory is taken offline.
$source = [IO.Path]::GetFullPath($newRoot)
$offline = [IO.Path]::GetFullPath((Join-Path $root 'offline-candidate'))
if ([IO.Path]::GetDirectoryName($source) -ine $root -or [IO.Path]::GetDirectoryName($offline) -ine $root) {
    throw 'Unexpected isolated package paths'
}
[IO.Directory]::Move($source,$offline)
$tool = Join-Path $backup 'scripts/kernel_task_rollback.ps1'
$overrideKey = 'DAILY_REPORT_PATHS__DB_DIR'
$oldOverride = [Environment]::GetEnvironmentVariable($overrideKey,'Process')
try {
    [Environment]::SetEnvironmentVariable($overrideKey,(Join-Path $root 'operator-shell-only'),'Process')
    & $tool -Restore -BackupDirectory $backup | Out-Null
} finally {
    if ($null -eq $oldOverride) { Remove-Item -LiteralPath ("Env:" + $overrideKey) -ErrorAction SilentlyContinue }
    else { [Environment]::SetEnvironmentVariable($overrideKey,$oldOverride,'Process') }
}
if ($global:kernelV2RollbackTestEdits) { throw 'Preview modified the scheduler' }
$global:kernelV2RollbackTestFailOn=$previousPlans[1].task_name
try { & $tool -Restore -Apply -BackupDirectory $backup | Out-Null; throw 'Expected simulated failure' } catch {
    if ($_.Exception.Message -notlike '*Injected action replacement failure*') { throw }
}
if (@($global:kernelV2RollbackTestStates.Values | Where-Object { $_ -ne 'Disabled' }).Count) { throw 'Partial restore left an enabled task' }
$global:kernelV2RollbackTestFailOn=''
& $tool -Restore -Apply -BackupDirectory $backup | Out-Null
foreach ($plan in $previousPlans) {
    if ($global:kernelV2RollbackTestStates[$plan.task_name] -ne 'Ready' -or @(Get-ReportTaskDifferences $plan $global:kernelV2RollbackTestDefinitions[$plan.task_name]).Count) { throw 'Original task did not recover' }
}
$priorEdits=$global:kernelV2RollbackTestEdits
[IO.File]::AppendAllText((Join-Path $backup 'daily.xml'),' ')
try { & $tool -Restore -Apply -BackupDirectory $backup | Out-Null; throw 'Expected corrupted backup rejection' } catch {
    if ($_.Exception.Message -notlike '*XML fingerprint changed*') { throw }
}
if ($global:kernelV2RollbackTestEdits -ne $priorEdits) { throw 'Corrupt receipt caused task modification' }
[ordered]@{status='passed'; real_task_writes=0; preview=$true; active_executable_unavailable=$true; partial_failure_disabled=$true; retry_restored=$true; corrupt_xml_rejected=$true} | ConvertTo-Json
