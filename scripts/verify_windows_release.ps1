[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$BundleDirectory)
. (Join-Path $PSScriptRoot 'release_common.ps1')
$manifest = Test-RustReleaseBundle $BundleDirectory
[ordered]@{ status = 'verified'; release_version = $manifest.release_version; files = @($manifest.files).Count; engine_build = $manifest.engine_build } | ConvertTo-Json
