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

Build and validate the source checkout on Windows with:

```powershell
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
.\.venv\Scripts\python.exe main.py doctor --strict
```

Strict doctor validates the scanner contract/build, writable v2 database
parent, and both worker handshakes. It does not parse a business file or call
an LLM.

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

## Benchmark evidence

Use a synthetic or approved sanitized directory and a temporary v2 database:

```powershell
.\.venv\Scripts\python.exe scripts\benchmark_scanner.py `
  --start-date 2026-05-24 `
  --end-date 2026-05-25 `
  --scan-db-path .tmp\scanner-benchmark\scan_index_v2.sqlite3 `
  --json-out .tmp\scanner-benchmark\scanner.json `
  --markdown-out .tmp\scanner-benchmark\scanner.md
```

Review parser-backend counts, worker-lane counts, cache status/reasons, stage
durations, and structured diagnostics. Benchmarking a business directory is
not authorization to send its content to an LLM.
