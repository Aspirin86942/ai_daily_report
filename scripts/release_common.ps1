Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
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
function Test-RustReleaseBundle([string]$BundleDirectory) {
    $root = (Resolve-Path -LiteralPath $BundleDirectory).Path
    Assert-ReleaseLocalPath $root
    $manifest = Get-Content -LiteralPath (Join-Path $root 'manifest.json') -Encoding UTF8 -Raw | ConvertFrom-Json
    if ($manifest.schema_version -cne 'ai_daily_rust_release_v1') { throw 'Invalid Rust release manifest' }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($item in $manifest.files) {
        if (-not $seen.Add($item.path) -or $item.path -match '\\|:|^/' -or @($item.path.Split('/') | Where-Object { $_ -in '..', '.', '' }).Count) { throw 'Invalid manifest path' }
        $path = Join-Path $root $item.path
        Assert-ReleaseLocalPath $path
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing release file: $($item.path)" }
        if ((Get-Item -LiteralPath $path).Length -ne $item.size -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $item.sha256) { throw "Release hash mismatch: $($item.path)" }
    }
    foreach ($required in @('ai-daily-report.exe', 'ai-daily-office-parser.exe', 'ai-daily-pdf-parser.exe', 'pdfium.dll', 'config/settings.example.yaml', 'licenses/dependencies.json')) {
        if (-not $seen.Contains($required)) { throw "Manifest omits required file: $required" }
    }
    foreach ($file in Get-ChildItem -LiteralPath $root -File -Recurse -Force) {
        $relative = $file.FullName.Substring($root.Length + 1).Replace('\', '/')
        if ($relative -cne 'manifest.json' -and -not $seen.Contains($relative)) { throw "Unlisted release file: $relative" }
        if ($relative -match '(?i)(^|/)(python[^/]*|\.venv|src|scanner_native)/|(^|/)python[^/]*\.(exe|dll|zip)$|\.(py|pyd|whl)$') { throw "Python runtime payload found: $relative" }
    }
    return $manifest
}
