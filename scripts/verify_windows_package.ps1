<#
.SYNOPSIS
Validate a Windows release archive before any packaged code is executed.

.DESCRIPTION
Use this trusted bootstrap from a source checkout or a previously verified
installation. Archive names, exact entry set, hashes, metadata, and Rust
handshakes are validated in that order. Matching hashes detect corruption;
they do not authenticate a publisher.
#>

[CmdletBinding(DefaultParameterSetName = 'Archive')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Archive')]
    [string]$ArchivePath,
    [Parameter(ParameterSetName = 'Archive')]
    [string]$ExtractTo = "",
    [Parameter(Mandatory = $true, ParameterSetName = 'Directory')]
    [string]$PackageDirectory,
    [string]$ExpectedGitCommit = "",
    [string]$ExpectedTarget = 'x86_64-pc-windows-msvc'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$utf8 = [Text.UTF8Encoding]::new($false, $true)
$archiveRootName = 'ai-daily-report-windows-x64'

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    return (Get-FileHash -LiteralPath $LiteralPath -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Assert-CanonicalRelativePath {
    param([Parameter(Mandatory = $true)][string]$PathText)
    if (
        -not $PathText -or
        $PathText.Contains('\') -or
        $PathText.Contains(':') -or
        $PathText.StartsWith('/') -or
        $PathText.StartsWith('//') -or
        $PathText -match '^[A-Za-z]:'
    ) {
        throw "Unsafe package path: $PathText"
    }
    $segments = $PathText.Split('/')
    if ($segments -contains '' -or $segments -contains '.' -or $segments -contains '..') {
        throw "Unsafe package path: $PathText"
    }
    if ($PathText -ne (($segments -join '/'))) {
        throw "Non-canonical package path: $PathText"
    }
}

function Read-Utf8File {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    return [IO.File]::ReadAllText($LiteralPath, $utf8)
}

function Read-Manifest {
    param([Parameter(Mandatory = $true)][string]$ManifestPath)
    try {
        $manifest = Read-Utf8File -LiteralPath $ManifestPath | ConvertFrom-Json -Depth 30
    }
    catch {
        throw "manifest.json is not valid UTF-8 JSON"
    }
    foreach ($property in @(
        'schema_version',
        'release_version',
        'git_commit',
        'target_triple',
        'engine_version',
        'engine_build',
        'contract_version',
        'worker_contract_version',
        'cargo_lock_sha256',
        'files'
    )) {
        if ($null -eq $manifest.PSObject.Properties[$property]) {
            throw "manifest.json is missing $property"
        }
    }
    if ($manifest.schema_version -ne 'ai_daily_windows_package_v1') {
        throw 'Unsupported package manifest schema'
    }
    if ($manifest.release_version -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$') {
        throw 'Invalid release_version'
    }
    if ($manifest.git_commit -notmatch '^[0-9a-f]{40,64}$') {
        throw 'Invalid git_commit'
    }
    if ($manifest.cargo_lock_sha256 -notmatch '^[0-9a-f]{64}$') {
        throw 'Invalid Cargo.lock hash'
    }
    if ($ExpectedGitCommit -and $manifest.git_commit -cne $ExpectedGitCommit.ToLowerInvariant()) {
        throw 'Package Git commit does not match the trusted expectation'
    }

    $paths = [System.Collections.Generic.HashSet[string]]::new(
        [StringComparer]::Ordinal
    )
    $casePaths = [System.Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    $lastPath = $null
    foreach ($record in @($manifest.files)) {
        if (
            $null -eq $record.PSObject.Properties['path'] -or
            $null -eq $record.PSObject.Properties['size'] -or
            $null -eq $record.PSObject.Properties['sha256']
        ) {
            throw 'Manifest file records require path, size, and sha256'
        }
        $path = [string]$record.path
        Assert-CanonicalRelativePath -PathText $path
        if ($path -in @('manifest.json', 'SHA256SUMS')) {
            throw "Manifest must not hash itself or SHA256SUMS"
        }
        if (-not $paths.Add($path)) {
            throw "Duplicate manifest path: $path"
        }
        if (-not $casePaths.Add($path)) {
            throw "Case-colliding manifest path: $path"
        }
        if ($null -ne $lastPath -and [StringComparer]::Ordinal.Compare($lastPath, $path) -ge 0) {
            throw 'Manifest allowlist is not in canonical ordinal order'
        }
        $lastPath = $path
        try {
            $size = [int64]$record.size
        }
        catch {
            throw "Invalid manifest size for $path"
        }
        if ($size -lt 0 -or [string]$record.sha256 -notmatch '^[0-9a-f]{64}$') {
            throw "Invalid manifest size or hash for $path"
        }
    }
    if ($paths.Count -eq 0) {
        throw 'Manifest payload allowlist is empty'
    }
    return $manifest
}

function Assert-Sha256Sums {
    param(
        [Parameter(Mandatory = $true)]$Manifest,
        [Parameter(Mandatory = $true)][string]$PackageRoot
    )
    $sumsPath = Join-Path $PackageRoot 'SHA256SUMS'
    $sumText = Read-Utf8File -LiteralPath $sumsPath
    $lines = @(($sumText -split "`n") | Where-Object { $_ })
    $expected = [System.Collections.Generic.List[string]]::new()
    $expected.Add("$(Get-Sha256 -LiteralPath (Join-Path $PackageRoot 'manifest.json'))  manifest.json")
    foreach ($record in @($Manifest.files)) {
        $expected.Add("$($record.sha256)  $($record.path)")
    }
    if ($lines.Count -ne $expected.Count) {
        throw 'SHA256SUMS entry set is not exact'
    }
    for ($index = 0; $index -lt $expected.Count; $index++) {
        if ($lines[$index] -cne $expected[$index]) {
            throw 'SHA256SUMS does not match the canonical manifest hashes'
        }
    }
}

function Assert-PackageDirectory {
    param(
        [Parameter(Mandatory = $true)][string]$PackageRoot,
        [switch]$AllowVenv
    )
    $root = [IO.Path]::GetFullPath($PackageRoot)
    if (-not (Test-Path -LiteralPath $root -PathType Container)) {
        throw "Package directory does not exist: $root"
    }
    $venv = Join-Path $root '.venv'
    if ($AllowVenv -and (Test-Path -LiteralPath $venv)) {
        $venvItem = Get-Item -LiteralPath $venv -Force
        if ($venvItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw 'Installed .venv must not be a reparse point'
        }
    }
    $manifestPath = Join-Path $root 'manifest.json'
    $sumsPath = Join-Path $root 'SHA256SUMS'
    if (
        -not (Test-Path -LiteralPath $manifestPath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $sumsPath -PathType Leaf)
    ) {
        throw 'Package requires manifest.json and SHA256SUMS'
    }
    $manifest = Read-Manifest -ManifestPath $manifestPath
    $actual = [System.Collections.Generic.HashSet[string]]::new(
        [StringComparer]::Ordinal
    )
    $actualCase = [System.Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    Get-ChildItem -LiteralPath $root -Recurse -Force | ForEach-Object {
        $relative = [IO.Path]::GetRelativePath($root, $_.FullName).Replace('\', '/')
        if ($AllowVenv -and ($relative -eq '.venv' -or $relative.StartsWith('.venv/'))) {
            return
        }
        if ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "Package item must not be a reparse point: $relative"
        }
        if ($_.PSIsContainer) {
            return
        }
        Assert-CanonicalRelativePath -PathText $relative
        if (-not $actual.Add($relative) -or -not $actualCase.Add($relative)) {
            throw "Duplicate or case-colliding package file: $relative"
        }
    }
    $expected = [System.Collections.Generic.HashSet[string]]::new(
        [StringComparer]::Ordinal
    )
    [void]$expected.Add('manifest.json')
    [void]$expected.Add('SHA256SUMS')
    foreach ($record in @($manifest.files)) {
        [void]$expected.Add([string]$record.path)
    }
    if (-not $actual.SetEquals($expected)) {
        throw 'Package file set does not equal the manifest allowlist'
    }
    foreach ($record in @($manifest.files)) {
        $path = Join-Path $root ([string]$record.path).Replace('/', '\')
        $item = Get-Item -LiteralPath $path -Force
        if ([int64]$item.Length -ne [int64]$record.size) {
            throw "Payload size mismatch: $($record.path)"
        }
        if ((Get-Sha256 -LiteralPath $path) -cne [string]$record.sha256) {
            throw "Payload hash mismatch: $($record.path)"
        }
    }
    Assert-Sha256Sums -Manifest $manifest -PackageRoot $root
    return [pscustomobject]@{ root = $root; manifest = $manifest }
}

function Assert-RustIdentity {
    param(
        [Parameter(Mandatory = $true)]$Manifest,
        [Parameter(Mandatory = $true)][string]$PackageRoot
    )
    if ($Manifest.target_triple -cne $ExpectedTarget) {
        throw 'Package target triple does not match the trusted expectation'
    }
    if ($Manifest.contract_version -cne 'ai_daily_context/v1') {
        throw 'Package context contract version is unsupported'
    }
    $scanner = Join-Path $PackageRoot 'rust\target\release\ai-daily-scanner.exe'
    $office = Join-Path $PackageRoot 'rust\target\release\ai-daily-office-parser.exe'
    $scannerVersion = (& $scanner version | ConvertFrom-Json -Depth 20)
    if ($LASTEXITCODE -ne 0) { throw 'Scanner version handshake failed' }
    $officeVersion = (& $office version | ConvertFrom-Json -Depth 20)
    if ($LASTEXITCODE -ne 0) { throw 'Office worker version handshake failed' }
    if (
        $scannerVersion.contract -cne 'ai_daily_context' -or
        [int]$scannerVersion.protocol_version -ne 1 -or
        $scannerVersion.target_triple -cne $Manifest.target_triple -or
        $scannerVersion.engine_version -cne $Manifest.engine_version -or
        $scannerVersion.engine_build -cne $Manifest.engine_build
    ) {
        throw 'Scanner handshake does not match the verified manifest'
    }
    if (
        $officeVersion.contract -cne 'ai_daily_worker' -or
        [int]$officeVersion.protocol_version -ne 1 -or
        $officeVersion.worker_contract_version -cne $Manifest.worker_contract_version -or
        $officeVersion.worker_build -cne $Manifest.engine_build
    ) {
        throw 'Office worker handshake does not match the verified manifest'
    }
}

$cleanupRoot = $null
try {
    if ($PSCmdlet.ParameterSetName -eq 'Directory') {
        $validated = Assert-PackageDirectory -PackageRoot $PackageDirectory -AllowVenv
    }
    else {
        $archive = [IO.Path]::GetFullPath($ArchivePath)
        if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
            throw "Archive does not exist: $archive"
        }
        Add-Type -AssemblyName System.IO.Compression
        $input = [IO.File]::OpenRead($archive)
        try {
            $zip = [IO.Compression.ZipArchive]::new(
                $input,
                [IO.Compression.ZipArchiveMode]::Read,
                $false,
                $utf8
            )
            try {
                $entries = @{}
                $entryCase = [System.Collections.Generic.HashSet[string]]::new(
                    [StringComparer]::OrdinalIgnoreCase
                )
                foreach ($entry in $zip.Entries) {
                    $raw = [string]$entry.FullName
                    if (-not $raw.StartsWith("$archiveRootName/", [StringComparison]::Ordinal)) {
                        throw "Archive entry is outside the canonical package root: $raw"
                    }
                    $relative = $raw.Substring($archiveRootName.Length + 1)
                    Assert-CanonicalRelativePath -PathText $relative
                    if ($entry.Name -eq '' -or $entries.ContainsKey($relative)) {
                        throw "Duplicate or directory archive entry: $raw"
                    }
                    if (-not $entryCase.Add($relative)) {
                        throw "Case-colliding archive entry: $raw"
                    }
                    $unixType = (($entry.ExternalAttributes -shr 16) -band 0xF000)
                    $dosAttributes = ($entry.ExternalAttributes -band 0xFFFF)
                    if ($unixType -eq 0xA000 -or ($dosAttributes -band 0x400)) {
                        throw "Archive reparse/symlink entry is forbidden: $raw"
                    }
                    $entries[$relative] = $entry
                }
                if (
                    -not $entries.ContainsKey('manifest.json') -or
                    -not $entries.ContainsKey('SHA256SUMS')
                ) {
                    throw 'Archive requires manifest.json and SHA256SUMS'
                }

                $manifestReader = [IO.StreamReader]::new(
                    $entries['manifest.json'].Open(),
                    $utf8,
                    $false
                )
                try { $manifestText = $manifestReader.ReadToEnd() }
                finally { $manifestReader.Dispose() }
                try { $manifest = $manifestText | ConvertFrom-Json -Depth 30 }
                catch { throw 'manifest.json is not valid UTF-8 JSON' }
                $manifestScratch = Join-Path ([IO.Path]::GetTempPath()) (
                    '.manifest-' + [guid]::NewGuid().ToString('N') + '.json'
                )
                try {
                    [IO.File]::WriteAllText($manifestScratch, $manifestText, $utf8)
                    $manifest = Read-Manifest -ManifestPath $manifestScratch
                }
                finally {
                    if (Test-Path -LiteralPath $manifestScratch) {
                        Remove-Item -LiteralPath $manifestScratch -Force
                    }
                }
                $expectedEntries = [System.Collections.Generic.HashSet[string]]::new(
                    [StringComparer]::Ordinal
                )
                [void]$expectedEntries.Add('manifest.json')
                [void]$expectedEntries.Add('SHA256SUMS')
                foreach ($record in @($manifest.files)) {
                    [void]$expectedEntries.Add([string]$record.path)
                }
                $actualEntries = [System.Collections.Generic.HashSet[string]]::new(
                    [string[]]$entries.Keys,
                    [StringComparer]::Ordinal
                )
                if (-not $actualEntries.SetEquals($expectedEntries)) {
                    throw 'Archive entry set does not equal the manifest allowlist'
                }

                if ($ExtractTo) {
                    $extractBase = [IO.Path]::GetFullPath($ExtractTo)
                    if (Test-Path -LiteralPath $extractBase) {
                        throw "Extraction destination already exists: $extractBase"
                    }
                }
                else {
                    $extractBase = Join-Path ([IO.Path]::GetTempPath()) (
                        'ai-daily-verify-' + [guid]::NewGuid().ToString('N')
                    )
                    $cleanupRoot = $extractBase
                }
                New-Item -ItemType Directory -Path $extractBase | Out-Null
                $extractPrefix = $extractBase.TrimEnd('\') + '\'
                foreach ($relative in $entries.Keys) {
                    $destination = [IO.Path]::GetFullPath((
                        Join-Path $extractBase "$archiveRootName\$($relative.Replace('/', '\'))"
                    ))
                    if (-not $destination.StartsWith(
                        $extractPrefix,
                        [StringComparison]::OrdinalIgnoreCase
                    )) {
                        throw "Archive extraction escaped the staging directory"
                    }
                    $parent = Split-Path -Parent $destination
                    New-Item -ItemType Directory -Path $parent -Force | Out-Null
                    $sourceStream = $entries[$relative].Open()
                    $destinationStream = [IO.File]::Open(
                        $destination,
                        [IO.FileMode]::CreateNew,
                        [IO.FileAccess]::Write,
                        [IO.FileShare]::None
                    )
                    try { $sourceStream.CopyTo($destinationStream) }
                    finally {
                        $destinationStream.Dispose()
                        $sourceStream.Dispose()
                    }
                }
            }
            finally { $zip.Dispose() }
        }
        finally { $input.Dispose() }
        $packageRoot = Join-Path $extractBase $archiveRootName
        $validated = Assert-PackageDirectory -PackageRoot $packageRoot
    }

    Assert-RustIdentity -Manifest $validated.manifest -PackageRoot $validated.root
    [pscustomobject]@{
        status = 'ok'
        package_root = if ($cleanupRoot) { $null } else { $validated.root }
        release_version = [string]$validated.manifest.release_version
        git_commit = [string]$validated.manifest.git_commit
        engine_build = [string]$validated.manifest.engine_build
    }
}
finally {
    if ($cleanupRoot -and (Test-Path -LiteralPath $cleanupRoot)) {
        $resolved = [IO.Path]::GetFullPath($cleanupRoot)
        $tempPrefix = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
        if (-not $resolved.StartsWith($tempPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Refusing to remove verification staging outside TEMP'
        }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
