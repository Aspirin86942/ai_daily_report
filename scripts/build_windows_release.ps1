[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [Parameter(Mandatory = $true)][string]$ReleaseVersion,
    [string]$Cargo = 'cargo',
    [string]$TargetDirectory = '',
    [string]$PdfiumDirectory = '',
    [string]$GitCommit = ''
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release_common.ps1')
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $TargetDirectory) { $TargetDirectory = Join-Path $repoRoot '.artifacts/rust-migration/cargo-release' }
$buildPath = [IO.Path]::GetFullPath($TargetDirectory).TrimEnd('\','/')
$activeTarget = [IO.Path]::GetFullPath((Join-Path $repoRoot 'rust/target')).TrimEnd('\','/')
if ($buildPath.Equals($activeTarget, [StringComparison]::OrdinalIgnoreCase)) { throw 'Use a separate target directory to preserve the active Python release workers' }
$outputPath = [IO.Path]::GetFullPath($OutputDirectory)
Assert-ReleaseLocalPath $outputPath
if (Test-Path -LiteralPath $outputPath) { throw 'OutputDirectory must not already exist' }
if ($ReleaseVersion -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$') { throw 'Invalid ReleaseVersion' }
if (-not $GitCommit) { $GitCommit = (& git -C $repoRoot rev-parse HEAD).Trim(); if ($LASTEXITCODE -ne 0) { throw 'Unable to read Git HEAD' } }
$dirty = [bool](& git -C $repoRoot status --porcelain)
if (-not $PdfiumDirectory) {
    $PdfiumDirectory = Join-Path $repoRoot 'rust/target/pdfium'
    & (Join-Path $PSScriptRoot 'prepare_rust_dependencies.ps1') -Destination $PdfiumDirectory | Out-Null
}
$inputs = Get-Content -LiteralPath (Join-Path $repoRoot 'rust/build-inputs.json') -Raw -Encoding UTF8 | ConvertFrom-Json
$dll = Join-Path $PdfiumDirectory 'bin/pdfium.dll'
if ((Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash.ToLowerInvariant() -cne $inputs.pdfium.dll_sha256) { throw 'PDFium DLL does not match pinned build input' }
& $Cargo build --manifest-path (Join-Path $repoRoot 'rust/Cargo.toml') -p ai-daily-report -p ai-daily-office-parser -p ai-daily-pdf-parser --release --locked --target x86_64-pc-windows-msvc --target-dir $buildPath --config 'target.x86_64-pc-windows-msvc.rustflags=["-C","target-feature=+crt-static"]'
if ($LASTEXITCODE -ne 0) { throw 'Locked Rust build failed' }
$metadataText = & $Cargo metadata --manifest-path (Join-Path $repoRoot 'rust/Cargo.toml') --format-version 1 --locked --offline --filter-platform x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw 'Unable to read locked dependency metadata' }
$metadata = $metadataText | ConvertFrom-Json
[IO.Directory]::CreateDirectory($outputPath) | Out-Null
foreach ($name in @('ai-daily-report.exe', 'ai-daily-office-parser.exe', 'ai-daily-pdf-parser.exe')) { Copy-Item -LiteralPath (Join-Path $buildPath "x86_64-pc-windows-msvc/release/$name") -Destination (Join-Path $outputPath $name) }
Copy-Item -LiteralPath $dll -Destination (Join-Path $outputPath 'pdfium.dll')
foreach ($folder in @('templates', 'config', 'scripts', 'docs', 'licenses/pdfium', 'licenses/rust')) { [IO.Directory]::CreateDirectory((Join-Path $outputPath $folder)) | Out-Null }
foreach ($template in Get-ChildItem -LiteralPath (Join-Path $repoRoot 'templates') -Filter '*.md' -File) { Copy-Item -LiteralPath $template.FullName -Destination (Join-Path $outputPath 'templates') }
Copy-Item -LiteralPath (Join-Path $repoRoot 'config/settings.example.yaml') -Destination (Join-Path $outputPath 'config/settings.example.yaml')
foreach ($script in @('daily_task_common.ps1', 'install_daily_task.ps1', 'check_daily_task.ps1', 'remove_daily_task.ps1', 'archive_scanner_database.ps1', 'update_release_pointer.ps1', 'release_common.ps1', 'verify_windows_release.ps1')) { Copy-Item -LiteralPath (Join-Path $repoRoot "scripts/$script") -Destination (Join-Path $outputPath 'scripts') }
Copy-Item -LiteralPath (Join-Path $repoRoot 'rust/build-inputs.json') -Destination (Join-Path $outputPath 'build-inputs.json')
foreach ($document in @('README.md', 'CONTEXT.md', 'AGENTS.md', 'CLAUDE.md')) { Copy-Item -LiteralPath (Join-Path $repoRoot $document) -Destination (Join-Path $outputPath $document) }
$docsRoot = Join-Path $repoRoot 'docs'
foreach ($document in Get-ChildItem -LiteralPath $docsRoot -Recurse -File) {
    $destination = Join-Path $outputPath ('docs/' + $document.FullName.Substring($docsRoot.Length + 1))
    [IO.Directory]::CreateDirectory((Split-Path -Parent $destination)) | Out-Null
    Copy-Item -LiteralPath $document.FullName -Destination $destination
}
Copy-Item -LiteralPath (Join-Path $PdfiumDirectory 'LICENSE') -Destination (Join-Path $outputPath 'licenses/pdfium/LICENSE')
Copy-Item -LiteralPath (Join-Path $PdfiumDirectory 'licenses') -Destination (Join-Path $outputPath 'licenses/pdfium/notices') -Recurse
$nodes = @{}; foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
$wanted = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$queue = [Collections.Generic.Queue[string]]::new()
foreach ($package in $metadata.packages | Where-Object { $_.name -in 'ai-daily-report','ai-daily-office-parser','ai-daily-pdf-parser' }) { $queue.Enqueue($package.id) }
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
$files = foreach ($file in Get-ChildItem -LiteralPath $outputPath -Recurse -File | Sort-Object FullName) {
    [ordered]@{ path = $file.FullName.Substring($outputPath.Length + 1).Replace('\','/'); size = $file.Length; sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant() }
}
$office = & (Join-Path $outputPath 'ai-daily-office-parser.exe') hello | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Office worker self-check failed' }
$pdf = & (Join-Path $outputPath 'ai-daily-pdf-parser.exe') hello | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'PDF worker self-check failed' }
if ($office.worker_build -cne $pdf.worker_build -or $office.protocol_version -ne 4 -or $pdf.protocol_version -ne 4) { throw 'Mixed worker builds' }
$manifest = [ordered]@{ schema_version = 'ai_daily_rust_release_v1'; release_version = $ReleaseVersion; git_commit = $GitCommit; source_dirty = $dirty; target = 'x86_64-pc-windows-msvc'; created_at_utc = [DateTime]::UtcNow.ToString('o'); engine_build = $office.worker_build; worker_protocol = $office.protocol_version; pdfium_version = $inputs.pdfium.version; files = @($files) }
Write-ReleaseJson -Path (Join-Path $outputPath 'manifest.json') -Value $manifest
$verified = Test-RustReleaseBundle $outputPath
[ordered]@{ status = 'verified'; output_directory = $outputPath; release_version = $verified.release_version; files = @($verified.files).Count; bytes = ($verified.files | Measure-Object -Property size -Sum).Sum } | ConvertTo-Json
