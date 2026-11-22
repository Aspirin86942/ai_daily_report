[CmdletBinding(DefaultParameterSetName='Capture')]
param(
    [Parameter(Mandatory)][string]$BackupDirectory,
    [Parameter(Mandatory,ParameterSetName='Capture')][string]$PreviousRoot,
    [Parameter(Mandatory,ParameterSetName='Capture')][string]$CandidateRoot,
    [Parameter(Mandatory,ParameterSetName='Capture')][string]$ConfigDirectory,
    [Parameter(ParameterSetName='Capture')][string]$CandidateConfigDirectory,
    [Parameter(Mandatory,ParameterSetName='Restore')][switch]$Restore,
    [Parameter(ParameterSetName='Restore')][string]$VerifierDirectory,
    [Parameter(ParameterSetName='Restore')][switch]$Apply
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'daily_task_common.ps1')
. (Join-Path $PSScriptRoot 'release_common.ps1')

function Get-Hash([string]$Path) {
    Assert-ReleaseLocalPath $Path
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}
function Confirm-PreviousPackage([string]$Path) {
    $manifest = Get-Content -LiteralPath (Join-Path $Path 'manifest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $hashes = Get-VerifiedReleaseHashes $Path $manifest
    if (-not $hashes.ContainsKey('ai-daily-report.exe')) { throw 'Previous manifest omits the main program' }
}
function Get-TaskDefinition([string]$Xml) {
    $doc = [xml]$Xml
    $ns = [Xml.XmlNamespaceManager]::new($doc.NameTable)
    $ns.AddNamespace('t', $doc.DocumentElement.NamespaceURI)
    $actions = @($doc.SelectNodes('/t:Task/t:Actions/*', $ns))
    if ($actions.Count -ne 1 -or $actions[0].LocalName -cne 'Exec') { throw 'Expected one executable action' }
    $exec = $actions[0]
    $action = [ordered]@{ Execute=[string]$exec.Command; Argument=[string]$exec.Arguments; WorkingDirectory=[string]$exec.WorkingDirectory }
    $null = $exec.ParentNode.RemoveChild($exec)
    foreach ($xpath in @('/t:Task/t:RegistrationInfo/t:Date','/t:Task/t:Settings/t:Enabled')) {
        $node = $doc.SelectSingleNode($xpath,$ns)
        if ($node) { $null = $node.ParentNode.RemoveChild($node) }
    }
    return [pscustomobject]@{ Action=$action; StaticXml=$doc.OuterXml }
}
function Get-TaskSecurity($Plan) {
    return $scheduler.GetFolder($Plan.task_path).GetTask($Plan.task_name).GetSecurityDescriptor(7)
}
function Read-TaskXml($Plan) {
    return Export-ScheduledTask -TaskName $Plan.task_name -TaskPath $Plan.task_path -ErrorAction Stop
}

function Get-GuardPathArgument([string]$Path) {
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    if ($resolved -match '["\r\n]') { throw 'Invalid rollback path' }
    return '"' + ($resolved -replace '(\\+)$', '$1$1') + '"'
}
function Close-Guard($Process) {
    try {
        if (-not $Process.HasExited) {
            $Process.StandardInput.Close()
            if (-not $Process.WaitForExit(3000)) {
                $Process.Kill()
                if (-not $Process.WaitForExit(5000)) { throw 'Rollback helper did not terminate' }
            }
        }
    } finally { $Process.Dispose() }
}
function Start-RollbackProcess([string]$Exe,[string]$Arguments) {
    $start = [Diagnostics.ProcessStartInfo]::new($Exe, $Arguments)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.StandardOutputEncoding = [Text.Encoding]::UTF8
    $start.StandardErrorEncoding = [Text.Encoding]::UTF8
    # The task owner is the current SID. Resolve path overrides from that user's
    # persisted environment, excluding temporary overrides from the operator's shell.
    foreach ($name in @('PATHS','PATHS__DATA_DIR','PATHS__DB_DIR','PATHS__REPORTS_DIR','PATHS__WORK_DIR',
        'PATHS__LOG_DIR','REPORT','REPORT__SCOPE_ID','INSTALL_ROOT','DATA_DIR','DB_DIR','REPORTS_DIR','LOG_DIR')) {
        $key = 'DAILY_REPORT_' + $name
        $value = [Environment]::GetEnvironmentVariable($key,'User')
        if ($null -eq $value) { $value = [Environment]::GetEnvironmentVariable($key,'Machine') }
        if ($null -eq $value) { $start.EnvironmentVariables.Remove($key) }
        else { $start.EnvironmentVariables[$key] = $value }
    }
    return [Diagnostics.Process]::Start($start)
}
function Get-RollbackPlan([string]$Root,[string]$Config,[ValidateSet('daily','health')][string]$Kind) {
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $arguments = '--config-dir ' + (Get-GuardPathArgument $Config) + " task plan --sid $sid --kind $Kind"
    $process = Start-RollbackProcess (Join-Path $Root 'ai-daily-report.exe') $arguments
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(15000)) { throw 'Timed out reading the task plan' }
        if ($process.ExitCode -ne 0) { throw ("TASK_PLAN_FAILED " + $stderr.GetAwaiter().GetResult().Trim()) }
        return ($stdout.GetAwaiter().GetResult() | ConvertFrom-Json)
    } finally { Close-Guard $process }
}
function Open-Guard([string]$Exe,[string]$Root,[string]$Config,[string]$OldRoot,[string]$OldConfig,[switch]$IdentityOnly) {
    $arguments = '--config-dir ' + (Get-GuardPathArgument $Config) +
        ' task rollback-guard --previous-config-dir ' + (Get-GuardPathArgument $OldConfig) +
        ' --previous-root ' + (Get-GuardPathArgument $OldRoot) +
        ' --candidate-root ' + (Get-GuardPathArgument $Root)
    if ($IdentityOnly) { $arguments += ' --identity-only' }
    $process = Start-RollbackProcess $Exe $arguments
    try {
        $errors = $process.StandardError.ReadToEndAsync()
        $line = $process.StandardOutput.ReadLineAsync()
        if (-not $line.Wait(15000)) { throw 'Timed out waiting for the report lock and rollback check' }
        if (-not $line.Result) {
            $detail = if ($errors.Wait(2000)) { $errors.Result.Trim() } else { 'No diagnostic received' }
            throw "Rollback guard declined handoff. $detail"
        }
        $message = $line.Result | ConvertFrom-Json
        if ($message.target_sha256 -cnotmatch '^[0-9a-f]{64}$') { throw 'Invalid rollback target fingerprint' }
        if (-not $IdentityOnly -and ($message.ready -ne $true -or $process.HasExited)) { throw 'Rollback guard did not acquire the lock' }
        return [pscustomobject]@{ Process=$process; Target=$message.target_sha256 }
    } catch {
        Close-Guard $process
        throw
    }
}
function Assert-GuardRunning($Process) {
    if ($Process.HasExited) { throw 'Rollback guard exited before task readback completed' }
}

