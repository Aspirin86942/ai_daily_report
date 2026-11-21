[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$BundleDirectory,
    [Parameter(Mandatory=$true)][string]$OutputDirectory
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$scripts = Join-Path (Split-Path -Parent $PSScriptRoot) 'scripts'
. (Join-Path $scripts 'release_common.ps1')
$source = (Resolve-Path -LiteralPath $BundleDirectory).Path
$root = [IO.Path]::GetFullPath($OutputDirectory)
Assert-ReleaseLocalPath $root
if ((Test-Path -LiteralPath $root) -or $root.StartsWith($source.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Use a fresh output directory outside the bundle'
}
New-Item -ItemType Directory -Path $root -ErrorAction Stop | Out-Null
$candidate = Join-Path $root 'candidate'
Copy-Item -LiteralPath $source -Destination $candidate -Recurse
$manifestPath = Join-Path $candidate 'manifest.json'
$manifestText = [IO.File]::ReadAllText($manifestPath)
$utf8 = [Text.UTF8Encoding]::new($false)
$checks = 0
$verified = (& (Join-Path $candidate 'scripts/verify_windows_release.ps1') -BundleDirectory $candidate | Out-String) | ConvertFrom-Json
if ($verified.status -cne 'verified' -or $verified.files -lt 1) { throw 'Packaged verifier did not verify the baseline bundle' }

function Assert-Rejected([scriptblock]$Action, [string]$Expected) {
    $message = ''
    try { & $Action | Out-Null } catch { $message = $_.Exception.Message }
    if ($message -notlike $Expected) { throw "Expected '$Expected', received '$message'" }
    $script:checks++
}
function Update-TestManifest([string]$RelativePath) {
    $manifest = $manifestText | ConvertFrom-Json
    $file = Get-Item -LiteralPath (Join-Path $candidate $RelativePath)
    $entry = $manifest.files | Where-Object path -ceq $RelativePath
    $entry.size = $file.Length
    $entry.sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 30), $utf8)
}

# Both the manifest and directory omit the file: the verifier must know it is required.
$required = @('scripts/health_notify.ps1', 'parser-build.json', 'templates/personal_daily_scan_template_v6.md')
$hasOcrModels = Test-Path -LiteralPath (Join-Path $candidate 'ocr-models')
if ($hasOcrModels) { $required += 'ocr-models/rec.mnn' }
foreach ($relative in $required) {
    $manifest = $manifestText | ConvertFrom-Json
    $manifest.files = @($manifest.files | Where-Object path -cne $relative)
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 30), $utf8)
    $file = Join-Path $candidate $relative
    [IO.File]::Delete($file)
    Assert-Rejected { Test-RustReleaseBundle $candidate } "Manifest omits required file: $relative"
    [IO.File]::Copy((Join-Path $source $relative), $file)
}

# Rehash each changed payload to exercise identity checks beyond the file manifest.
if ($hasOcrModels) {
    $relative = 'ocr-models/dict.txt'
    $file = Join-Path $candidate $relative
    [IO.File]::AppendAllText($file, 'changed', $utf8)
    Update-TestManifest $relative
    Assert-Rejected { Test-RustReleaseBundle $candidate } 'Pinned OCR model hash mismatch: dict.txt'
    [IO.File]::Copy((Join-Path $source $relative), $file, $true)
}
$relative = 'parser-manifest.json'
$file = Join-Path $candidate $relative
$parser = [IO.File]::ReadAllText($file) | ConvertFrom-Json
$parser.files.'pdfium.dll' = '0' * 64
[IO.File]::WriteAllText($file, ($parser | ConvertTo-Json -Depth 10), $utf8)
Update-TestManifest $relative
Assert-Rejected { Test-RustReleaseBundle $candidate } 'Parser manifest hash mismatch: pdfium.dll'
[IO.File]::Copy((Join-Path $source $relative), $file, $true)

$relative = 'templates/personal_daily_scan_prompt_v8.md'
$file = Join-Path $candidate $relative
[IO.File]::AppendAllText($file, 'changed', $utf8)
Update-TestManifest $relative
Assert-Rejected { Test-RustReleaseBundle $candidate } '*TEMPLATE_BUILD_MISMATCH*personal_daily_scan_prompt_v8.md*'
[IO.File]::Copy((Join-Path $source $relative), $file, $true)
[IO.File]::WriteAllText($manifestPath, $manifestText, $utf8)

$runner = Join-Path $root 'no-matching-test.ps1'
[IO.File]::WriteAllText($runner, 'exit 0', $utf8)
$results = Join-Path $root 'missing-evidence'
Assert-Rejected { & (Join-Path $scripts 'accept_windows_bundle.ps1') -BundleDirectory $candidate -Harness $runner -OutputDirectory $results } 'Missing acceptance result:*'
if (Test-Path -LiteralPath (Join-Path $results 'acceptance.json')) { throw 'Missing case evidence produced a passing summary' }
[ordered]@{status='passed'; checks=$checks; bundle=$source} | ConvertTo-Json
