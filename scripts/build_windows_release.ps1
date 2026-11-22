[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [Parameter(Mandatory = $true)][string]$ReleaseVersion,
    [string]$Cargo = 'cargo',
    [string]$TargetDirectory = '',
    [string]$PdfiumDirectory = '',
    [string]$GitCommit = '',
    [string]$Harness = '',
    [switch]$WithOcr,
    [string]$OcrModelDirectory = ''
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release_common.ps1')
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $TargetDirectory) { $TargetDirectory = Join-Path $repoRoot '.artifacts/rust-migration/cargo-release' }
$buildPath = [IO.Path]::GetFullPath($TargetDirectory).TrimEnd('\','/')
$activeTarget = [IO.Path]::GetFullPath((Join-Path $repoRoot 'rust/target')).TrimEnd('\','/')
if ($buildPath.Equals($activeTarget, [StringComparison]::OrdinalIgnoreCase)) { throw 'Use a separate target directory to preserve the active Python release workers' }
$destination = [IO.Path]::GetFullPath($OutputDirectory).TrimEnd('\','/')
Assert-ReleaseLocalPath $destination
if (Test-Path -LiteralPath $destination) { throw 'OutputDirectory must not already exist' }
if ($buildPath -ieq $destination -or $buildPath.StartsWith($destination + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Cargo target must be outside OutputDirectory' }
if ($ReleaseVersion -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$') { throw 'Invalid ReleaseVersion' }
$gitRoot = & git -C $repoRoot rev-parse --show-toplevel 2>$null
$inCheckout = $LASTEXITCODE -eq 0 -and [IO.Path]::GetFullPath($gitRoot) -ieq $repoRoot
if ($inCheckout) {
    if (-not $GitCommit) { $GitCommit = (& git -C $repoRoot rev-parse HEAD).Trim() }
    $dirty = [bool](& git -C $repoRoot status --porcelain)
    if ($LASTEXITCODE -ne 0) { throw 'Unable to identify source checkout' }
} else {
    if ($GitCommit -notmatch '^[0-9a-fA-F]{40}$') { throw 'A source export requires its full base Git commit' }
    $dirty = $true
}
if (-not $PdfiumDirectory) {
    $PdfiumDirectory = Join-Path $repoRoot 'rust/target/pdfium'
    & (Join-Path $PSScriptRoot 'prepare_rust_dependencies.ps1') -Destination $PdfiumDirectory | Out-Null
}
$inputs = Get-Content -LiteralPath (Join-Path $repoRoot 'rust/build-inputs.json') -Raw -Encoding UTF8 | ConvertFrom-Json
if ($WithOcr) {
    if (-not $OcrModelDirectory) {
        $OcrModelDirectory = Join-Path $buildPath 'inputs/ppocrv6-tiny'
        [IO.Directory]::CreateDirectory($OcrModelDirectory) | Out-Null
        foreach ($model in $inputs.image_ocr.files) {
            $cached = Join-Path $OcrModelDirectory $model.name
            if (Test-Path -LiteralPath $cached -PathType Leaf) { continue }
            $download = Join-Path $OcrModelDirectory ([guid]::NewGuid().ToString('N') + '.download')
            try {
                Invoke-WebRequest -UseBasicParsing -Uri ($inputs.image_ocr.source + '/' + $model.name) -OutFile $download
                if ((Get-FileHash -LiteralPath $download).Hash.ToLowerInvariant() -cne $model.sha256) { throw "OCR download hash mismatch: $($model.name)" }
                if (-not (Test-Path -LiteralPath $cached)) { [IO.File]::Move($download, $cached) }
            } finally {
                if (Test-Path -LiteralPath $download -PathType Leaf) { [IO.File]::Delete($download) }
            }
        }
    }
    foreach ($model in $inputs.image_ocr.files) {
        $file = Get-Item -LiteralPath (Join-Path $OcrModelDirectory $model.name) -ErrorAction Stop
        if ($file.Length -ne $model.size -or (Get-FileHash -LiteralPath $file.FullName).Hash.ToLowerInvariant() -cne $model.sha256) {
            throw "OCR model differs from pinned build input: $($model.name)"
        }
    }
}
$dll = Join-Path $PdfiumDirectory 'bin/pdfium.dll'
$pdfiumLicense = Join-Path $PdfiumDirectory 'LICENSE'
$pdfiumNotices = Join-Path $PdfiumDirectory 'licenses'
if (-not (Test-Path -LiteralPath $dll -PathType Leaf)) {
    $dll = Join-Path $PdfiumDirectory 'pdfium.dll'
    $pdfiumLicense = Join-Path $PdfiumDirectory 'licenses/pdfium/LICENSE'
    $pdfiumNotices = Join-Path $PdfiumDirectory 'licenses/pdfium/notices'
}
$features = @()
if ($WithOcr) { $features = @('--features', 'pdf_worker/ocr') }
if ((Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash.ToLowerInvariant() -cne $inputs.pdfium.dll_sha256) { throw 'PDFium DLL does not match pinned build input' }
& $Cargo build --manifest-path (Join-Path $repoRoot 'rust/Cargo.toml') -p ai-daily-report -p pdf_worker @features --release --locked --target x86_64-pc-windows-msvc --target-dir $buildPath --config 'target.x86_64-pc-windows-msvc.rustflags=["-C","target-feature=+crt-static"]'
if ($LASTEXITCODE -ne 0) { throw 'Locked Rust build failed' }
$metadataText = & $Cargo metadata --manifest-path (Join-Path $repoRoot 'rust/Cargo.toml') --format-version 1 @features --locked --offline --filter-platform x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw 'Unable to read locked dependency metadata' }
$metadata = $metadataText | ConvertFrom-Json
if (-not $Harness) {
    $artifacts = & $Cargo test --manifest-path (Join-Path $repoRoot 'rust/Cargo.toml') -p ai-daily-report --test bundle_acceptance --no-run --locked --offline --target-dir $buildPath --message-format json
    if ($LASTEXITCODE -ne 0) { throw 'Building the standalone acceptance harness failed' }
    $executables = @($artifacts | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object {
        $_.reason -ceq 'compiler-artifact' -and $_.target.name -ceq 'bundle_acceptance' -and $_.executable
    })
    if ($executables.Count -ne 1) { throw 'Unable to identify the standalone acceptance harness' }
    $Harness = $executables[0].executable
}
$Harness = (Resolve-Path -LiteralPath $Harness).Path
$parent = Split-Path -Parent $destination
$attempt = [guid]::NewGuid().ToString('N')
$outputPath = Join-Path $parent ('.release-stage-' + $attempt)
$acceptancePath = $destination + '-acceptance-' + $attempt
New-Item -ItemType Directory -Path $outputPath -ErrorAction Stop | Out-Null
foreach ($name in @('ai-daily-report.exe', 'ai-daily-pdf-worker.exe')) { Copy-Item -LiteralPath (Join-Path $buildPath "x86_64-pc-windows-msvc/release/$name") -Destination (Join-Path $outputPath $name) }
Copy-Item -LiteralPath $dll -Destination (Join-Path $outputPath 'pdfium.dll')
foreach ($folder in @('templates', 'config', 'scripts', 'docs', 'licenses/pdfium', 'licenses/rust')) { [IO.Directory]::CreateDirectory((Join-Path $outputPath $folder)) | Out-Null }
if ($WithOcr) {
    [IO.Directory]::CreateDirectory((Join-Path $outputPath 'ocr-models')) | Out-Null
    foreach ($model in $inputs.image_ocr.files) { Copy-Item -LiteralPath (Join-Path $OcrModelDirectory $model.name) -Destination (Join-Path $outputPath "ocr-models/$($model.name)") }
    Copy-Item -LiteralPath (Join-Path $repoRoot 'rust/pdf_worker/licenses/ocr-models') -Destination (Join-Path $outputPath 'licenses/ocr-models') -Recurse
}
foreach ($template in $ReleaseTemplates) { Copy-Item -LiteralPath (Join-Path $repoRoot "templates/$template") -Destination (Join-Path $outputPath 'templates') }
Copy-Item -LiteralPath (Join-Path $repoRoot 'config/settings.example.yaml') -Destination (Join-Path $outputPath 'config/settings.example.yaml')
Copy-Item -LiteralPath (Join-Path $repoRoot 'scripts/model-services.example.json') -Destination (Join-Path $outputPath 'config/model-services.example.json')
foreach ($script in $ReleaseScripts) { Copy-Item -LiteralPath (Join-Path $repoRoot "scripts/$script") -Destination (Join-Path $outputPath 'scripts') }
Copy-Item -LiteralPath (Join-Path $repoRoot 'rust/build-inputs.json') -Destination (Join-Path $outputPath 'build-inputs.json')
foreach ($document in @('README.md', 'CONTEXT.md', 'docs/windows-deployment.md', 'docs/daily-report-operations.md',
    'docs/change-archive.md', 'docs/scanner-backends.md', 'docs/current-state.md',
    'docs/plans/2026-11-22-kernel-v2-three-phase-plan.md')) {
    [IO.Directory]::CreateDirectory((Split-Path -Parent (Join-Path $outputPath $document))) | Out-Null
    Copy-Item -LiteralPath (Join-Path $repoRoot $document) -Destination (Join-Path $outputPath $document)
}
Copy-Item -LiteralPath $pdfiumLicense -Destination (Join-Path $outputPath 'licenses/pdfium/LICENSE')
Copy-Item -LiteralPath $pdfiumNotices -Destination (Join-Path $outputPath 'licenses/pdfium/notices') -Recurse
$nodes = @{}; foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
$wanted = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$queue = [Collections.Generic.Queue[string]]::new()
foreach ($package in $metadata.packages | Where-Object { $_.name -in 'ai-daily-report','pdf_worker' }) { $queue.Enqueue($package.id) }
while ($queue.Count) {
    $id = $queue.Dequeue()
    if (-not $wanted.Add($id)) { continue }
    foreach ($dependency in $nodes[$id].deps) { if (@($dependency.dep_kinds | Where-Object { $null -eq $_.kind -or $_.kind -eq 'build' }).Count) { $queue.Enqueue($dependency.pkg) } }
}
$dependencies = foreach ($package in $metadata.packages | Sort-Object name, version) {
    if (-not $wanted.Contains($package.id)) { continue }
    $licensePaths = @()
    if ($package.source) {
        $directory = Split-Path -Parent $package.manifest_path
        $licenseFiles = @(Get-ChildItem -LiteralPath $directory -File | Where-Object { $_.Name -match '^(LICENSE|COPYING|NOTICE)([-._].*)?$' })
        if ($package.license_file) { $licenseFiles += Get-Item -LiteralPath (Join-Path $directory $package.license_file) }
        if ($package.name -in 'rusto-core', 'rusto-nn' -and -not $licenseFiles.Count) {
            $licenseFiles += Get-Item -LiteralPath (Join-Path $repoRoot 'rust/pdf_worker/licenses/RustO-LICENSE')
        }
        $licenseFiles = @($licenseFiles | Sort-Object FullName -Unique)
        foreach ($file in $licenseFiles) {
            $relative = "licenses/rust/$($package.name)-$($package.version)/$($file.Name)"
            [IO.Directory]::CreateDirectory((Split-Path -Parent (Join-Path $outputPath $relative))) | Out-Null
            Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $outputPath $relative)
            $licensePaths += $relative
        }
        if (-not $package.license -and -not $licensePaths.Count) { throw "Missing license metadata: $($package.name)" }
    }
    [ordered]@{ name = $package.name; version = $package.version; license = $package.license; source = $package.source; notices = $licensePaths }
}
Write-ReleaseJson -Path (Join-Path $outputPath 'licenses/dependencies.json') -Value @($dependencies)
$build = & (Join-Path $outputPath 'ai-daily-report.exe') build-info | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Cannot read compiled report build identity' }
$pdfBuild = & (Join-Path $outputPath 'ai-daily-pdf-worker.exe') --build-info | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or $build.parser_content_identity -cne $pdfBuild.parser_content_identity -or
    $build.protocol_version -ne $pdfBuild.protocol_version -or $pdfBuild.ocr_enabled -ne [bool]$WithOcr) {
    throw 'Report and PDF worker must be built from matching inputs'
}
$files = foreach ($file in Get-ChildItem -LiteralPath $outputPath -Recurse -File | Sort-Object FullName) {
    [ordered]@{ path = $file.FullName.Substring($outputPath.Length + 1).Replace('\','/'); size = $file.Length; sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant() }
}
$manifest = [ordered]@{ schema_version = 'ai_daily_rust_release_v2'; release_version = $ReleaseVersion; git_commit = $GitCommit; source_dirty = $dirty; target = 'x86_64-pc-windows-msvc'; created_at_utc = [DateTime]::UtcNow.ToString('o'); engine_build = $build.engine_build; parser_content_identity = $build.parser_content_identity; worker_protocol = $build.protocol_version; ocr_enabled = [bool]$WithOcr; pdfium_version = $inputs.pdfium.version; files = @($files) }
Write-ReleaseJson -Path (Join-Path $outputPath 'manifest.json') -Value $manifest
& (Join-Path $PSScriptRoot 'accept_windows_bundle.ps1') -BundleDirectory $outputPath -OutputDirectory $acceptancePath -Harness $Harness | Out-Null
$summary = [ordered]@{ status = 'verified'; output_directory = $destination; release_version = $ReleaseVersion; files = @($files).Count;
    bytes = [long]($files | ForEach-Object { $_['size'] } | Measure-Object -Sum).Sum; acceptance = (Join-Path $acceptancePath 'acceptance.json') } | ConvertTo-Json
# A same-volume rename is the only operation that makes the final directory visible.
Assert-ReleaseLocalPath $destination
$stage = (Resolve-Path -LiteralPath $outputPath).Path
if ((Split-Path -Parent $stage) -ine $parent -or (Split-Path -Leaf $stage) -cne ('.release-stage-' + $attempt)) { throw 'Unexpected release staging path' }
[IO.Directory]::Move($stage, $destination)
$summary
