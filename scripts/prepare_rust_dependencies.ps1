param([string]$Destination = '')
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$repository = Split-Path -Parent $PSScriptRoot
if (-not $Destination) { $Destination = Join-Path $repository 'rust/target/pdfium' }
$destinationPath = [IO.Path]::GetFullPath($Destination)
$manifest = Get-Content -LiteralPath (Join-Path $repository 'rust/build-inputs.json') -Raw | ConvertFrom-Json
$archive = Join-Path $destinationPath 'pdfium-win-x64.tgz'
$library = Join-Path $destinationPath 'bin/pdfium.dll'
New-Item -ItemType Directory -Path $destinationPath -Force | Out-Null
if (-not (Test-Path -LiteralPath $archive)) {
    Invoke-WebRequest -Uri $manifest.pdfium.archive_url -OutFile $archive
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $manifest.pdfium.archive_sha256) {
    throw 'PDFium archive SHA-256 mismatch; remove the incorrect archive explicitly before retrying.'
}
if (-not (Test-Path -LiteralPath $library)) {
    & tar -xzf $archive -C $destinationPath
    if ($LASTEXITCODE -ne 0) { throw 'PDFium extraction failed' }
}
if ((Get-FileHash -LiteralPath $library -Algorithm SHA256).Hash.ToLowerInvariant() -ne $manifest.pdfium.dll_sha256) {
    throw 'PDFium DLL SHA-256 mismatch'
}
foreach ($license in $manifest.pdfium.archive_license_paths) {
    if (-not (Test-Path -LiteralPath (Join-Path $destinationPath $license))) { throw "PDFium license missing: $license" }
}
Write-Output (Join-Path $destinationPath 'bin')
