# Scanner Backends

This project has two Rust-accelerated scanner boundaries and keeps Python fallbacks for correctness and auditability.

## Runtime Model

```text
FileScanner
  -> FileDiscoveryService
       -> rust discovery CLI, fallback to Python discovery
  -> ScanPlanner
       -> stable parser profile for cache keys
  -> parser lanes
       -> light_text_v1 for text-like files
       -> rust_xlsx_bounded_v1 for .xlsx previews
       -> rust_office_oxide_v1 for other Office files, with Python fallback
       -> pdf_text_v1 for PDF files
  -> ScanIndexStore
       -> inventory, parse cache, metrics, reparse details
```

The scanner separates parser backend from execution lane. `parser_backend` describes who produced the content; `worker_lane` describes where it ran. Office parsing can be counted as subprocess work while still reporting `rust_xlsx_bounded_v1` or `rust_office_oxide_v1` as the parser backend.

## Discovery Backend

Default config:

```yaml
scanner:
  discovery_backend: "rust"
  rust_discovery_bin: "rust/discovery/target/release/ai-daily-discovery"
  discovery_timeout_seconds: 30
```

Build command:

```bash
cd rust/discovery
cargo test
cargo build --release
```

If the Rust discovery binary is missing, exits non-zero, times out, or returns invalid JSON, `FileDiscoveryService` logs a warning and falls back to Python discovery.

## Office Parser Backend

Default config:

```yaml
scanner:
  office_parser_backend: "rust_office_oxide_v1"
  rust_office_parser_bin: "rust/office_parser/target/release/ai-daily-office-parser"
  office_parser_fallback_enabled: true
  office_parser_fallback_order:
    - "python_office_v1"
    - "python_sharepoint_text_v1"
  office_fallback_after_timeout: false
  office_external_fallback: "disabled"
  office_legacy_extensions_enabled: false
```

Build command:

```bash
cd rust/office_parser
cargo test
cargo build --release
```

Behavior:

- `.xlsx` uses the Rust `rust_xlsx_bounded_v1` fast path inside the Office parser CLI. It reads only the configured sheet, row, column, and character budgets instead of converting the whole workbook before truncation, and handles inline/shared strings used by common `openpyxl` files.
- `.docx` and `.pptx` default to the Rust `office_oxide` path and report `rust_office_oxide_v1`.
- Rust start failures, non-zero exits, invalid JSON, and invalid `FileContext` payloads can fall back to Python.
- Rust timeout is treated as a parse failure by default. Enable `office_fallback_after_timeout: true` only if getting slower fallback content is more important than keeping a strict per-file budget.
- Deterministic bad `.xlsx` ZIP errors from `rust_xlsx_bounded_v1` skip Python fallback so warm scans do not repeatedly pay for a slow failure path.
- `.doc` and `.ppt` are not in the default extension set. Enable legacy extensions only after testing real samples.
- `.xls` keeps the Python Office fallback path available because old binary Excel behavior is less predictable through the Rust path.

## Cache Profile

`ScanPlanner` includes the backend names, Rust Office binary path, fallback switches, and document budgets in the parser profile. Changing any of those values should cause reparse instead of reusing stale parse cache.

Important profile fields include:

- `text_parser_backend`
- `office_parser_backend`
- `pdf_parser_backend`
- `rust_office_parser_bin`
- `office_parser_fallback_enabled`
- `office_parser_fallback_order`
- `office_fallback_after_timeout`
- `office_external_fallback`
- document budget fields such as `document_excerpt_max_chars`

## Benchmark Evidence

Use the real scanner benchmark when changing discovery, cache, or parser behavior:

```bash
DAILY_REPORT_SCANNER__INDEX_DB_PATH=/tmp/ai-daily-report-scan-index.sqlite3 \
python scripts/benchmark_scanner.py \
  --start-date 2026-05-24 \
  --end-date 2026-05-25 \
  --json-out /tmp/scanner.json \
  --markdown-out /tmp/scanner.md
```

Use a temporary `DAILY_REPORT_SCANNER__INDEX_DB_PATH` for no-cache runs so local `data/db/scan_index.sqlite3` is not polluted.

Read these fields in the output:

- `parameters.discovery_backend` confirms Rust or Python discovery selection.
- `parser_backend_summary.by_extension` shows which parser backend handled each extension.
- `reparse_details[].attempted_backend` shows the first Office backend attempted.
- `reparse_details[].fallback_backend` and `fallback_reason` show whether Python fallback was used.
- `failure_class` classifies Rust Office parser failures as `deterministic`, `environment_unavailable`, `contract_failure`, or `recoverable_parser_failure`; `environment_unavailable` rows mean the Rust parser did not start and should not be used as Rust parser performance evidence.
- `rust_duration_ms` and `fallback_duration_ms` split Rust and fallback parser cost.

For `.xlsx`, `parser_backend_summary.by_extension[".xlsx"]` should normally show `rust_xlsx_bounded_v1` when the Rust Office parser CLI is available.

## Verification

Before closing scanner backend changes, run:

```bash
cd rust/discovery && cargo test && cargo build --release
cd ../../rust/office_parser && cargo test && cargo build --release
cd ../..
/home/george/miniconda3/bin/conda run -n test python -m pytest tests -q
/home/george/miniconda3/bin/conda run -n test python -m compileall main.py src tests
git diff --check
```

If `conda` is not available at `/home/george/miniconda3/bin/conda`, use the project-local Python environment that has `requirements.txt` installed.
