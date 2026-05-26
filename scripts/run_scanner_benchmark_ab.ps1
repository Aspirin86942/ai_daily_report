<#
.SYNOPSIS
Run ai_daily_report scanner benchmark for Python/Rust discovery cold/warm runs.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\run_scanner_benchmark_ab.ps1

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\run_scanner_benchmark_ab.ps1 `
  -StartDate 2026-05-11 `
  -EndDate 2026-05-25 `
  -BenchDir "$env:TEMP\ai_daily_report_benchmarks"
#>

[CmdletBinding()]
param(
    [string]$StartDate = "2020-01-01",
    [string]$EndDate = "2026-05-25",
    [string]$BenchDir = (Join-Path $env:TEMP "ai_daily_report_benchmarks"),
    [string]$CondaEnv = "test",
    [switch]$NoSummaryMode,
    [switch]$SkipBuild,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $PSCommandPath
$repoRoot = Split-Path -Parent $scriptDir
$rangeLabel = "${StartDate}_${EndDate}"

$oldDiscoveryBackend = $env:DAILY_REPORT_SCANNER__DISCOVERY_BACKEND
$oldIndexDbPath = $env:DAILY_REPORT_SCANNER__INDEX_DB_PATH
$oldRustDiscoveryBin = $env:DAILY_REPORT_SCANNER__RUST_DISCOVERY_BIN
$oldRustOfficeParserBin = $env:DAILY_REPORT_SCANNER__RUST_OFFICE_PARSER_BIN

function Restore-EnvVar {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [string]$Value
    )

    if ($null -eq $Value) {
        Remove-Item -LiteralPath "Env:\$Name" -ErrorAction SilentlyContinue
        return
    }

    Set-Item -LiteralPath "Env:\$Name" -Value $Value
}

function Invoke-CheckedCommand {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [Parameter(Mandatory = $true)]
        [string]$FilePath,
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    Write-Host ""
    Write-Host "==> $Label" -ForegroundColor Cyan
    Write-Host ("    " + $FilePath + " " + ($Arguments -join " "))

    if ($DryRun) {
        return
    }

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Label failed with exit code $LASTEXITCODE"
    }
}

function Invoke-CargoBuild {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [Parameter(Mandatory = $true)]
        [string]$ProjectDir
    )

    Push-Location $ProjectDir
    try {
        Invoke-CheckedCommand -Label "Build $Name" -FilePath "cargo" -Arguments @("build", "--release")
    }
    finally {
        Pop-Location
    }
}

function Clear-SqliteArtifacts {
    param(
        [Parameter(Mandatory = $true)]
        [string]$DbPath
    )

    foreach ($path in @($DbPath, "$DbPath-wal", "$DbPath-shm")) {
        if ($DryRun) {
            Write-Host "DRY RUN: remove $path"
            continue
        }
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    }
}

function Invoke-BenchmarkRun {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [Parameter(Mandatory = $true)]
        [string]$Backend,
        [Parameter(Mandatory = $true)]
        [string]$DbPath
    )

    $jsonOut = Join-Path $BenchDir "$Name.json"
    $markdownOut = Join-Path $BenchDir "$Name.md"

    $env:DAILY_REPORT_SCANNER__DISCOVERY_BACKEND = $Backend
    $env:DAILY_REPORT_SCANNER__INDEX_DB_PATH = $DbPath
    $env:DAILY_REPORT_SCANNER__RUST_DISCOVERY_BIN = "rust/discovery/target/release/ai-daily-discovery.exe"
    $env:DAILY_REPORT_SCANNER__RUST_OFFICE_PARSER_BIN = "rust/office_parser/target/release/ai-daily-office-parser.exe"

    $args = @(
        "run",
        "-n",
        $CondaEnv,
        "python",
        "scripts\benchmark_scanner.py",
        "--start-date",
        $StartDate,
        "--end-date",
        $EndDate,
        "--json-out",
        $jsonOut,
        "--markdown-out",
        $markdownOut
    )

    if (-not $NoSummaryMode) {
        $args += "--summary-mode"
    }

    Invoke-CheckedCommand -Label "Benchmark $Name" -FilePath "conda" -Arguments $args

    if ($DryRun) {
        return [pscustomobject]@{
            Run = $Name
            Backend = $Backend
            Discovered = ""
            DiscoveryMs = ""
            TotalMs = ""
            Reused = ""
            Reparsed = ""
            Json = $jsonOut
            Markdown = $markdownOut
        }
    }

    $payload = Get-Content -LiteralPath $jsonOut -Raw -Encoding UTF8 | ConvertFrom-Json
    return [pscustomobject]@{
        Run = $Name
        Backend = $payload.parameters.discovery_backend
        Discovered = $payload.metrics.discovered_count
        DiscoveryMs = $payload.metrics.discovery_duration_ms
        TotalMs = $payload.metrics.total_duration_ms
        Reused = $payload.metrics.reused_count
        Reparsed = $payload.metrics.reparsed_count
        Json = $jsonOut
        Markdown = $markdownOut
    }
}

try {
    Set-Location $repoRoot
    New-Item -ItemType Directory -Force -Path $BenchDir | Out-Null

    Write-Host "Repository: $repoRoot"
    Write-Host "Benchmark directory: $BenchDir"
    Write-Host "Date range: $StartDate to $EndDate"
    Write-Host "Summary mode: $(-not $NoSummaryMode)"

    if (-not $SkipBuild) {
        Invoke-CargoBuild -Name "Rust discovery" -ProjectDir (Join-Path $repoRoot "rust\discovery")
        Invoke-CargoBuild -Name "Rust Office parser" -ProjectDir (Join-Path $repoRoot "rust\office_parser")
    }

    $pythonDb = Join-Path $BenchDir "scanner-python-$rangeLabel.sqlite3"
    $rustDb = Join-Path $BenchDir "scanner-rust-$rangeLabel.sqlite3"

    Clear-SqliteArtifacts -DbPath $pythonDb
    Clear-SqliteArtifacts -DbPath $rustDb

    $results = @()
    $results += Invoke-BenchmarkRun -Name "scanner-python-cold-$rangeLabel" -Backend "python" -DbPath $pythonDb
    $results += Invoke-BenchmarkRun -Name "scanner-python-warm-$rangeLabel" -Backend "python" -DbPath $pythonDb
    $results += Invoke-BenchmarkRun -Name "scanner-rust-cold-$rangeLabel" -Backend "rust" -DbPath $rustDb
    $results += Invoke-BenchmarkRun -Name "scanner-rust-warm-$rangeLabel" -Backend "rust" -DbPath $rustDb

    Write-Host ""
    Write-Host "Benchmark summary" -ForegroundColor Green
    $results | Format-Table -AutoSize
    Write-Host "Artifacts written to: $BenchDir" -ForegroundColor Green
}
finally {
    Restore-EnvVar -Name "DAILY_REPORT_SCANNER__DISCOVERY_BACKEND" -Value $oldDiscoveryBackend
    Restore-EnvVar -Name "DAILY_REPORT_SCANNER__INDEX_DB_PATH" -Value $oldIndexDbPath
    Restore-EnvVar -Name "DAILY_REPORT_SCANNER__RUST_DISCOVERY_BIN" -Value $oldRustDiscoveryBin
    Restore-EnvVar -Name "DAILY_REPORT_SCANNER__RUST_OFFICE_PARSER_BIN" -Value $oldRustOfficeParserBin
}