$folder = [IO.Path]::GetFullPath($BackupDirectory)
Assert-ReleaseLocalPath $folder
$receiptPath = Join-Path $folder 'task-rollback.json'
$scheduler = New-Object -ComObject Schedule.Service
$scheduler.Connect()
if (-not $Restore) {
    $old = (Resolve-Path -LiteralPath $PreviousRoot).Path
    $candidate = (Resolve-Path -LiteralPath $CandidateRoot).Path
    $config = (Resolve-Path -LiteralPath $ConfigDirectory).Path
    $nextConfig = if ($CandidateConfigDirectory) {
        (Resolve-Path -LiteralPath $CandidateConfigDirectory).Path
    } else { $config }
    if ($old -ieq $candidate -or (Test-Path -LiteralPath $folder)) { throw 'Capture requires distinct packages and a fresh backup directory' }
    $oldManifest = Get-Hash (Join-Path $old 'manifest.json')
    $oldProgram = Get-Hash (Join-Path $old 'ai-daily-report.exe')
    Confirm-PreviousPackage $old
    Confirm-PreviousPackage $candidate
    $candidateManifest = Get-Hash (Join-Path $candidate 'manifest.json')
    $candidateProgram = Get-Hash (Join-Path $candidate 'ai-daily-report.exe')
    $null = New-Item -Path $folder -ItemType Directory -ErrorAction Stop
    $guardBackup = Join-Path $folder 'rollback-guard.exe'
    Copy-Item -LiteralPath (Join-Path $candidate 'ai-daily-report.exe') -Destination $guardBackup
    if ((Get-Hash $guardBackup) -cne $candidateProgram) { throw 'Rollback helper copy failed verification' }
    $savedScripts = Join-Path $folder 'scripts'
    [IO.Directory]::CreateDirectory($savedScripts) | Out-Null
    $scriptHashes = [ordered]@{}
    foreach ($scriptName in @('kernel_task_rollback.ps1','daily_task_common.ps1','release_common.ps1')) {
        $original = Join-Path $PSScriptRoot $scriptName
        $copy = Join-Path $savedScripts $scriptName
        Copy-Item -LiteralPath $original -Destination $copy
        $scriptHashes[$scriptName] = Get-Hash $original
        if ((Get-Hash $copy) -cne $scriptHashes[$scriptName]) { throw 'Rollback script copy failed verification' }
    }
    $identity = Open-Guard $guardBackup $candidate $nextConfig $old $config -IdentityOnly
    try { $targetHash = $identity.Target } finally { Close-Guard $identity.Process }
    $rows = foreach ($kind in @('daily','health')) {
        $plan = Get-RollbackPlan $old $config $kind
        $next = Get-RollbackPlan $candidate $nextConfig $kind
        if ($plan.owner_marker -cne $next.owner_marker -or $plan.task_name -cne $next.task_name) { throw 'Candidate task owner or name changed' }
        $task = Get-ExistingReportTask $plan
        if (-not $task -or $task.State -eq 'Running') { throw 'Capture requires idle existing tasks' }
        $xml = Read-TaskXml $plan
        Assert-ReportTaskOwned $plan $xml
        if (@(Get-ReportTaskDifferences $plan $xml).Count) { throw 'Current task differs from its previous package' }
        $path = Join-Path $folder "$kind.xml"
        [IO.File]::WriteAllText($path,$xml,[Text.UTF8Encoding]::new($false))
        [ordered]@{ kind=$kind; name=$plan.task_name; xml_sha256=(Get-Hash $path);
            enabled=($task.State -ne 'Disabled'); security=(Get-TaskSecurity $plan);
            candidate_action=(Get-TaskDefinition $next.xml).Action }
    }
    Write-ReleaseJson -Path $receiptPath -Value ([ordered]@{
        schema=3; target_sha256=$targetHash; recovery_scripts=$scriptHashes;
        previous=$old; config=$config; candidate=$candidate; candidate_config=$nextConfig;
        candidate_manifest_sha256=$candidateManifest; candidate_exe_sha256=$candidateProgram;
        previous_manifest_sha256=$oldManifest; previous_exe_sha256=$oldProgram; tasks=@($rows)
    })
    Write-Output $receiptPath
    return
}

