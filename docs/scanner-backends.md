# Native scanner and parser backends

## Production chain

```text
CLI → ReportRunner → NativeScanner → PyO3 Scanner
→ scanner_core → worker v2 pools
```

`ReportRunner` owns report recipes and publication ordering. `NativeScanner` is
the only Python adapter at the scanner seam. The PyO3 object owns one Rust
`Scanner`, which keeps the v3 SQLite store and both lazy worker pools for its
lifetime. A top-level `build_context` call is serialized; work inside that call
may run concurrently and releases the GIL.

There is no scanner executable, JSON scanner transport, request-id transport
protocol, or second database query for evidence. `ScanResult` returns the
context envelope and `ScannerEvidence` from the same run.

## Parser routes

| File type | Parser backend | Worker lane | Isolation |
|---|---|---|---|
| text-like | `light_text_v2` | `rust_core` | in-process Rust |
| `.xlsx` | `rust_xlsx_bounded_v2` | `rust_office_process_v2` | Office worker |
| `.docx`, `.pptx` | `rust_office_oxide_v2` | `rust_office_process_v2` | Office worker |
| `.pdf` | `python_pdf_text_v2` | `python_document_process_v2` | Python worker |
| enabled `.doc`, `.ppt` | `python_sharepoint_text_v2` | `python_document_process_v2` | Python worker |
| eligible Office fallback | `python_office_v2` | `python_document_process_v2` | Python worker |

`parser_backend` identifies who produced content. `worker_lane` identifies
where it ran. Cache and acceptance evidence must keep them separate.

Routing and ordinary fallback order are compiled Rust policy. The only runtime
fallback switch is `fallback_after_timeout`, which defaults to false. A source
version change returns outer retryable `SOURCE_VERSION_CHANGED`; the old
request is never silently replayed or cached.

## Worker v2

Both isolated workers use the same strict NDJSON lifecycle:

1. Start lazily and emit one `hello` frame.
2. Declare worker kind, build identity, and supported operations.
3. Accept request envelopes and return matching response envelopes.
4. Recycle after the request cap, idle TTL, RSS limit, crash, timeout, or dirty
   protocol.

The Office worker supports `office_parse`. The Python worker supports
`pdf_classify`, `pdf_parse`, `python_office_parse`, and
`python_sharepoint_parse`. A capability/build mismatch fails before work is
accepted.

## Cache identity and evidence

The v3 cache identity includes the native build identity, normalized mutable
settings, backend, lane, worker contract/build, budgets, timeouts, and source
fingerprint. Changing any of them invalidates incompatible cached content.

One native result carries context status, diagnostics, warnings, summary,
run ids, stage and extension metrics, file audit, decisions, artifact/reuse
metadata, backend/lane/session data, RSS, and cache evidence. Cold and warm
runs over unchanged inputs must render identical context bytes.

## Benchmark

Use only a synthetic or explicitly approved sanitized directory and a fresh
state directory:

```powershell
$state = Join-Path $env:TEMP ('ai-daily-benchmark-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $state | Out-Null
.\.venv\Scripts\python.exe scripts\benchmark_scanner.py `
  --work-dir (Resolve-Path 'tests\fixtures\worker_documents') `
  --state-dir $state `
  --start-date 2000-01-01 `
  --end-date 2100-01-01 `
  --iterations 5 `
  --json-out (Join-Path $state 'scanner.json') `
  --markdown-out (Join-Path $state 'scanner.md')
```

Each sample pair gets a distinct `scan_index_v3_pair_N.sqlite3` and reuses one
`NativeScanner` for its cold and warm calls. The report includes median,
nearest-rank p95, throughput, peak worker RSS, full warm reuse, native call
count, zero scanner process starts, and zero scanner transport bytes.
