[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Program,
    [Parameter(Mandatory=$true)][string]$OutputDirectory
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
. (Join-Path $repo 'scripts/daily_task_common.ps1')
if (Test-Path -LiteralPath $OutputDirectory) { throw 'Use a fresh isolated output directory' }
New-Item -ItemType Directory -Path (Join-Path $OutputDirectory 'config') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $OutputDirectory 'work') -Force | Out-Null
$config = [ordered]@{paths=@{work_dir=(Join-Path $OutputDirectory 'work');data_dir=(Join-Path $OutputDirectory 'data');db_dir=(Join-Path $OutputDirectory 'data/db');reports_dir=(Join-Path $OutputDirectory 'data/reports')};archive=@{collection_interval_minutes=120};schedule=@{task_name='Isolated collector window test'}}
$config | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'config/settings.yaml') -Encoding utf8
$plan = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory (Join-Path $OutputDirectory 'config') -Kind collect
if ($plan.PSObject.Properties.Name -notcontains 'collection_workday_window') { $plan | Add-Member -NotePropertyName collection_workday_window -NotePropertyValue $null }
function Assert-Test([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Set-TaskEnabled([string]$Xml, [bool]$Enabled) {
    [xml]$doc = $Xml
    $ns = [Xml.XmlNamespaceManager]::new($doc.NameTable)
    $ns.AddNamespace('t','http://schemas.microsoft.com/windows/2004/02/mit/task')
    $settings = $doc.SelectSingleNode('/t:Task/t:Settings',$ns)
    $node = $settings.SelectSingleNode('t:Enabled',$ns)
    if ($null -eq $node) { $node = $doc.CreateElement('Enabled',$settings.NamespaceURI); $settings.AppendChild($node) | Out-Null }
    $node.InnerText = $Enabled.ToString().ToLowerInvariant()
    $doc.OuterXml
}
function Reset-TestState {
    $script:taskXml = ''
    $script:registrationEnabled = @()
    $script:activationObservedDisabled = $false
    $script:activationCount = 0
    $script:disabledCount = 0
    $script:registerCount = 0
    $script:failureMode = ''
}
function Get-ScheduledTask { param($TaskPath) if ($script:taskXml) { [pscustomobject]@{TaskName=$plan.task_name} } }
function Export-ScheduledTask {
    param($TaskName,$TaskPath)
    if ($script:failureMode -eq 'export' -and $script:registerCount -gt 0) { throw 'synthetic export failure' }
    if ($script:failureMode -eq 'mismatch' -and $script:registerCount -gt 0) { return $script:taskXml.Replace('IgnoreNew','Parallel') }
    $script:taskXml
}
function Register-ScheduledTask {
    param($TaskName,$TaskPath,$Xml,[switch]$Force)
    $script:registerCount++
    $script:taskXml = $Xml
    $script:registrationEnabled += (Get-ReportTaskXmlFields -Xml $Xml)['/t:Task/t:Settings/t:Enabled']
    if ($script:failureMode -eq 'register') { throw 'synthetic register failure after write' }
}
function Disable-ScheduledTask {
    param($TaskName,$TaskPath)
    $script:disabledCount++
    if ($script:taskXml) { $script:taskXml = Set-TaskEnabled $script:taskXml $false }
}
function Enable-ScheduledTask {
    param($TaskName,$TaskPath)
    if ($script:failureMode -eq 'enable') { throw 'synthetic enable failure' }
    $script:taskXml = Set-TaskEnabled $script:taskXml $true
}
# Replace only the native policy activation boundary. The installer and XML comparison run unchanged.
function Activate-ReportCollectionSchedule {
    param($Plan)
    $script:activationCount++
    $script:activationObservedDisabled = (Get-ReportTaskXmlFields -Xml $script:taskXml)['/t:Task/t:Settings/t:Enabled'] -eq 'false'
    if ($script:failureMode -eq 'activate') { throw 'synthetic collector lock conflict' }
    $minutes = if ($script:failureMode -eq 'receipt') { 30 } else { 120 }
    [pscustomobject]@{status='unchanged';interval_minutes=$minutes;collection_workday_window=$null;effective_at='2026-10-19T01:00:00+00:00'}
}
Reset-TestState
$result = Install-ReportDailyTask -Plan $plan -Apply
Assert-Test ($script:registrationEnabled.Count -eq 1 -and $script:registrationEnabled[0] -eq 'false') 'Collector must be registered disabled before activation'
Assert-Test $script:activationObservedDisabled 'Collector must stay disabled while policy history is activated'
Assert-Test ($script:activationCount -eq 1) 'Collector installation did not activate policy history'
Assert-Test ((Get-ReportTaskXmlFields -Xml $script:taskXml)['/t:Task/t:Settings/t:Enabled'] -eq 'true') 'Collector was not enabled after verified activation'
Assert-Test ($result.ScheduleReceipt.interval_minutes -eq 120) 'Installer did not expose the verified policy receipt'
# An unchanged task still needs activation: a crash may have left history behind its XML.
$script:registrationEnabled = @()
$script:activationCount = 0
$result = Install-ReportDailyTask -Plan $plan -Apply
Assert-Test ($result.Action -eq 'unchanged' -and $script:activationCount -eq 1 -and $script:activationObservedDisabled) 'Unchanged collector skipped safe activation'
Assert-Test ((Get-ReportTaskXmlFields -Xml $script:taskXml)['/t:Task/t:Settings/t:Enabled'] -eq 'true') 'Unchanged collector did not return to enabled'
Reset-TestState
$preview = Install-ReportDailyTask -Plan $plan
Assert-Test (-not $preview.Applied -and $script:registerCount -eq 0 -and $script:activationCount -eq 0) 'Preview changed task or policy history'
foreach ($mode in @('register','export','mismatch','activate','receipt','enable')) {
    Reset-TestState
    $script:taskXml = $plan.xml
    # A changed action forces the real update branch, while ownership stays intact.
    $script:taskXml = $script:taskXml.Replace('collect --scheduled','collect --scheduled --format text')
    $script:failureMode = $mode
    $failed = $false
    try { Install-ReportDailyTask -Plan $plan -Apply | Out-Null } catch { $failed = $true }
    Assert-Test $failed ("Failure was hidden: $mode")
    Assert-Test ($script:disabledCount -gt 0) ("Collector was not stopped on failure: $mode")
    Assert-Test ((Get-ReportTaskXmlFields -Xml $script:taskXml)['/t:Task/t:Settings/t:Enabled'] -eq 'false') ("Collector remained enabled after failure: $mode")
    Assert-Test (@(Get-ChildItem -LiteralPath (Join-Path $plan.data_dir 'task_backups') -File).Count -gt 0) ("Task backup missing on failure: $mode")
}
# Bind each calendar time to its own weekday mask. Independent field sets would miss this swap.
$weekdays = '<Monday/><Tuesday/><Wednesday/><Thursday/><Friday/>'
$triggers = foreach ($time in @('09:00','11:00','13:00','15:00','17:00','18:00')) { '<CalendarTrigger><StartBoundary>2026-10-19T' + $time + ':00+08:00</StartBoundary><Enabled>true</Enabled><ScheduleByWeek><WeeksInterval>1</WeeksInterval><DaysOfWeek>' + $weekdays + '</DaysOfWeek></ScheduleByWeek></CalendarTrigger>' }
[xml]$weeklyDoc = $plan.xml
$ns = [Xml.XmlNamespaceManager]::new($weeklyDoc.NameTable)
$ns.AddNamespace('t','http://schemas.microsoft.com/windows/2004/02/mit/task')
$weeklyDoc.SelectSingleNode('/t:Task/t:Triggers',$ns).InnerXml = ($triggers -join '')
$weeklyPlan = $plan.PSObject.Copy()
$weeklyPlan.xml = $weeklyDoc.OuterXml
$wrongDay = $weeklyPlan.xml.Replace('<Monday />','<Saturday />').Replace('<Monday/>','<Saturday/>')
Assert-Test (@(Get-ReportTaskDifferences -Plan $weeklyPlan -Xml $wrongDay).Count -gt 0) 'Weekend weekday mask was accepted'
$wrongWeek = $weeklyPlan.xml.Replace('<WeeksInterval>1</WeeksInterval>','<WeeksInterval>2</WeeksInterval>')
Assert-Test (@(Get-ReportTaskDifferences -Plan $weeklyPlan -Xml $wrongWeek).Count -gt 0) 'Two-week trigger interval was accepted'
$duplicateTick = $weeklyPlan.xml.Replace('T17:00:00+08:00','T18:00:00+08:00')
Assert-Test (@(Get-ReportTaskDifferences -Plan $weeklyPlan -Xml $duplicateTick).Count -gt 0) 'Duplicate tick replaced a required collection time'
[xml]$mixed = $weeklyPlan.xml
$mixedNs = [Xml.XmlNamespaceManager]::new($mixed.NameTable)
$mixedNs.AddNamespace('t','http://schemas.microsoft.com/windows/2004/02/mit/task')
$masks = $mixed.SelectNodes('/t:Task/t:Triggers/t:CalendarTrigger/t:ScheduleByWeek/t:DaysOfWeek',$mixedNs)
$masks[0].InnerXml = '<Monday xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"/>'
$masks[1].InnerXml = '<Tuesday xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"/>'
$bindingPlan = $weeklyPlan.PSObject.Copy()
$bindingPlan.xml = $mixed.OuterXml
$savedMask = $masks[0].InnerXml
$masks[0].InnerXml = $masks[1].InnerXml
$masks[1].InnerXml = $savedMask
Assert-Test (@(Get-ReportTaskDifferences -Plan $bindingPlan -Xml $mixed.OuterXml).Count -gt 0) 'Weekday masks swapped between two times were accepted'
$normalized = $weeklyPlan.xml -replace '<Enabled>true</Enabled>',''
Assert-Test (@(Get-ReportTaskDifferences -Plan $weeklyPlan -Xml $normalized).Count -eq 0) 'Windows omission of enabled defaults was rejected'
[pscustomobject]@{status='passed';real_tasks_changed=0;policy_store_changed=0;failure_modes=6;atomic_installation=$true;unchanged_install_activates=$true;weekly_trigger_identity_verified=$true} | ConvertTo-Json | Tee-Object -FilePath (Join-Path $OutputDirectory 'result.json')
