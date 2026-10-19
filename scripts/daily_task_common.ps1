Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-ReportTaskPlan {
    param([string]$ProjectRoot, [string]$ConfigDirectory, [ValidateSet("daily", "collect", "parse", "health")][string]$Kind = "daily")
    $root = (Resolve-Path -LiteralPath $ProjectRoot).Path
    $program = Join-Path $root 'ai-daily-report.exe'
    if (-not $ConfigDirectory) { $ConfigDirectory = Join-Path $root 'config' }
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    # A Windows GUI-subsystem executable needs a pipeline for PowerShell to
    # wait for completion and capture its output and exit code.
    $previousEncoding = [Console]::OutputEncoding
    try {
        [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
        $text = & $program --config-dir $ConfigDirectory task plan --sid $sid --kind $Kind | Out-String
    }
    finally { [Console]::OutputEncoding = $previousEncoding }
    if ($LASTEXITCODE -ne 0) { throw 'TASK_PLAN_FAILED' }
    return ($text | ConvertFrom-Json)
}

function Get-ReportTaskXmlFields {
    param([string]$Xml)
    [xml]$document = $Xml
    $ns = [Xml.XmlNamespaceManager]::new($document.NameTable)
    $ns.AddNamespace('t', 'http://schemas.microsoft.com/windows/2004/02/mit/task')
    $paths = @(
        '/t:Task/t:RegistrationInfo/t:Description', '/t:Task/t:Principals/t:Principal/t:UserId',
        '/t:Task/t:Principals/t:Principal/t:LogonType', '/t:Task/t:Principals/t:Principal/t:RunLevel',
        '/t:Task/t:Actions/t:Exec/t:Command', '/t:Task/t:Actions/t:Exec/t:Arguments',
        '/t:Task/t:Actions/t:Exec/t:WorkingDirectory', '/t:Task/t:Settings/t:MultipleInstancesPolicy',
        '/t:Task/t:Settings/t:DisallowStartIfOnBatteries', '/t:Task/t:Settings/t:StopIfGoingOnBatteries',
        '/t:Task/t:Settings/t:StartWhenAvailable', '/t:Task/t:Settings/t:WakeToRun',
        '/t:Task/t:Settings/t:Enabled',
        '/t:Task/t:Settings/t:AllowStartOnDemand', '/t:Task/t:Settings/t:ExecutionTimeLimit',
        '/t:Task/t:Settings/t:RestartOnFailure/t:Interval', '/t:Task/t:Settings/t:RestartOnFailure/t:Count',
        '/t:Task/t:Triggers/t:CalendarTrigger/t:Repetition/t:Interval',
        '/t:Task/t:Triggers/t:CalendarTrigger/t:Repetition/t:Duration',
        '/t:Task/t:Triggers/t:CalendarTrigger/t:Repetition/t:StopAtDurationEnd',
        '/t:Task/t:Triggers/t:LogonTrigger/t:Enabled', '/t:Task/t:Triggers/t:LogonTrigger/t:UserId',
        '/t:Task/t:Triggers/t:LogonTrigger/t:Delay'
    )
    $fields = [ordered]@{}
    # Windows omits XML elements whose values equal the Task Scheduler defaults.
    $defaults = @{
        '/t:Task/t:Principals/t:Principal/t:RunLevel' = 'LeastPrivilege'
        '/t:Task/t:Settings/t:WakeToRun' = 'false'
        '/t:Task/t:Settings/t:AllowStartOnDemand' = 'true'
        '/t:Task/t:Settings/t:Enabled' = 'true'
        '/t:Task/t:Triggers/t:CalendarTrigger/t:Repetition/t:StopAtDurationEnd' = 'false'
        '/t:Task/t:Triggers/t:LogonTrigger/t:Enabled' = 'true'
    }
    foreach ($path in $paths) {
        $nodes = @($document.SelectNodes($path, $ns))
        $value = if ($nodes.Count -eq 1) { $nodes[0].InnerText } else { '' }
        $parentPath = $path.Substring(0, $path.LastIndexOf('/'))
        if ($nodes.Count -eq 0 -and $defaults.ContainsKey($path) -and
            $null -ne $document.SelectSingleNode($parentPath, $ns)) {
            $value = $defaults[$path]
        }
        if ($path.EndsWith('/t:UserId') -and $value -and $value -notmatch '^S-\d-') {
            try {
                $value = [Security.Principal.NTAccount]::new($value).Translate([Security.Principal.SecurityIdentifier]).Value
            } catch [Security.Principal.IdentityNotMappedException] {
                # An unresolved account remains unequal; never accept it as the requested user.
            }
        }
        $fields[$path] = $value
    }
    $fields['action_count'] = @($document.SelectNodes('/t:Task/t:Actions/*', $ns)).Count
    $fields['trigger_count'] = @($document.SelectNodes('/t:Task/t:Triggers/*', $ns)).Count
    $calendar = @($document.SelectNodes('/t:Task/t:Triggers/t:CalendarTrigger', $ns))
    $fields['calendar_count'] = $calendar.Count
    $fields['calendar_times'] = (($calendar | ForEach-Object {
        $boundary = $_.SelectSingleNode('t:StartBoundary', $ns)
        if ($null -eq $boundary) { '' } else {
            ([DateTimeOffset]::Parse($boundary.InnerText)).ToOffset([TimeSpan]::FromHours(8)).ToString('HH:mm')
        }
    }) | Sort-Object) -join ';'
    $fields['calendar_days'] = (($calendar | ForEach-Object {
        $days = $_.SelectSingleNode('t:ScheduleByDay/t:DaysInterval', $ns)
        if ($null -eq $days -or $days.InnerText -eq '') { '1' } else { $days.InnerText }
    }) | Sort-Object) -join ';'
    $fields['calendar_dates'] = (($calendar | ForEach-Object {
        $boundary = $_.SelectSingleNode('t:StartBoundary', $ns)
        if ($null -eq $boundary) { '' } else {
            ([DateTimeOffset]::Parse($boundary.InnerText)).ToOffset([TimeSpan]::FromHours(8)).ToString('yyyy-MM-dd')
        }
    }) | Sort-Object -Unique) -join ';'
    $fields['calendar_enabled'] = if (@($calendar | Where-Object {
        $enabled = $_.SelectSingleNode('t:Enabled', $ns)
        $null -ne $enabled -and $enabled.InnerText -ne '' -and $enabled.InnerText -ne 'true'
    }).Count -eq 0) { 'true' } else { 'false' }
    # Keep each time bound to its own recurrence, weekday mask and limits.
    $fields['calendar_signatures'] = (($calendar | ForEach-Object {
        $trigger = $_
        $values = [ordered]@{}
        foreach ($path in @('StartBoundary','Enabled','EndBoundary','ExecutionTimeLimit','ScheduleByDay/DaysInterval','ScheduleByWeek/WeeksInterval','Repetition/Interval','Repetition/Duration','Repetition/StopAtDurationEnd')) {
            $xpath = (($path.Split('/') | ForEach-Object { 't:' + $_ }) -join '/')
            $node = $trigger.SelectSingleNode($xpath, $ns)
            $value = if ($null -eq $node) { '' } else { $node.InnerText }
            if ($path -eq 'StartBoundary' -and $value) { $value = ([DateTimeOffset]::Parse($value)).ToOffset([TimeSpan]::FromHours(8)).ToString('HH:mm:ss.fffffff') }
            if ($path -eq 'Enabled' -and -not $value) { $value = 'true' }
            if ($path -eq 'ScheduleByDay/DaysInterval' -and -not $value -and $trigger.SelectSingleNode('t:ScheduleByDay',$ns)) { $value = '1' }
            if ($path -eq 'ScheduleByWeek/WeeksInterval' -and -not $value -and $trigger.SelectSingleNode('t:ScheduleByWeek',$ns)) { $value = '1' }
            if ($path -eq 'Repetition/StopAtDurationEnd' -and -not $value -and $trigger.SelectSingleNode('t:Repetition',$ns)) { $value = 'false' }
            $values[$path] = $value
        }
        $values['recurrence'] = (@($trigger.ChildNodes | Where-Object { $_.LocalName.StartsWith('ScheduleBy') } | ForEach-Object { $_.LocalName }) | Sort-Object) -join ','
        $values['weekdays'] = (@($trigger.SelectNodes('t:ScheduleByWeek/t:DaysOfWeek/*',$ns) | ForEach-Object { $_.LocalName }) | Sort-Object) -join ','
        $values | ConvertTo-Json -Compress
    }) | Sort-Object) -join ';'
    return $fields
}

function Assert-ReportTaskOwned {
    param($Plan, [string]$Xml)
    $fields = Get-ReportTaskXmlFields -Xml $Xml
    if ($fields['/t:Task/t:RegistrationInfo/t:Description'] -cne $Plan.owner_marker) {
        throw 'TASK_NAME_BELONGS_TO_ANOTHER_PROJECT'
    }
}

function Get-ReportTaskDifferences {
    param($Plan, [string]$Xml)
    $actual = Get-ReportTaskXmlFields -Xml $Xml
    $expected = Get-ReportTaskXmlFields -Xml $Plan.xml
    $differences = @()
    foreach ($key in $expected.Keys) {
        if ($key -eq 'calendar_dates' -and $actual[$key] -and [string]$actual[$key] -cle [string]$expected[$key]) { continue }
        if ([string]$actual[$key] -cne [string]$expected[$key]) {
            $differences += [pscustomobject]@{ Field = $key; Actual = $actual[$key]; Expected = $expected[$key] }
        }
    }
    return $differences
}

function Get-ExistingReportTask {
    param($Plan)
    # Enumerating an exact task folder avoids suppressing access/service errors.
    $tasks = @(Get-ScheduledTask -TaskPath $Plan.task_path | Where-Object { $_.TaskName -ceq $Plan.task_name })
    if ($tasks.Count -gt 1) { throw 'TASK_IDENTITY_AMBIGUOUS' }
    if ($tasks.Count -eq 0) { return $null }
    return $tasks[0]
}

function Activate-ReportCollectionSchedule {
    param($Plan)
    $text = & $Plan.program --config-dir $Plan.config_dir archive activate-schedule | Out-String
    if ($LASTEXITCODE -ne 0) { throw 'COLLECTION_SCHEDULE_ACTIVATION_FAILED' }
    return ($text | ConvertFrom-Json)
}

function Install-ReportCollectionTask {
    param($Plan, [switch]$Apply)
    $mayDisable = $false
    try {
        $existing = Get-ExistingReportTask -Plan $Plan
        $oldXml = $null
        $differences = @()
        if ($null -ne $existing) {
            $oldXml = Export-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path
            Assert-ReportTaskOwned -Plan $Plan -Xml $oldXml
            $mayDisable = $true
            $differences = @(Get-ReportTaskDifferences -Plan $Plan -Xml $oldXml)
        }
        if (-not $Apply) {
            return [pscustomobject]@{Action=$(if ($null -eq $existing) {'create'} else {'update-or-reuse'});Applied=$false;TaskName=$Plan.task_name;Plan=$Plan;Differences=$differences}
        }
        $backup = $null
        if ($oldXml) {
            $backupDirectory = Join-Path $Plan.data_dir 'task_backups'
            [IO.Directory]::CreateDirectory($backupDirectory) | Out-Null
            $backup = Join-Path $backupDirectory ('task-' + [Guid]::NewGuid().ToString('N') + '.xml')
            [IO.File]::WriteAllText($backup, $oldXml, [Text.UTF8Encoding]::new($false))
        }
        $staged = $Plan.PSObject.Copy()
        [xml]$document = $Plan.xml
        $ns = [Xml.XmlNamespaceManager]::new($document.NameTable)
        $ns.AddNamespace('t','http://schemas.microsoft.com/windows/2004/02/mit/task')
        $settings = $document.SelectSingleNode('/t:Task/t:Settings',$ns)
        $enabled = $settings.SelectSingleNode('t:Enabled',$ns)
        if ($null -eq $enabled) { $enabled = $document.CreateElement('Enabled',$settings.NamespaceURI); $settings.AppendChild($enabled) | Out-Null }
        $enabled.InnerText = 'false'
        $staged.xml = $document.OuterXml
        $changed = $null -eq $existing -or $differences.Count -gt 0
        $mayDisable = $true
        if ($changed) {
            Register-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path -Xml $staged.xml -Force | Out-Null
        } else {
            Disable-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path | Out-Null
        }
        $readback = Export-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path
        if (@(Get-ReportTaskDifferences -Plan $staged -Xml $readback).Count -ne 0) { throw 'TASK_READBACK_MISMATCH_DISABLED' }
        $receipt = Activate-ReportCollectionSchedule -Plan $Plan
        if ($receipt.status -notin @('activated','unchanged') -or $receipt.interval_minutes -ne $Plan.interval_minutes) { throw 'COLLECTION_SCHEDULE_RECEIPT_MISMATCH' }
        $expectedWindow = $Plan.collection_workday_window
        $actualWindow = $receipt.collection_workday_window
        if ($null -eq $expectedWindow) {
            if ($null -ne $actualWindow) { throw 'COLLECTION_SCHEDULE_RECEIPT_MISMATCH' }
        } elseif ($null -eq $actualWindow -or $actualWindow.start -cne $expectedWindow.start -or $actualWindow.end -cne $expectedWindow.end) {
            throw 'COLLECTION_SCHEDULE_RECEIPT_MISMATCH'
        }
        Enable-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path | Out-Null
        $readback = Export-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path
        if (@(Get-ReportTaskDifferences -Plan $Plan -Xml $readback).Count -ne 0) { throw 'TASK_READBACK_MISMATCH_DISABLED' }
        return [pscustomobject]@{Action=$(if ($changed) {'registered'} else {'unchanged'});Applied=$true;TaskName=$Plan.task_name;Backup=$backup;ScheduleReceipt=$receipt}
    } catch {
        $failure = $_
        if ($Apply -and $mayDisable) {
            try { Disable-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path | Out-Null } catch { Write-Warning 'Collector could not be disabled after installation failure' }
        }
        throw $failure
    }
}

function Install-ReportDailyTask {
    param($Plan, [switch]$Apply)
    if ($Plan.PSObject.Properties.Name -contains 'kind' -and $Plan.kind -eq 'collect') {
        return Install-ReportCollectionTask -Plan $Plan -Apply:$Apply
    }
    $existing = Get-ExistingReportTask -Plan $Plan
    $oldXml = $null
    $differences = @()
    if ($null -ne $existing) {
        $oldXml = Export-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path
        Assert-ReportTaskOwned -Plan $Plan -Xml $oldXml
        $differences = @(Get-ReportTaskDifferences -Plan $Plan -Xml $oldXml)
    }
    if (-not $Apply) {
        return [pscustomobject]@{ Action = $(if ($null -eq $existing) { 'create' } else { 'update-or-reuse' });
            Applied = $false; TaskName = $Plan.task_name; Plan = $Plan; Differences = $differences }
    }
    $collect = $Plan.PSObject.Properties.Name -contains 'kind' -and $Plan.kind -eq 'collect'
    $health = $Plan.PSObject.Properties.Name -contains 'kind' -and $Plan.kind -eq 'health'
    $parse = $Plan.PSObject.Properties.Name -contains 'kind' -and $Plan.kind -eq 'parse'
    if (-not $collect -and -not $health -and -not $parse) {
        if (-not $Plan.credential_present) { throw 'TASK_CREDENTIAL_NOT_VISIBLE' }
        if (-not $Plan.capabilities_configured -or -not $Plan.model_check_verified) { throw 'TASK_MODEL_ACCEPTANCE_REQUIRED' }
    }
    if ($null -ne $existing -and $differences.Count -eq 0) {
        return [pscustomobject]@{ Action = 'unchanged'; Applied = $false; TaskName = $Plan.task_name }
    }
    $backup = $null
    if ($oldXml) {
        $backupDirectory = Join-Path $Plan.data_dir 'task_backups'
        [IO.Directory]::CreateDirectory($backupDirectory) | Out-Null
        $backup = Join-Path $backupDirectory ('task-' + [Guid]::NewGuid().ToString('N') + '.xml')
        [IO.File]::WriteAllText($backup, $oldXml, [Text.UTF8Encoding]::new($false))
    }
    Register-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path -Xml $Plan.xml -Force | Out-Null
    $readback = Export-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path
    $remaining = @(Get-ReportTaskDifferences -Plan $Plan -Xml $readback)
    if ($remaining.Count -ne 0) {
        Disable-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path | Out-Null
        throw 'TASK_READBACK_MISMATCH_DISABLED'
    }
    return [pscustomobject]@{ Action = 'registered'; Applied = $true; TaskName = $Plan.task_name; Backup = $backup }
}

function Remove-ReportDailyTask {
    param($Plan, [switch]$Apply)
    $existing = Get-ExistingReportTask -Plan $Plan
    if ($null -eq $existing) { return [pscustomobject]@{ Action = 'absent'; Applied = $false } }
    $xml = Export-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path
    Assert-ReportTaskOwned -Plan $Plan -Xml $xml
    if ($Apply) {
        Unregister-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path -Confirm:$false
    }
    return [pscustomobject]@{ Action = 'remove-task-only'; Applied = [bool]$Apply; TaskName = $Plan.task_name }
}
