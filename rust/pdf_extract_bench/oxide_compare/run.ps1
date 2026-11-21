param([string]$PythonExe)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..\..')).Path
$stage = Join-Path $repo '.artifacts\kernel-once-20261121\pdf-compare\acceptance'
$target = (Resolve-Path -LiteralPath (Join-Path $repo '.artifacts\rust-migration\cargo-release')).Path
$manifest = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot 'Cargo.toml')).Path

if (-not $PythonExe) {
    $poppler = Get-Command pdftoppm -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($poppler) {
        $deps = (Get-Item -LiteralPath $poppler.Source).Directory.Parent.Parent.Parent.Parent
        $bundled = Join-Path $deps.FullName 'python\python.exe'
        if (Test-Path -LiteralPath $bundled -PathType Leaf) { $PythonExe = $bundled }
    }
    if (-not $PythonExe) {
        $PythonExe = (Get-Command python -CommandType Application | Select-Object -First 1).Source
    }
}
$PythonExe = (Resolve-Path -LiteralPath $PythonExe).Path
$null = [IO.Directory]::CreateDirectory($stage)
$oldPath = $env:Path
$oldTarget = $env:CARGO_TARGET_DIR
$oldFlags = $env:RUSTFLAGS
Push-Location -LiteralPath $stage
try {
    $env:Path = "$env:USERPROFILE\.cargo\bin;$oldPath"
    $env:CARGO_TARGET_DIR = $target
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    & cargo build --release --locked --target x86_64-pc-windows-msvc --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw "benchmark build failed ($LASTEXITCODE)" }
    if (-not (Test-Path -LiteralPath 'truth.json')) {
        & $PythonExe -B (Join-Path $PSScriptRoot 'corpus.py')
        if ($LASTEXITCODE -ne 0) { throw "synthetic corpus generation failed ($LASTEXITCODE)" }
    }
    & $PythonExe -B (Join-Path $PSScriptRoot 'compare.py')
    if ($LASTEXITCODE -ne 0) { throw "benchmark execution failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
    $env:Path = $oldPath
    $env:CARGO_TARGET_DIR = $oldTarget
    $env:RUSTFLAGS = $oldFlags
}
