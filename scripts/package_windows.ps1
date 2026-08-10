<#
.SYNOPSIS
Create the deterministic Windows x64 release archive from a trusted checkout.

.DESCRIPTION
The archive contains only the production payload declared in manifest.json.
It does not contain local settings, secrets, data, logs, a virtual environment,
Cargo sources, or target intermediates. The script never downloads anything.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$OutputPath,
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$')]
    [string]$ReleaseVersion,
    [string]$GitCommit = "",
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$utf8 = [Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = $utf8
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8

$scriptDir = Split-Path -Parent $PSCommandPath
$repoRoot = [IO.Path]::GetFullPath((Join-Path $scriptDir ".."))
$archivePath = [IO.Path]::GetFullPath($OutputPath)
$archiveParent = Split-Path -Parent $archivePath
if (-not (Test-Path -LiteralPath $archiveParent -PathType Container)) {
    New-Item -ItemType Directory -Path $archiveParent -Force | Out-Null
}
if (Test-Path -LiteralPath $archivePath) {
    if (-not $Force) {
        throw "Output archive already exists: $archivePath"
    }
    Remove-Item -LiteralPath $archivePath -Force
}

if (-not $GitCommit.Trim()) {
    $GitCommit = (& git -C $repoRoot rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to resolve the Git commit"
    }
}
if ($GitCommit -notmatch '^[0-9a-fA-F]{40,64}$') {
    throw "GitCommit must be a full hexadecimal commit id"
}
$GitCommit = $GitCommit.ToLowerInvariant()

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    return (Get-FileHash -LiteralPath $LiteralPath -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-NormalizedRelativePath {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    $relative = [IO.Path]::GetRelativePath($repoRoot, [IO.Path]::GetFullPath($LiteralPath))
    $normalized = $relative.Replace('\', '/')
    if (
        [IO.Path]::IsPathRooted($normalized) -or
        $normalized.StartsWith('/') -or
        $normalized.Contains(':') -or
        $normalized.Split('/') -contains '..'
    ) {
        throw "Unsafe payload path: $normalized"
    }
    return $normalized
}

function Add-PayloadFile {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [System.Collections.Generic.List[object]]$Payload,
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath
    )
    $fullPath = [IO.Path]::GetFullPath($LiteralPath)
    if (-not (Test-Path -LiteralPath $fullPath -PathType Leaf)) {
        throw "Missing release payload file: $fullPath"
    }
    $Payload.Add([pscustomobject]@{
        path = Get-NormalizedRelativePath -LiteralPath $fullPath
        source = $fullPath
    })
}

$scanner = Join-Path $repoRoot 'rust\target\release\ai-daily-scanner.exe'
$officeWorker = Join-Path $repoRoot 'rust\target\release\ai-daily-office-parser.exe'
if (-not (Test-Path -LiteralPath $scanner -PathType Leaf)) {
    throw "Missing release scanner; build rust/Cargo.toml --release --locked first"
}
if (-not (Test-Path -LiteralPath $officeWorker -PathType Leaf)) {
    throw "Missing release Office worker; build rust/Cargo.toml --release --locked first"
}

$scannerVersion = (& $scanner version | ConvertFrom-Json -Depth 20)
if ($LASTEXITCODE -ne 0) {
    throw "Scanner version handshake failed"
}
$officeVersion = (& $officeWorker version | ConvertFrom-Json -Depth 20)
if ($LASTEXITCODE -ne 0) {
    throw "Office worker version handshake failed"
}
if (
    $scannerVersion.contract -ne 'ai_daily_context' -or
    [int]$scannerVersion.protocol_version -ne 1 -or
    $scannerVersion.target_triple -ne 'x86_64-pc-windows-msvc'
) {
    throw "Scanner identity is not the Windows context v1 contract"
}
if (
    $officeVersion.contract -ne 'ai_daily_worker' -or
    [int]$officeVersion.protocol_version -ne 1 -or
    $officeVersion.worker_kind -ne 'office' -or
    $officeVersion.worker_build -ne $scannerVersion.engine_build
) {
    throw "Office worker identity does not match the scanner build"
}

$payload = [System.Collections.Generic.List[object]]::new()
Add-PayloadFile -Payload $payload -LiteralPath (Join-Path $repoRoot 'main.py')
Add-PayloadFile -Payload $payload -LiteralPath (Join-Path $repoRoot '.python-version')
Get-ChildItem -LiteralPath (Join-Path $repoRoot 'src') -Recurse -File -Filter '*.py' |
    ForEach-Object { Add-PayloadFile -Payload $payload -LiteralPath $_.FullName }
Get-ChildItem -LiteralPath (Join-Path $repoRoot 'templates') -Recurse -File |
    ForEach-Object { Add-PayloadFile -Payload $payload -LiteralPath $_.FullName }
Add-PayloadFile -Payload $payload -LiteralPath (Join-Path $repoRoot 'requirements.lock')
Add-PayloadFile -Payload $payload -LiteralPath (
    Join-Path $repoRoot 'config\settings.example.yaml'
)
foreach ($scriptName in @(
    'deploy_windows.ps1',
    'verify_windows_package.ps1',
    'install_windows_release.ps1',
    'run_current_release.ps1',
    'rollback_windows_release.ps1'
)) {
    Add-PayloadFile -Payload $payload -LiteralPath (
        Join-Path $repoRoot "scripts\$scriptName"
    )
}
Add-PayloadFile -Payload $payload -LiteralPath $scanner
Add-PayloadFile -Payload $payload -LiteralPath $officeWorker

$payloadByPath = @{}
foreach ($item in $payload) {
    if ($payloadByPath.ContainsKey($item.path)) {
        throw "Duplicate payload path: $($item.path)"
    }
    foreach ($existing in $payloadByPath.Keys) {
        if ($existing.Equals($item.path, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Case-colliding payload paths: $existing and $($item.path)"
        }
    }
    $payloadByPath[$item.path] = $item.source
}
$paths = [string[]]$payloadByPath.Keys
[Array]::Sort($paths, [StringComparer]::Ordinal)
$fileRecords = [System.Collections.Generic.List[object]]::new()
foreach ($path in $paths) {
    $source = $payloadByPath[$path]
    $fileRecords.Add([ordered]@{
        path = $path
        size = [int64](Get-Item -LiteralPath $source).Length
        sha256 = Get-Sha256 -LiteralPath $source
    })
}

$cargoLock = Join-Path $repoRoot 'rust\Cargo.lock'
$manifest = [ordered]@{
    schema_version = 'ai_daily_windows_package_v1'
    release_version = $ReleaseVersion
    git_commit = $GitCommit
    target_triple = 'x86_64-pc-windows-msvc'
    engine_version = [string]$scannerVersion.engine_version
    engine_build = [string]$scannerVersion.engine_build
    contract_version = 'ai_daily_context/v1'
    worker_contract_version = [string]$officeVersion.worker_contract_version
    cargo_lock_sha256 = Get-Sha256 -LiteralPath $cargoLock
    files = $fileRecords
}

$scratch = Join-Path $archiveParent ('.package-' + [guid]::NewGuid().ToString('N'))
$archiveRoot = 'ai-daily-report-windows-x64'
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    $manifestPath = Join-Path $scratch 'manifest.json'
    $manifestText = $manifest | ConvertTo-Json -Depth 20
    [IO.File]::WriteAllText($manifestPath, $manifestText + "`n", $utf8)

    $sumLines = [System.Collections.Generic.List[string]]::new()
    $sumLines.Add("$(Get-Sha256 -LiteralPath $manifestPath)  manifest.json")
    foreach ($record in $fileRecords) {
        $sumLines.Add("$($record.sha256)  $($record.path)")
    }
    $sumsPath = Join-Path $scratch 'SHA256SUMS'
    [IO.File]::WriteAllText(
        $sumsPath,
        (($sumLines -join "`n") + "`n"),
        $utf8
    )

    Add-Type -AssemblyName System.IO.Compression
    $stream = [IO.File]::Open(
        $archivePath,
        [IO.FileMode]::CreateNew,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    try {
        $archive = [IO.Compression.ZipArchive]::new(
            $stream,
            [IO.Compression.ZipArchiveMode]::Create,
            $false,
            $utf8
        )
        try {
            $entries = @(
                [pscustomobject]@{ path = 'manifest.json'; source = $manifestPath },
                [pscustomobject]@{ path = 'SHA256SUMS'; source = $sumsPath }
            )
            foreach ($path in $paths) {
                $entries += [pscustomobject]@{
                    path = $path
                    source = $payloadByPath[$path]
                }
            }
            foreach ($item in $entries) {
                $entry = $archive.CreateEntry(
                    "$archiveRoot/$($item.path)",
                    [IO.Compression.CompressionLevel]::Optimal
                )
                $entry.ExternalAttributes = 0
                $input = [IO.File]::OpenRead($item.source)
                $output = $entry.Open()
                try {
                    $input.CopyTo($output)
                }
                finally {
                    $output.Dispose()
                    $input.Dispose()
                }
            }
        }
        finally {
            $archive.Dispose()
        }
    }
    finally {
        $stream.Dispose()
    }
}
finally {
    if (Test-Path -LiteralPath $scratch) {
        $resolvedScratch = [IO.Path]::GetFullPath($scratch)
        $parentPrefix = [IO.Path]::GetFullPath($archiveParent).TrimEnd('\') + '\'
        if (-not $resolvedScratch.StartsWith(
            $parentPrefix,
            [StringComparison]::OrdinalIgnoreCase
        )) {
            throw "Refusing to remove package scratch outside output directory"
        }
        Remove-Item -LiteralPath $resolvedScratch -Recurse -Force
    }
}

[pscustomobject]@{
    status = 'ok'
    archive_path = $archivePath
    release_version = $ReleaseVersion
    git_commit = $GitCommit
    payload_file_count = $fileRecords.Count
}