Assert-ReleaseLocalPath $receiptPath
$receipt = Get-Content -Raw -Encoding UTF8 -LiteralPath $receiptPath | ConvertFrom-Json
if ($receipt.schema -notin 1,2,3 -or @($receipt.tasks).Count -ne 2 -or
    (@($receipt.tasks.kind | Sort-Object) -join ',') -cne 'daily,health') { throw 'Invalid rollback receipt' }
# The v2 child holds the same OS lock as a daily run until this script closes its input.
# A read-only preflight by itself would allow a new model call during task replacement.
$guardBackup = Join-Path $folder 'rollback-guard.exe'
$guardRoot = $receipt.candidate
$guardConfig = if ($receipt.schema -eq 1) { $receipt.config } else { $receipt.candidate_config }
if ($receipt.schema -eq 3) {
    if ((Get-Hash $guardBackup) -cne $receipt.candidate_exe_sha256) { throw 'Stored rollback helper fingerprint changed' }
    if (-not (Test-Path -LiteralPath $guardRoot -PathType Container)) {
        $guardRoot = $receipt.previous
        $guardConfig = $receipt.config
    }
} else {
    # Earlier captures recorded the original package hash. Use the verifier shipped with this script.
    $toolRoot = if ($VerifierDirectory) { (Resolve-Path -LiteralPath $VerifierDirectory).Path } else { Split-Path -Parent $PSScriptRoot }
    $tool = Join-Path $toolRoot 'ai-daily-report.exe'
    if (Test-Path -LiteralPath $tool -PathType Leaf) {
        Confirm-PreviousPackage $toolRoot
        $guardBackup = $tool
    } elseif ($receipt.schema -eq 2) {
        $guardBackup = Join-Path $receipt.candidate 'ai-daily-report.exe'
        if ((Get-Hash $guardBackup) -cne $receipt.candidate_exe_sha256) { throw 'Candidate package fingerprint changed' }
    } else {
        throw 'Use the current v2 package or specify its -VerifierDirectory for this historical receipt'
    }
}
if ((Get-Hash (Join-Path $receipt.previous 'manifest.json')) -cne $receipt.previous_manifest_sha256 -or
    (Get-Hash (Join-Path $receipt.previous 'ai-daily-report.exe')) -cne $receipt.previous_exe_sha256) { throw 'Previous package fingerprint changed' }
