[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('company', 'openai')][string]$Service,
    [string]$ConfigDirectory,
    [string]$Program,
    [switch]$Apply,
    [switch]$CheckModel
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($CheckModel -and -not $Apply) { throw 'CheckModel requires Apply; the probe makes an audited model request.' }
if (-not $Program -or -not $ConfigDirectory) {
    $task = Get-ScheduledTask -TaskName 'AI Daily Report' -TaskPath '\'
    if (@($task.Actions).Count -ne 1) { throw 'TASK_ACTION_AMBIGUOUS' }
    if (-not $Program) { $Program = [string]$task.Actions[0].Execute }
    if (-not $ConfigDirectory) {
        if ([string]$task.Actions[0].Arguments -notmatch '--config-dir\s+"([^"]+)"') { throw 'TASK_CONFIG_UNVERIFIABLE' }
        $ConfigDirectory = $Matches[1]
    }
}
$Program = (Resolve-Path -LiteralPath $Program).Path
$ConfigDirectory = (Resolve-Path -LiteralPath $ConfigDirectory).Path
. (Join-Path $PSScriptRoot 'daily_task_common.ps1')
$profiles = Get-Content -LiteralPath (Join-Path $ConfigDirectory 'model-services.json') -Raw -Encoding UTF8 | ConvertFrom-Json
$selected = $profiles.PSObject.Properties[$Service]
if ($null -eq $selected) { throw 'MODEL_SERVICE_NOT_CONFIGURED' }
$selected = $selected.Value
if (@($selected.PSObject.Properties.Name | Where-Object { $_ -notin @('llm', 'proxy') }).Count) { throw 'MODEL_SERVICE_FIELDS_INVALID' }
if (-not $selected.llm.provider -or -not $selected.llm.model_id -or -not $selected.llm.base_url) { throw 'MODEL_SERVICE_INCOMPLETE' }

function Write-ModelSettings([string]$Path, [byte[]]$Bytes) {
    $temporary = $Path + '.' + [Guid]::NewGuid().ToString('N') + '.tmp'
    try {
        [IO.File]::WriteAllBytes($temporary, $Bytes)
        if ([IO.File]::Exists($Path)) { [IO.File]::Replace($temporary, $Path, [NullString]::Value) }
        else { [IO.File]::Move($temporary, $Path) }
    } finally {
        if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) }
    }
}

