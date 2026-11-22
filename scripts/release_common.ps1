Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ReleaseTemplates = @('daily_prompt.md', 'daily_compact_prompt.md', 'daily_render_template.md')
$ReleaseScripts = @(
    'daily_task_common.ps1', 'install_daily_task.ps1', 'check_daily_task.ps1', 'remove_daily_task.ps1',
    'health_notify.ps1', 'set_model_service.ps1', 'kernel_task_rollback.ps1',
    'update_release_pointer.ps1', 'release_common.ps1', 'verify_windows_release.ps1'
)
function Assert-ReleaseLocalPath([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            if ((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Redirected path is not allowed: $cursor" }
        }
        $parent = [IO.Path]::GetDirectoryName($cursor)
        if ($parent -eq $cursor) { break }
        $cursor = $parent
    }
}
function Write-ReleaseJson([string]$Path, $Value, [string]$PreviousHash) {
    Assert-ReleaseLocalPath $Path
    $directory = [IO.Path]::GetDirectoryName($Path)
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $temporary = Join-Path $directory ('.release-' + [guid]::NewGuid().ToString('N') + '.tmp')
    [IO.File]::WriteAllText($temporary, (($Value | ConvertTo-Json -Depth 30) + "`n"), [Text.UTF8Encoding]::new($false))
    if (Test-Path -LiteralPath $Path) {
        if (-not $PreviousHash -or (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -cne $PreviousHash) { throw 'Existing file changed; prepared temporary file retained' }
        [IO.File]::Replace($temporary, $Path, ($Path + '.' + [guid]::NewGuid().ToString('N') + '.bak'))
    } else {
        if ($PreviousHash) { throw 'Existing file disappeared' }
        [IO.File]::Move($temporary, $Path)
    }
}
function Get-ReleaseDoctor([string]$BundleDirectory) {
    # The staging probe must not load a caller's model configuration or write to a live database.
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('ai-daily-release-doctor-' + [guid]::NewGuid().ToString('N'))
    Assert-ReleaseLocalPath $scratch
    $config = Join-Path $scratch 'config'
    [IO.Directory]::CreateDirectory($config) | Out-Null
    try {
        $settings = @{
            paths = @{ work_dir = $scratch; data_dir = "$scratch/data"; db_dir = "$scratch/db"; reports_dir = "$scratch/reports"; log_dir = "$scratch/logs" }
            llm = @{ context_window_tokens = 128000; verified_max_output_tokens = 8192 }
        }
        [IO.File]::WriteAllText((Join-Path $config 'settings.yaml'), ($settings | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
        $start = [Diagnostics.ProcessStartInfo]::new((Join-Path $BundleDirectory 'ai-daily-report.exe'), ('--config-dir "' + $config + '" doctor --strict'))
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $start.WorkingDirectory = $scratch
        $start.RedirectStandardOutput = $true
        $start.RedirectStandardError = $true
        $start.StandardOutputEncoding = [Text.Encoding]::UTF8
        $start.StandardErrorEncoding = [Text.Encoding]::UTF8
        $start.EnvironmentVariables.Clear()
        $start.EnvironmentVariables['SystemRoot'] = $env:SystemRoot
        $start.EnvironmentVariables['WINDIR'] = $env:SystemRoot
        $start.EnvironmentVariables['PATH'] = (Join-Path $env:SystemRoot 'System32')
        $start.EnvironmentVariables['TEMP'] = $scratch
        $start.EnvironmentVariables['TMP'] = $scratch
        $start.EnvironmentVariables['DEEPSEEK_API_KEY'] = 'synthetic-offline-doctor'
        $process = [Diagnostics.Process]::Start($start)
        try {
            $stdout = $process.StandardOutput.ReadToEndAsync()
            $stderr = $process.StandardError.ReadToEndAsync()
            if (-not $process.WaitForExit(45000)) {
                $process.Kill()
                $process.WaitForExit()
                throw 'Isolated bundle doctor exceeded its deadline'
            }
            if ($process.ExitCode -ne 0) { throw "Isolated bundle doctor failed: $($stderr.Result)" }
            return ($stdout.Result | ConvertFrom-Json)
        } finally { $process.Dispose() }
    } finally {
        $resolved = (Resolve-Path -LiteralPath $scratch).Path
        if ($resolved -ine $scratch) { throw "Unexpected doctor scratch directory: $resolved" }
        Assert-ReleaseLocalPath $resolved
        [IO.Directory]::Delete($resolved, $true)
    }
}
function Get-VerifiedReleaseHashes([string]$Root, $Manifest) {
    $hashes = [Collections.Generic.Dictionary[string,string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($item in $manifest.files) {
        if ($hashes.ContainsKey($item.path) -or $item.path -match '\\|:|^/' -or @($item.path.Split('/') | Where-Object { $_ -in '..', '.', '' }).Count) { throw 'Invalid manifest path' }
        $hashes.Add($item.path, $item.sha256)
        $path = Join-Path $root $item.path
        Assert-ReleaseLocalPath $path
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing release file: $($item.path)" }
        if ((Get-Item -LiteralPath $path).Length -ne $item.size -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $item.sha256) { throw "Release hash mismatch: $($item.path)" }
    }
    foreach ($file in Get-ChildItem -LiteralPath $root -File -Recurse -Force) {
        $relative = $file.FullName.Substring($root.Length + 1).Replace('\', '/')
        if ($relative -cne 'manifest.json' -and -not $hashes.ContainsKey($relative)) { throw "Unlisted release file: $relative" }
        if ($relative -match '(?i)(^|/)(python[^/]*|\.venv|src|scanner_native)/|(^|/)python[^/]*\.(exe|dll|zip)$|\.(py|pyd|whl)$') { throw "Python runtime payload found: $relative" }
    }
    return $hashes
}
function Test-RustReleaseBundle([string]$BundleDirectory) {
    $root = (Resolve-Path -LiteralPath $BundleDirectory).Path
    Assert-ReleaseLocalPath $root
    Assert-ReleaseLocalPath (Join-Path $root 'manifest.json')
    $manifest = Get-Content -LiteralPath (Join-Path $root 'manifest.json') -Encoding UTF8 -Raw | ConvertFrom-Json
    if ($manifest.schema_version -cne 'ai_daily_rust_release_v2') { throw 'Invalid Rust release manifest' }
    $hashes = Get-VerifiedReleaseHashes $root $manifest
    $requiredFiles = @('ai-daily-report.exe', 'ai-daily-pdf-worker.exe', 'pdfium.dll', 'build-inputs.json',
        'config/settings.example.yaml', 'config/model-services.example.json', 'licenses/dependencies.json', 'licenses/pdfium/LICENSE',
        'README.md', 'CONTEXT.md','docs/windows-deployment.md', 'docs/daily-report-operations.md', 'docs/change-archive.md',
        'docs/scanner-backends.md', 'docs/current-state.md', 'docs/plans/2026-11-22-kernel-v2-three-phase-plan.md')
    $requiredFiles += @($ReleaseTemplates | ForEach-Object { "templates/$_" })
    $requiredFiles += @($ReleaseScripts | ForEach-Object { "scripts/$_" })
    foreach ($required in $requiredFiles) {
        if (-not $hashes.ContainsKey($required)) { throw "Manifest omits required file: $required" }
    }
    if (@($hashes.Keys | Where-Object { $_ -like 'templates/*' }).Count -ne $ReleaseTemplates.Count) { throw 'Unexpected packaged template' }
    if (-not @($hashes.Keys | Where-Object { $_ -clike 'licenses/pdfium/notices/*' }).Count) { throw 'Manifest omits PDFium notices' }
    $dependencies = Get-Content -LiteralPath (Join-Path $root 'licenses/dependencies.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    foreach ($dependency in $dependencies) {
        foreach ($notice in $dependency.notices) {
            if (-not $hashes.ContainsKey($notice)) { throw "Manifest omits dependency notice: $notice" }
        }
    }
    $inputs = Get-Content -LiteralPath (Join-Path $root 'build-inputs.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($manifest.ocr_enabled) {
        foreach ($notice in @('licenses/ocr-models/LICENSE', 'licenses/ocr-models/NOTICE')) {
            if (-not $hashes.ContainsKey($notice)) { throw "Manifest omits OCR license file: $notice" }
        }
        foreach ($model in $inputs.image_ocr.files) {
            $relative = "ocr-models/$($model.name)"
            if (-not $hashes.ContainsKey($relative)) { throw "Manifest omits required file: $relative" }
            if ($hashes[$relative] -cne $model.sha256) { throw "Pinned OCR model hash mismatch: $($model.name)" }
        }
    }
    if (-not $manifest.ocr_enabled -and @($hashes.Keys | Where-Object { $_ -like 'ocr-models/*' }).Count) {
        throw 'Default release contains unexpected OCR weights'
    }
    $build = & (Join-Path $root 'ai-daily-report.exe') build-info | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read report build identity' }
    $worker = & (Join-Path $root 'ai-daily-pdf-worker.exe') --build-info | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or $manifest.target -cne $inputs.target -or $manifest.worker_protocol -ne 1 -or
        $manifest.pdfium_version -cne $inputs.pdfium.version -or $inputs.pdfium.dll_sha256 -cne $hashes['pdfium.dll'] -or
        $build.engine_build -cnotmatch '^[a-f0-9]{64}$' -or $manifest.engine_build -cne $build.engine_build -or
        $manifest.parser_content_identity -cnotmatch '^[a-f0-9]{64}$' -or $worker.ocr_enabled -ne $manifest.ocr_enabled -or
        $worker.parser_content_identity -cne $build.parser_content_identity -or $worker.parser_content_identity -cne $manifest.parser_content_identity -or
        $worker.protocol_version -ne $manifest.worker_protocol -or $build.protocol_version -ne $manifest.worker_protocol) {
        throw 'Release build identity mismatch'
    }
    $doctor = Get-ReleaseDoctor $root
    if ($doctor.status -cne 'ok' -or $doctor.build -cne $manifest.engine_build -or
        $doctor.templates -ne $ReleaseTemplates.Count -or $doctor.model.calls -ne 0 -or
        $doctor.parser_content_identity -cne $manifest.parser_content_identity) {
        throw 'Packaged offline doctor failed'
    }
    return $manifest
}
