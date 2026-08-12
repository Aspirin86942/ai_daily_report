[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [Parameter(Mandatory = $true)][string]$ReleaseVersion,
    [string]$Python = (Join-Path $PSScriptRoot '..\.venv\Scripts\python.exe'),
    [string]$GitCommit = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$pythonPath = [IO.Path]::GetFullPath($Python)
$tool = Join-Path $PSScriptRoot 'windows_release.py'
$outputPath = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $outputPath) { throw "OutputDirectory already exists: $outputPath" }
if (-not $GitCommit) {
    $GitCommit = (& git -C $repoRoot rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Unable to resolve Git HEAD' }
}

& $pythonPath $tool verify-runtime --python $pythonPath
if ($LASTEXITCODE -ne 0) { throw 'Release Python is not exact CPython 3.13.13 x64' }

$scratch = Join-Path ([IO.Path]::GetTempPath()) ('.ai-daily-release-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
$savedPyo3 = [Environment]::GetEnvironmentVariable('PYO3_PYTHON', 'Process')
try {
    $env:PYO3_PYTHON = $pythonPath
    & cargo build --manifest-path (Join-Path $repoRoot 'rust\Cargo.toml') `
        --workspace --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Locked Rust release build failed' }

    $wheelDir = Join-Path $scratch 'wheel'
    New-Item -ItemType Directory -Path $wheelDir | Out-Null
    & $pythonPath -m maturin build --release --locked --interpreter $pythonPath `
        --auditwheel repair --out $wheelDir `
        --manifest-path (Join-Path $repoRoot 'rust\scanner_native\Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'Native wheel build failed' }
    $wheels = @(Get-ChildItem -LiteralPath $wheelDir -Filter '*-cp313-cp313-win_amd64.whl' -File)
    if ($wheels.Count -ne 1) { throw 'Build must produce one cp313-cp313-win_amd64 wheel' }

    $cleanVenv = Join-Path $scratch 'verify-venv'
    & $pythonPath -m venv $cleanVenv
    if ($LASTEXITCODE -ne 0) { throw 'Disposable verification venv creation failed' }
    $cleanPython = Join-Path $cleanVenv 'Scripts\python.exe'
    & $cleanPython -m pip install --disable-pip-version-check --no-deps $wheels[0].FullName
    if ($LASTEXITCODE -ne 0) { throw 'Native wheel installation failed' }
    $nativeJson = & $cleanPython -c (
        "import ai_daily_scanner_native as m,json;" +
        "print(json.dumps({'version':m.__version__,'build':m.__build_identity__}))"
    )
    if ($LASTEXITCODE -ne 0) { throw 'Native wheel import failed' }
    $native = $nativeJson | ConvertFrom-Json

    & $cleanPython $tool verify-runtime --python $cleanPython --expected-version '3.13.12' 2>$null
    if ($LASTEXITCODE -eq 0) { throw 'Wrong-version rejection gate did not fail closed' }

    $officeWorker = Join-Path $repoRoot 'rust\target\release\ai-daily-office-parser.exe'
    & $pythonPath $tool package --repo-root $repoRoot --output-dir $outputPath `
        --wheel $wheels[0].FullName --office-worker $officeWorker `
        --release-version $ReleaseVersion --git-commit $GitCommit `
        --native-build-identity ([string]$native.build)
    if ($LASTEXITCODE -ne 0) { throw 'Release bundle creation failed' }
    & $cleanPython $tool verify-bundle --bundle-dir $outputPath
    if ($LASTEXITCODE -ne 0) { throw 'Release bundle verification failed' }
}
finally {
    [Environment]::SetEnvironmentVariable('PYO3_PYTHON', $savedPyo3, 'Process')
    if (Test-Path -LiteralPath $scratch) {
        $resolved = [IO.Path]::GetFullPath($scratch)
        $tempPrefix = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
        if (-not $resolved.StartsWith($tempPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Refusing to remove release scratch outside TEMP'
        }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
