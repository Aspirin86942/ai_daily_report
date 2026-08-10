# Scanner / Context Core

The production architecture is a Python application shell with a Rust
scanner/context core. Python owns CLI input, local configuration, report
storage, templates, and LLM integration. Rust owns discovery, classification,
parser routing, worker deadlines, inventory, parse cache, planning,
aggregation, deterministic compression, and scan/context audit.

## Process boundary

```text
Python CLI
  -> ContextScheduler
     -> RustContextClient
        -> ai-daily-scanner build-context
           -> Rust discovery and light-text parsing
           -> ai-daily-office-parser worker
           -> Python document worker for configured formats/fallbacks
           -> Rust v2 SQLite cache and audit
        <- ContextEnvelope
  -> report generation
```

One scan-backed report crosses the application/core boundary once. Invalid
scanner output, a process crash, timeout, or contract mismatch is an explicit
error. There is no top-level fallback to another scanner implementation.

## Production paths

The repository values below are source-checkout defaults:

```yaml
scanner:
  engine: "rust_v2"
  rust_scanner_bin: "rust/target/release/ai-daily-scanner"
  rust_office_parser_bin: "rust/target/release/ai-daily-office-parser"
  rust_index_db_path: "data/db/scan_index_v2.sqlite3"
```

The discovery crate is a library linked into `ai-daily-scanner`; it is not a
standalone production executable. The Office executable remains a separate,
crash-isolated worker.

An installed release does not trust caller cwd or version-local mutable paths.
`run_current_release.ps1` selects one verified `releases/<version>` directory,
sets it as the child working directory, and supplies absolute scanner, Office
worker, Python worker, module-root, config, report, database, and log paths.
Mutable state remains below `<install-root>/shared`; `Config` rejects a missing,
relative, version-local, or `shared/`-escaping installed path. Strict doctor
reports every effective path before probing the Rust core.

Build and validate the source checkout on Windows with:

```powershell
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
.\.venv\Scripts\python.exe main.py doctor --strict
```

Strict doctor validates the scanner contract/build, writable v2 database
parent, and both worker handshakes. It does not parse a business file or call
an LLM.

The Windows package manifest and `SHA256SUMS` cover both Rust executables,
every shipped Python source/template/PowerShell file, `requirements.lock`, and
the example config. A trusted external verifier checks the exact archive entry
set and all hashes before extracting or executing package code, then validates
the scanner and Office-worker handshakes. Side-by-side install and rollback
switch only `current.json`; scanner cache, report storage, and logs remain in
shared paths. See `windows-deployment.md` for the trust and rollback contract.

## Parser routes and fallback

- Text-like files run inside the Rust core.
- `.xlsx` uses `rust_xlsx_bounded_v1` in the Office worker.
- `.docx` and `.pptx` use `rust_office_oxide_v1` in the Office worker.
- PDF and configured legacy document formats use the Python document worker.
- Parser fallback decisions are owned and audited by Rust. Timeout fallback is
  disabled unless `office_fallback_after_timeout` is explicitly enabled.

`parser_backend` identifies the parser that produced content. `worker_lane`
identifies the isolated execution lane. They are separate audit dimensions.

## Cache identity

The Rust core normalizes the raw scanner profile and owns cache identity.
Parser budgets, backend/fallback settings, semantic profile versions, and the
applicable scanner/worker build fingerprints participate in invalidation.
Every run performs live worker handshakes before cache lookup; persisted
fingerprints are audit evidence, not a substitute for a live check.

Cold and warm runs over unchanged inputs must produce identical context bytes.
Cache state is available through `inspect-run` and is not embedded in the
report prompt context.

## Context compression

The compressor preserves file content verbatim within the per-file budget
(default 100_000 chars); files within budget pass through unchanged. Files
exceeding the budget keep the first 40% and last 60% cut at line boundaries,
joined by an explicit omission marker, so no mid-file content is dropped
silently. `.log` files keep the recent tail with a head-omission marker.
Global context budget defaults to 500_000 chars for every report mode; all
values are overridable through the scanner profile leaves.

## Benchmark evidence

Use a synthetic or approved sanitized directory and a fresh v2 database. The
following Windows PowerShell example runs the tracked Office fixtures once cold
and once warm while keeping all generated evidence under the ignored `.uv/`
directory:

```powershell
$benchmarkRun = Join-Path '.uv\benchmarks' (
  'scanner-fixture-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
)
New-Item -ItemType Directory -Path $benchmarkRun -Force | Out-Null
$previousWorkDir = $env:DAILY_REPORT_PATHS__WORK_DIR
$env:DAILY_REPORT_PATHS__WORK_DIR = (
  Resolve-Path -LiteralPath 'tests\fixtures\worker_documents'
).Path

try {
  uv run python scripts\benchmark_scanner.py `
    --start-date 2000-01-01 `
    --end-date 2100-01-01 `
    --scan-db-path (Join-Path $benchmarkRun 'scan_index_v2.sqlite3') `
    --json-out (Join-Path $benchmarkRun 'cold.json') `
    --markdown-out (Join-Path $benchmarkRun 'cold.md')

  uv run python scripts\benchmark_scanner.py `
    --start-date 2000-01-01 `
    --end-date 2100-01-01 `
    --scan-db-path (Join-Path $benchmarkRun 'scan_index_v2.sqlite3') `
    --json-out (Join-Path $benchmarkRun 'warm.json') `
    --markdown-out (Join-Path $benchmarkRun 'warm.md')
} finally {
  if ([string]::IsNullOrEmpty($previousWorkDir)) {
    Remove-Item Env:DAILY_REPORT_PATHS__WORK_DIR -ErrorAction SilentlyContinue
  } else {
    $env:DAILY_REPORT_PATHS__WORK_DIR = $previousWorkDir
  }
}
```

Review parser-backend counts, worker-lane counts, cache status/reasons, stage
durations, `files_per_second`, and structured diagnostics. The throughput is
defined as `discovered_count * 1000 / total_duration_ms`. The fixed-corpus gate
requires both runs to succeed without errors/timeouts, the warm run to reuse all
files without reparsing, matching content hashes/backend/lane summaries, and
positive warm throughput greater than cold throughput. Benchmarking a business
directory is not authorization to send its content to an LLM.