Confirm-PreviousPackage $receipt.previous
$taskLock = $null
$guard = $null
try {
    $dailyPlan = Get-RollbackPlan $receipt.previous $receipt.config daily
    if ($Apply) { $taskLock = Enter-ReportTaskMaintenance $dailyPlan }
    $lease = Open-Guard $guardBackup $guardRoot $guardConfig $receipt.previous $receipt.config
    $guard = $lease.Process
    if ($receipt.schema -eq 3 -and $lease.Target -cne $receipt.target_sha256) { throw 'Rollback target changed since capture' }

    $actions = foreach ($row in $receipt.tasks) {
        $plan = if ($row.kind -ceq 'daily') { $dailyPlan } else { Get-RollbackPlan $receipt.previous $receipt.config $row.kind }
        $task = Get-ExistingReportTask $plan
        if (-not $task -or $task.State -eq 'Running' -or $plan.task_name -cne $row.name) { throw 'Task is running, missing, or renamed' }
        $path = Join-Path $folder ($row.kind + '.xml')
        if ((Get-Hash $path) -cne $row.xml_sha256) { throw 'Saved task XML fingerprint changed' }
        $before = Get-TaskDefinition (Get-Content -Raw -Encoding UTF8 -LiteralPath $path)
        $xml = Read-TaskXml $plan
        Assert-ReportTaskOwned $plan $xml
        $actual = Get-TaskDefinition $xml
        if ($actual.StaticXml -cne $before.StaticXml -or (Get-TaskSecurity $plan) -cne $row.security) { throw 'Task settings or permissions changed since capture' }
        $current = $actual.Action | ConvertTo-Json -Compress
        $known = @($before.Action, $row.candidate_action) | ForEach-Object { $_ | ConvertTo-Json -Compress }
        if ($known -cnotcontains $current) { throw 'Task action no longer points to either verified package' }
        [pscustomobject]@{ Plan=$plan; Previous=$before; Row=$row }
    }
    if (-not $Apply) { $actions | ForEach-Object { $_.Previous.Action } | ConvertTo-Json; return }

    $attempt = [guid]::NewGuid().ToString('N')
    $startedPath = Join-Path $folder ("restore-$attempt-started.json")
    Write-ReleaseJson $startedPath ([ordered]@{target=$lease.Target; receipt_sha256=(Get-Hash $receiptPath); tasks=@($receipt.tasks.name)})
    try {
        foreach ($item in $actions) {
            Disable-ScheduledTask -TaskName $item.Row.name -TaskPath $item.Plan.task_path | Out-Null
        }
        foreach ($item in $actions) {
            if ((Get-ExistingReportTask $item.Plan).State -eq 'Running') { throw 'A task started during rollback' }
            Assert-GuardRunning $guard
            $argsForAction = $item.Previous.Action
            $action = New-ScheduledTaskAction @argsForAction
            Set-ScheduledTask -TaskName $item.Row.name -TaskPath $item.Plan.task_path -Action $action | Out-Null
            $check = Get-TaskDefinition (Read-TaskXml $item.Plan)
            if ($check.StaticXml -cne $item.Previous.StaticXml -or
                ($check.Action | ConvertTo-Json -Compress) -cne ($item.Previous.Action | ConvertTo-Json -Compress) -or
                (Get-TaskSecurity $item.Plan) -cne $item.Row.security) { throw 'Restored task failed readback' }
        }
        foreach ($kind in @('health','daily')) {
            $item = $actions | Where-Object { $_.Row.kind -ceq $kind }
            Assert-GuardRunning $guard
            if ($item.Row.enabled) { Enable-ScheduledTask -TaskName $item.Row.name -TaskPath $item.Plan.task_path | Out-Null }
        }
        foreach ($item in $actions) {
            $last = Get-TaskDefinition (Read-TaskXml $item.Plan)
            if ($last.StaticXml -cne $item.Previous.StaticXml -or
                ($last.Action | ConvertTo-Json -Compress) -cne ($item.Previous.Action | ConvertTo-Json -Compress) -or
                (Get-TaskSecurity $item.Plan) -cne $item.Row.security) { throw 'Task changed during final rollback readback' }
            $enabled = (Get-ExistingReportTask $item.Plan).State -ne 'Disabled'
            Assert-GuardRunning $guard
            if ($enabled -ne $item.Row.enabled) { throw 'Restored task enablement differs' }
        }
    } catch {
        foreach ($item in $actions) { Disable-ScheduledTask -TaskName $item.Row.name -TaskPath $item.Plan.task_path -ErrorAction SilentlyContinue | Out-Null }
        throw
    }
    $result = [ordered]@{ restored=$true; package=$receipt.previous; tasks=@($receipt.tasks.name); attempt=$attempt }
    Write-ReleaseJson (Join-Path $folder ("restore-$attempt-completed.json")) $result
    $result | ConvertTo-Json
} finally {
    try { if ($guard) { Close-Guard $guard } }
    finally { if ($taskLock) { $taskLock.ReleaseMutex(); $taskLock.Dispose() } }
}