function Get-ModelSettingsHash([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $algorithm = [Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($algorithm.ComputeHash($stream)).Replace('-', '') }
    finally { $algorithm.Dispose(); $stream.Dispose() }
}

$overlayPath = Join-Path $ConfigDirectory 'settings.windows.yaml'
$previousBytes = $null
$previousHash = $null
$overlay = [pscustomobject]@{}
if (Test-Path -LiteralPath $overlayPath) {
    $previousBytes = [IO.File]::ReadAllBytes($overlayPath)
    $previousHash = Get-ModelSettingsHash $overlayPath
    try { $overlay = [Text.Encoding]::UTF8.GetString($previousBytes).TrimStart([char]0xFEFF) | ConvertFrom-Json }
    catch { throw 'MODEL_OVERLAY_MUST_BE_JSON: preserve the existing YAML overlay and convert it to JSON before using this switcher.' }
}
foreach ($group in @('llm', 'proxy')) {
    if (-not $selected.PSObject.Properties[$group]) { throw 'MODEL_SERVICE_INCOMPLETE' }
    $overlay | Add-Member -NotePropertyName $group -NotePropertyValue $selected.$group -Force
}
$bytes = [Text.UTF8Encoding]::new($false).GetBytes(($overlay | ConvertTo-Json -Depth 20) + "`n")
$savedEnvironment = @{}
$previousEncoding = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
    foreach ($group in @('llm', 'proxy')) {
        foreach ($property in $selected.$group.PSObject.Properties) {
            if ($property.Name -notmatch '^[a-z][a-z0-9_]*$' -or $property.Name -match 'api_key|secret|credential') { throw 'MODEL_SERVICE_FIELDS_INVALID' }
            $name = 'DAILY_REPORT_' + $group.ToUpperInvariant() + '__' + $property.Name.ToUpperInvariant()
            $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
            # Windows PowerShell 5.1 emits nothing for a standalone null.
            $value = if ($null -eq $property.Value) { 'null' } else { ConvertTo-Json -InputObject $property.Value -Depth 20 -Compress }
            [Environment]::SetEnvironmentVariable($name, $value, 'Process')
        }
    }
    $plan = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory $ConfigDirectory -Kind daily
    if (-not $plan.PSObject.Properties['config_fingerprint']) { throw 'MODEL_SERVICE_RUNTIME_TOO_OLD' }
    if ($Apply) {
        if (-not $plan.credential_present) {
            $keyName = if ($plan.llm.provider -eq 'openai') { 'OPENAI_API_KEY' } else { 'DEEPSEEK_API_KEY' }
            throw ('MODEL_CREDENTIAL_MISSING: configure ' + $keyName + ' locally; do not paste it into chat.')
        }
        foreach ($name in @($plan.task_name, ($plan.task_name + ' Health'))) {
            $registered = @(Get-ScheduledTask -TaskPath $plan.task_path | Where-Object TaskName -eq $name)
            if (@($registered | Where-Object { [string]$_.State -eq 'Running' }).Count) { throw 'TASK_IS_RUNNING' }
        }
        Assert-ReportTaskContext -Plan $plan
        $doctorArguments = @('--config-dir', $ConfigDirectory, 'doctor', '--strict')
        if ($CheckModel) { $doctorArguments += '--check-model' }
        $doctorText = & $Program @doctorArguments | Out-String
        if ($LASTEXITCODE -ne 0) { throw 'MODEL_SERVICE_DOCTOR_FAILED' }
        $doctor = $doctorText | ConvertFrom-Json
        $plan = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory $ConfigDirectory -Kind daily
        if (-not $plan.model_check_verified) { throw 'MODEL_SERVICE_PROBE_REQUIRED: use Apply with CheckModel once to verify this connection.' }
    }
} finally {
    foreach ($name in $savedEnvironment.Keys) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
    [Console]::OutputEncoding = $previousEncoding
}

$backup = $null
if ($Apply) {
    $currentHash = if (Test-Path -LiteralPath $overlayPath) { Get-ModelSettingsHash $overlayPath } else { $null }
    if ($currentHash -cne $previousHash) { throw 'MODEL_OVERLAY_CHANGED_DURING_CHECK' }
    if ($null -ne $previousBytes) {
        $backupDirectory = Join-Path $plan.data_dir 'model_config_backups'
        [IO.Directory]::CreateDirectory($backupDirectory) | Out-Null
        $backup = Join-Path $backupDirectory ('settings-' + [Guid]::NewGuid().ToString('N') + '.json')
        [IO.File]::WriteAllBytes($backup, $previousBytes)
        if ((Get-ModelSettingsHash $backup) -cne $previousHash) { throw 'MODEL_OVERLAY_BACKUP_FAILED' }
    }
    Write-ModelSettings -Path $overlayPath -Bytes $bytes
    try {
        $actual = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory $ConfigDirectory -Kind daily
        if ($actual.config_fingerprint -cne $plan.config_fingerprint -or $actual.endpoint -cne $plan.endpoint -or -not $actual.credential_present) { throw 'MODEL_SERVICE_READBACK_MISMATCH' }
    } catch {
        if ($null -ne $previousBytes) { Write-ModelSettings -Path $overlayPath -Bytes $previousBytes }
        else { [IO.File]::Delete($overlayPath) }
        throw
    }
}
[ordered]@{
    service = $Service
    applied = [bool]$Apply
    provider = $plan.llm.provider
    model = $plan.llm.model_id
    endpoint = $plan.endpoint
    credential_present = $plan.credential_present
    model_verified = $plan.model_check_verified
    model_calls = if ($Apply) { $doctor.model.calls } else { 0 }
    config_directory = $ConfigDirectory
    backup = $backup
} | ConvertTo-Json -Depth 5
