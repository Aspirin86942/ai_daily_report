# Windows-first Rust Scanner Core Migration Implementation Plan

> **For the executing Codex:** Follow this plan in order. Complete and verify one task before starting the next. Do not silently widen scope, do not read or print API keys, and do not run business files through an LLM during scanner migration verification.

**Goal:** Evolve the project from “Python application with two Rust helpers” into a Windows-first application where Python is the application shell and Rust is the deterministic scanner/context core.

**Target architecture:** Python owns CLI, application configuration, LLM integration, report models, report history, and rendering. One versioned Rust engine owns discovery, classification, parser routing, process isolation, timeouts, inventory, parse cache, planning, aggregation, deterministic compression, scan/context audit, and scanner metrics.

**Primary platform:** Windows 10/11 x64 with PowerShell. Linux remains a compatibility build/test target, not a production deployment target.

**Execution style:** Small commits, explicit gates, no permanent dual implementation. A temporary legacy adapter is allowed only until the Rust path passes the cutover gate; rollback after deletion is a Git revert, not silent runtime fallback.

---

## 1. Current Baseline

The current production chain is:

```text
main.py
  -> Python ContextScheduler
      -> Python FileScanner / ColdScannerRun
          -> Rust discovery helper, with Python fallback
          -> Python planner / thread pool / timeout / cache / metrics
          -> Rust Office helper, with Python fallback
          -> Python text and PDF parsing
          -> Python ScanAggregator
      -> Python ContextCompressor
      -> Python scan/context SQLite audit
  -> Python LLMClient
  -> Python report SQLiteStore
  -> Python Jinja2 / Markdown ReportGenerator
```

Confirmed Windows baseline on 2026-07-15 (local paths and filenames are not
recorded in tracked documentation):

- The configured local work directory and ignore rule were honored.
- Scanner-only cold and warm runs used `rust_xlsx_bounded_v1`, completed with
  zero errors/timeouts/Python fallback, and proved warm cache reuse.
- `python main.py doctor` succeeds and both Rust helpers are startable.
- These checks used the scanner only; no business file content was sent to an LLM.

The implementation must preserve that working behavior while changing ownership.

---

## 2. Destination And Ownership

### 2.1 Final runtime flow

```text
Python main.py / CLI
  -> Python ContextScheduler
      -> RustContextClient.build_context(request)
          -> ai-daily-scanner.exe
              -> discovery / classifier / planner
              -> parser routing / workers / deadlines
              -> scan_index_v2.sqlite3
              -> aggregation / decisions / compression
              -> scan + context audit / metrics
          <- ContextEnvelope
      -> final file_context string
  -> Python LLMClient
  -> Python report models / report SQLite / Jinja2 / Markdown
```

Python must make exactly one production cross-language call for one context build. It must not call Rust in a shallow sequence such as `discover -> plan -> parse -> compress`.

### 2.2 Final ownership table

| Concern | Final owner | Notes |
|---|---|---|
| CLI, command selection, exit codes | Python | `main.py` remains the application entry point |
| API keys, LLM provider, prompt, retries | Python | Rust never receives secrets |
| Daily/weekly/monthly report models | Python | Existing Pydantic report models remain |
| Report history and DB aggregation | Python | Existing report SQLite store remains |
| Jinja2 and Markdown output | Python | Existing report generator remains |
| Work directory and date range | Python request | Rust validates before use |
| Discovery and file identity | Rust | Includes Windows path normalization |
| Parser configuration validation | Rust | Python transports an opaque scanner profile only |
| Parser routing and worker lanes | Rust | No Python-side routing in the final state |
| Per-file timeout and process-tree cleanup | Rust | One total deadline per file |
| Inventory, parse cache, scan metrics | Rust | Rust is the only writer |
| File priority and context decisions | Rust | Includes `keep/compress/metadata_only/omit/error` |
| Aggregation and deterministic compression | Rust | Merge the current two overlapping budget layers |
| Scan/context audit | Rust | Python reads through CLI DTOs, never table SQL |
| PDF, legacy parsing, and permitted modern Office fallback implementation | Python worker initially | Rust chooses, starts, times out, validates, and audits it |

### 2.3 Database ownership

Use separate databases with single writers:

- Rust owns `data/db/scan_index_v2.sqlite3`:
  - `file_inventory`
  - `parse_cache`
  - `scan_runs`
  - `scan_extension_metrics`
  - `context_runs`
  - `context_decisions`
- Python owns the existing report database:
  - daily reports
  - weekly reports
  - monthly reports
  - report-history queries

Do not import the old parse cache into v2. Parser ownership and profile semantics are changing, so the safe migration is a cold rebuild. Keep the old database untouched until the legacy implementation is deleted; do not automatically delete it afterward.

---

## 3. Deep Module Contract

### 3.1 Production interface

The only application-facing production interface remains:

```python
class ContextScheduler:
    def build_context(
        self,
        request: ContextScheduleRequest,
    ) -> ContextBuildResult: ...
```

`ContextScheduler` validates the application request, enforces `ok/partial/error` policy, and calls a private injected engine adapter. `RustContextClient` is the production process adapter; it is not a second public application interface. Tests may inject an in-memory fake engine.

The final Python application result is deliberately smaller than the current scanner-internal result:

```text
ContextBuildResult
  file_context: str
  status: ok | partial | error
  summary: ContextSummary
  scan_run_id: int | None
  context_run_id: int | None
  warnings: list[Diagnostic]
  error: Diagnostic | None
```

Task 9 must update `main.py` to consume `summary` rather than the current `scan_result`, and must remove `compressed_context` and per-file `decisions` from the Python application seam. LLM and report code only depend on `file_context`, so their external contracts remain stable.

### 3.2 `BuildContextRequest` v1

Required top-level fields:

```json
{
  "contract": "ai_daily_context",
  "protocol_version": 1,
  "request_id": "caller-generated-stable-id",
  "work_dir": "C:\\scanner-fixtures\\work",
  "start_date": "2026-07-14",
  "end_date": "2026-07-15",
  "report_mode": "daily",
  "compression_profile": null,
  "scan_db_path": "C:\\scanner-fixtures\\state\\scan_index_v2.sqlite3",
  "scanner_profile": {"schema_version": "scanner_profile_v1"},
  "adapters": {
    "office_worker_path": "C:\\scanner-fixtures\\bin\\ai-daily-office-parser.exe",
    "python_executable": "C:\\scanner-fixtures\\venv\\Scripts\\python.exe",
    "python_module_root": "C:\\scanner-fixtures\\repo",
    "python_document_worker_module": "src.workers.document_parser_worker"
  }
}
```

Rules:

- `scanner_profile` is opaque transport on the Python side. Rust owns defaults, normalization, validation, and cache-key semantics.
- `compression_profile` preserves the current optional CLI/scheduler override; `null` selects the Rust default for the report mode.
- `scan_db_path` and `adapters` are process-level infrastructure assembled inside `RustContextClient` from effective local configuration. `main.py` and callers of `ContextScheduler` never construct or interpret them.
- The request must never contain an API key, LLM endpoint, prompt, user input, or report content.
- Unknown fields are rejected on both sides.
- Date ranges are closed intervals.
- The Rust engine validates `work_dir` and `scan_db_path` before starting work.
- `request_id` is a UUID generated once per logical context-build call. A transport retry of that same logical call reuses the id; a new CLI report run gets a new id. Rust enforces uniqueness/idempotency in the scan DB.
- Python resolves `work_dir`, `scan_db_path`, executable paths, and module root to absolute paths before serialization. Rust rejects relative infrastructure paths, so Task Scheduler or a non-repository current directory cannot change behavior.

#### Scanner profile v1

`scanner_profile` is opaque only to the Python application layer; it is not schema-free. Task 2 must commit `docs/contracts/scanner-profile-request-v1.schema.json` and `docs/contracts/scanner-profile-normalized-v1.schema.json`. The following is the complete equivalent schema and is authoritative if prose elsewhere is ambiguous.

**Raw request shape:** it is one flat object, not nested regular/summary subobjects. `schema_version` is the only required leaf and must equal `scanner_profile_v1`. Every other listed leaf is optional; absence selects the frozen Rust default, while explicit `null` is invalid. Integers use strict wire typing, so booleans, numeric strings, and floats including integral `4.0` tokens are invalid. Generic JSON Schema treats `4.0` as an integer value, so Rust/Python typed deserializers must enforce this lexical rule and both consume the semantic invalid corpus. Unknown leaves are invalid.

| Raw leaf | Exact type |
|---|---|
| `schema_version` | required literal `scanner_profile_v1` |
| `allowed_extensions`, `ignored_patterns`, `excluded_dirs` | optional arrays of non-empty strings |
| `max_workers`, `max_file_size_mb`, `discovery_timeout_seconds`, `file_timeout_seconds`, `total_max_chars` | optional integers |
| `file_timeout_by_extension` | optional object whose keys are extensions and values are integers |
| `parser_profile_version`, `office_parser_backend`, `pdf_parser_backend`, `office_fallback_policy_version` | optional non-empty strings |
| `office_parser_fallback_enabled`, `office_fallback_after_timeout`, `office_legacy_extensions_enabled`, `pptx_include_notes` | optional booleans |
| `office_parser_fallback_order` | optional ordered array of unique adapter-name strings |
| `direct_text_max_bytes`, `direct_text_read_bytes`, `log_tail_read_bytes`, `text_excerpt_max_chars` | optional integers |
| `excel_max_rows`, `pdf_max_pages`, `text_max_chars`, `excel_max_sheets`, `excel_max_columns` | optional regular-mode integers |
| `docx_max_paragraphs`, `docx_max_tables`, `docx_table_max_rows`, `docx_table_max_cols` | optional regular-mode integers |
| `pptx_max_slides`, `document_excerpt_max_chars` | optional regular-mode integers |
| `summary_excel_max_rows`, `summary_pdf_max_pages`, `summary_text_max_chars`, `summary_excel_max_sheets`, `summary_excel_max_columns` | optional summary-mode integers |
| `summary_docx_max_paragraphs`, `summary_docx_max_tables`, `summary_docx_table_max_rows`, `summary_docx_table_max_cols` | optional summary-mode integers |
| `summary_pptx_max_slides`, `summary_document_excerpt_max_chars` | optional summary-mode integers |

No infrastructure leaf is allowed in this object. In particular, reject `discovery_backend`, `rust_discovery_bin`, `rust_office_parser_bin`, `index_db_path`, `worker_lane_mode`, and `office_external_fallback`. Their required replacements are either absolute fields elsewhere in `BuildContextRequest` or Rust-owned implementation choices.

**Frozen defaults:**

| Area | v1 default |
|---|---|
| Discovery | extensions `[.xlsx,.xls,.pptx,.pdf,.txt,.md,.docx,.csv,.json,.log]`; ignored `[~$*,*.tmp]`; excluded `[]` |
| Execution | workers `4`; max file `50 MiB`; discovery timeout `30 s`; file timeout `30 s`; extension timeouts `{.pdf:45,.xlsx:60,.xls:60}`; aggregate parse-content cap `50000` chars |
| Identity/routing | parser profile `v1`; text `light_text_v1`; Office `rust_office_oxide_v1`; PDF `pdf_text_v1`; fallback enabled; ordered fallback `[python_office_v1,python_sharepoint_text_v1]`; fallback after timeout `false`; legacy extensions `false`; fallback policy `hybrid_v1` |
| Shared text | direct/read-head `262144` bytes; log read-tail `262144` bytes; excerpt defaults to the selected mode's text max |
| Regular | text/excerpt `6000`; PDF pages `5`; Excel sheets/rows/columns `5/50/20`; DOCX paragraphs/tables/table rows/table cols `200/20/50/12`; PPTX slides `50`; notes `true`; document excerpt `6000` |
| Summary | text/excerpt `2000`; PDF pages `2`; Excel sheets/rows/columns `2/10/12`; DOCX paragraphs/tables/table rows/table cols `80/8/20/8`; PPTX slides `15`; notes `true`; document excerpt `2000` |
| Daily context | `daily_balanced_v1`, global/per-file chars `50000/8000` |
| Weekly context | `weekly_balanced_v1`, global/per-file chars `50000/5000` |
| Monthly context | `monthly_balanced_v1`, global/per-file chars `60000/4000` |
| Shared context | size thresholds `65536/1048576/10485760` bytes; priority policy `default_v1`; compression policy `markdown_context_v1` |

Task 2 must prove each value against current `ScanPlanner.build_parser_profile()`, `ContextProfile.for_report_mode()`, and the checked-in example config before copying it into Rust constants. A disagreement is a stop condition: record it and amend this plan/ADR rather than silently choosing a value.

**Validation bounds:**

- `max_workers`: `1..=64`; max file size: `1..=4096 MiB`; timeouts: `1..=3600 s`.
- Read-byte budgets: `1..=67108864`; character budgets: `1..=10000000`.
- PDF pages: `1..=10000`; Excel sheets: `1..=1024`; Excel rows/table rows: `1..=1048576`; Excel/table columns: `1..=16384`.
- DOCX paragraphs: `1..=1000000`; DOCX tables and PPTX slides: `1..=100000`.
- Context size thresholds: `1..=4294967296` bytes and strictly `small < medium < large`; global chars must be greater than or equal to per-file chars.
- Arrays have at most 256 entries; a pattern/extension/backend/version string is `1..=1024` Unicode scalar values.
- Extensions are lowercase, begin with `.`, contain no separator/NUL/colon, and are at most 32 characters. Normalized set-like arrays are trimmed, sorted, and deduplicated.
- Ignore/excluded patterns are trimmed; empty values are rejected. Fallback order is trimmed and deduplicated without reordering, because order is semantic; allowed values are exactly `python_office_v1` and `python_sharepoint_text_v1` in v1.
- `report_mode=daily` chooses regular parser budgets and daily context. `weekly|monthly` choose summary parser budgets and the corresponding context.
- A non-null `compression_profile` must equal the selected mode's frozen profile name; arbitrary custom names are not accepted in v1.

Python adds `schema_version`, copies only present raw settings from the allowlist, and does no defaulting, unit conversion, classification, or fallback selection. This preserves ignored local values such as `ignored_patterns: ["*归档*.xlsx"]` without making Python the semantic owner.

**Normalized shape:** every leaf below is required and non-null. Daily is shown; weekly/monthly change only the selected parse-budget values and context object described above.

```json
{
  "schema_version": "normalized_scanner_profile_v1",
  "parser_profile_version": "v1",
  "report_mode": "daily",
  "discovery": {
    "allowed_extensions": [".csv", ".docx", ".json", ".log", ".md", ".pdf", ".pptx", ".txt", ".xls", ".xlsx"],
    "ignored_patterns": ["*.tmp", "~$*"],
    "excluded_dirs": []
  },
  "execution": {
    "max_workers": 4,
    "max_file_size_bytes": 52428800,
    "discovery_timeout_ms": 30000,
    "file_timeout_ms": 30000,
    "file_timeout_by_extension_ms": {".pdf": 45000, ".xls": 60000, ".xlsx": 60000}
  },
  "parse": {
    "aggregate_max_chars": 50000,
    "text": {
      "backend": "light_text_v1",
      "read_head_bytes": 262144,
      "read_tail_bytes": 262144,
      "max_chars": 6000,
      "excerpt_max_chars": 6000
    },
    "office": {
      "primary_backend": "rust_office_oxide_v1",
      "fallback_enabled": true,
      "fallback_order": ["python_office_v1", "python_sharepoint_text_v1"],
      "fallback_after_timeout": false,
      "fallback_policy_version": "hybrid_v1",
      "legacy_extensions_enabled": false,
      "excel_max_sheets": 5,
      "excel_max_rows": 50,
      "excel_max_columns": 20,
      "docx_max_paragraphs": 200,
      "docx_max_tables": 20,
      "docx_table_max_rows": 50,
      "docx_table_max_cols": 12,
      "pptx_max_slides": 50,
      "pptx_include_notes": true,
      "document_excerpt_max_chars": 6000
    },
    "pdf": {
      "backend": "pdf_text_v1",
      "max_pages": 5,
      "excerpt_max_chars": 6000
    }
  },
  "context": {
    "profile_name": "daily_balanced_v1",
    "global_max_chars": 50000,
    "per_file_max_chars": 8000,
    "small_file_max_bytes": 65536,
    "medium_file_max_bytes": 1048576,
    "large_file_max_bytes": 10485760,
    "priority_policy_version": "default_v1",
    "compression_policy_version": "markdown_context_v1"
  }
}
```

Canonical serialization is UTF-8 JSON from typed Rust structs with fixed field order. Set-like arrays and timeout-map keys are sorted; fallback arrays are not.

Worker implementation identity must be known before cache lookup. At engine startup Rust runs and validates the Office and Python worker version handshakes once, before discovery/cache planning. It then derives a route-specific stack fingerprint:

```text
text-like       = engine_build
modern Office   = engine_build + office_worker_build
                  + (python_worker_build when fallback is enabled)
PDF/legacy      = engine_build + python_worker_build

canonical_normalized_parse_json = canonical JSON of
  {max_file_size_bytes, file_timeout_ms,
   file_timeout_by_extension_ms, parse}

parse_profile_hash(file_type) = SHA-256(
  protocol_version
  + route_stack_fingerprint(file_type)
  + canonical_normalized_parse_json
)
context_profile_hash = SHA-256(
  protocol_version + engine_build + canonical_normalized_context_json
)
```

The worker contract version and implementation build are both fields in each handshake and therefore both enter the route stack fingerprint. A missing, invalid, or changed handshake is resolved before cache lookup; it can never discover a new worker build only after accepting a cache hit. The file cache identity is `(file_identity, source_version, parse_profile_hash)`; file identity/source version are not folded into the profile hash.

### 3.3 `ContextEnvelope` v1

Required top-level fields:

```json
{
  "contract": "ai_daily_context",
  "protocol_version": 1,
  "request_id": "caller-generated-stable-id",
  "engine_version": "0.1.0",
  "engine_build": "stable-build-fingerprint",
  "status": "ok",
  "file_context": "final deterministic context for the LLM",
  "summary": {
    "source_file_count": 1,
    "success_count": 1,
    "timeout_count": 0,
    "included_file_count": 1,
    "omitted_file_count": 0,
    "error_file_count": 0,
    "input_chars": 1000,
    "output_chars": 1000,
    "total_duration_ms": 25,
    "discovery_duration_ms": 2,
    "parse_duration_ms": 18,
    "compression_duration_ms": 1
  },
  "scan_run_id": 1,
  "context_run_id": 1,
  "warnings": [],
  "error": null
}
```

Status values:

- `ok`: all required stages completed with zero unplanned file read/parse errors. Configured exclusions, `metadata_only`, compression, and budget-driven `omit` are expected policy outcomes and do not make the run partial.
- `partial`: the engine completed with one or more explicit unplanned file read/parse errors or declared parser degradation, but produced a trustworthy non-empty context.
- `error`: the engine could not produce a trustworthy context.

Application policy is fixed:

- `ok`: continue to LLM.
- `partial`: display explicit warnings and continue to LLM because the envelope is still trustworthy.
- `error`: stop `daily`, `weekly --source scan`, or `monthly --source scan` before constructing or calling the LLM client, return a nonzero command result, and never substitute an empty or legacy context.
- `weekly/monthly --source db`: unaffected by scanner status.

All `ContextEnvelope` top-level keys and all `ContextSummary` keys shown above are always present; unknown keys are rejected. Counts and durations are non-negative integers and `success_count + timeout_count + error_file_count <= source_file_count`. Status-specific invariants are:

| Status | `file_context` | run ids | `warnings` | `error` |
|---|---|---|---|---|
| `ok` | non-empty auditable context, including the explicit no-files context | both non-null | may contain only non-completeness operational diagnostics | exactly `null` |
| `partial` | non-empty trustworthy context | both non-null | at least one completeness/degradation diagnostic | exactly `null` |
| `error` | exactly `""` | each independently nullable if its stage was never persisted | zero or more prior diagnostics | exactly one `Diagnostic` |

Any completeness-affecting warning forces `partial`; do not return `ok` merely because some files succeeded. An `error` summary reports the non-negative counts/timings observed before failure and uses zero for stages not started; it is never omitted or set to `null`.

The production response intentionally does not expose every internal cache row and parser decision. Benchmark and diagnosis use a separate read-only `inspect-run` command.

### 3.4 Structured error model

Every externally visible error or warning is the same strict `Diagnostic` object:

```json
{
  "error_code": "PARSER_TIMEOUT",
  "message": "file parse exceeded the configured deadline",
  "retryable": true,
  "stage": "parse",
  "file_path": "C:\\scanner-fixtures\\work\\sample.xlsx",
  "backend": "rust_office_oxide_v1"
}
```

All six keys are required. `error_code` and `message` are non-empty strings, `retryable` is boolean, and `stage` is exactly one of `request|discovery|cache|parse|context|process|doctor|inspect|internal`. `file_path` is an absolute string only for a file-scoped diagnostic and otherwise is JSON `null`; `backend` is a non-empty string only when a parser/worker/backend is known and otherwise is JSON `null`. Omission, empty-string sentinels, and invented placeholder paths/backends are invalid.

Initial stable codes:

- `INVALID_REQUEST`
- `CONTRACT_VERSION_MISMATCH`
- `WORK_DIR_NOT_FOUND`
- `WORK_DIR_NOT_DIRECTORY`
- `DISCOVERY_ENTRY_UNREADABLE`
- `FILE_TOO_LARGE`
- `PARSER_START_FAILED`
- `PARSER_TIMEOUT`
- `PARSER_INVALID_PAYLOAD`
- `PARSER_FAILED`
- `WORKER_HANDSHAKE_FAILED`
- `WORKER_VERSION_MISMATCH`
- `WORKER_BUILD_CHANGED`
- `SOURCE_VERSION_CHANGED`
- `CACHE_OPEN_FAILED`
- `CACHE_WRITE_FAILED`
- `SCAN_ALREADY_RUNNING`
- `REQUEST_IN_PROGRESS`
- `REQUEST_ID_CONFLICT`
- `RUN_NOT_FOUND`
- `RUN_CORRUPT`
- `CONTEXT_BUDGET_INVALID`
- `NOT_IMPLEMENTED` (Task 4 shell only; removed before cutover)
- `RUST_CORE_CRASHED` (constructed by Python only when no valid Rust envelope exists)
- `INTERNAL_ERROR`

Do not classify errors by parsing human-readable string prefixes once v1 is active.

### 3.5 CLI process contract

- Except for the explicitly requestless `version` commands below, the caller sends one UTF-8 JSON request on stdin and the process writes exactly one UTF-8 JSON response on stdout.
- A `version` command reads no stdin, has no `request_id`, and returns the strict version DTO. Callers validate contract/version/build/binary identity rather than a nonexistent request id.
- Human-readable diagnostics go to stderr.
- All stderr warnings that affect completeness must also appear in `warnings`.
- Exit `0`: valid `ok` or `partial` response.
- Exit `1`: valid `error` response.
- Exit `2`: request JSON could not be decoded, so no trusted request id exists. Stdout contains the only requestless error DTO, `TransportErrorResponse = {contract:"ai_daily_transport", protocol_version:1, status:"error", error:Diagnostic}`; its diagnostic has `stage=request`, null path/backend, and code `INVALID_REQUEST`. It is never accepted on exit `0/1` or for a decoded request.
- Any nonzero exit without a valid error envelope, including Rust's platform/profile-dependent panic exit, maps to `RUST_CORE_CRASHED`; Python does not depend on a fixed panic exit number and does not fall back.
- Python validates contract name, version, status, schema, and—on request/response commands—the exact request id before trusting content.

### 3.6 Windows path rules

- Accept Chinese characters and spaces.
- Return absolute paths.
- Preserve display casing in `file_path`.
- Normalize `\\?\` and UNC prefixes consistently.
- Use a case-folded normalized path only for `file_identity`.
- Never include file content, API keys, or environment dumps in errors.

### 3.7 Auxiliary command and worker contracts

Task 2 freezes schemas/fixtures for every DTO below; Task 3 implements all of them in Rust and Python before any command implementation. Every object rejects unknown keys. Fields marked nullable are still required keys containing a JSON value or `null`; no other field is optional.

**Scanner `version`** — command reads no stdin. `VersionResponse` contains exactly:

```text
contract="ai_daily_context", protocol_version=1,
binary_name="ai-daily-scanner", engine_version, engine_build, target_triple,
supported_commands["version","doctor","build-context","inspect-run"],
office_worker_contract_version, python_worker_contract_version
```

**`doctor`** — `DoctorRequest` contains exactly `contract`, `protocol_version`, `request_id`, absolute `scan_db_path`, and `adapters` with the same four required absolute/string leaves as `BuildContextRequest`. `DoctorResponse` contains exactly:

```text
contract, protocol_version, request_id,
status=ok|partial|error, engine_version, engine_build,
checks[{name,status=ok|warning|error,message}], warnings[Diagnostic],
error: Diagnostic|null
```

`ok/partial` require `error=null`; `error` requires a non-null error. Doctor runs both worker version handshakes plus DB-parent/capability probes only. It does not parse a business document, mutate the scan DB, or call an LLM.

**Worker version handshake** — both `ai-daily-office-parser version` and `python -m src.workers.document_parser_worker version` read no stdin and return exactly:

```text
contract="ai_daily_worker", protocol_version=1,
worker_kind=office|python_document,
worker_contract_version, worker_version, worker_build,
supported_backends[string], supported_extensions[string]
```

Arrays are canonical sorted unique values. Rust rejects a mismatched kind, contract, version, missing configured backend/extension, empty build, extra stdout, or nonzero exit before any discovery/cache lookup.

**Worker parse** — `WorkerParseRequest` contains exactly:

```text
contract="ai_daily_worker", protocol_version=1, request_id,
file_path, file_type, backend, remaining_timeout_ms,
max_file_size_bytes, parser_limits, expected_source_version
```

`parser_limits` is one of these strict tagged objects; every shown key is required, no other key is allowed, and text is never sent to a worker:

```text
OfficeLimits {
  kind="office", excel_max_sheets, excel_max_rows, excel_max_columns,
  docx_max_paragraphs, docx_max_tables,
  docx_table_max_rows, docx_table_max_cols,
  pptx_max_slides, pptx_include_notes, document_excerpt_max_chars
}
PdfLimits {
  kind="pdf", max_pages, excerpt_max_chars
}
SharePointTextLimits {
  kind="sharepoint_text", excerpt_max_chars
}
```

`rust_office_oxide_v1`, `rust_xlsx_bounded_v1`, and `python_office_v1` use `OfficeLimits` (including `.xls` for the Python legacy route); `pdf_text_v1` uses `PdfLimits`; `python_sharepoint_text_v1` uses `SharePointTextLimits`. Rust requests `rust_xlsx_bounded_v1` for `.xlsx`, so the success invariant that `parser_backend` equals the requested backend preserves the existing dedicated XLSX backend evidence. `WorkerParseResponse` contains exactly:

```text
contract, protocol_version, request_id, status=ok|error,
file_path, file_type, content, parser_backend, worker_lane,
truncated, warnings[Diagnostic], error: Diagnostic|null,
duration_ms, worker_contract_version, worker_version, worker_build,
observed_source_version
```

For worker `ok`, `error=null`; for `error`, `content=""` and `error` is non-null. Rust validates request id, version, exact path/type/backend, worker build against the preflight handshake, observed source version, size budget, and remaining deadline. A source-version change during parsing is a structured error and is never cached. `python_module_root` is absolute; Rust starts the worker without a shell and sets `Command.current_dir` to that root. E2E launches the scanner from outside the repository.

On success `parser_backend` equals the requested backend. `worker_lane` is exactly `rust_office_process` for the Office binary or `python_document_process` for the Python worker; Rust-core text results use `rust_core` only in scan audit rows, never in a worker response.

**`inspect-run`** — `InspectRunRequest` contains exactly:

```text
contract="ai_daily_context", protocol_version=1, request_id,
scan_db_path, scan_run_id, include_content
```

`scan_db_path` is absolute, `scan_run_id` is a positive integer, and `include_content` is a required boolean. `InspectRunResponse` contains exactly:

```text
contract, protocol_version, request_id,
scan_run_id, context_run_id:int|null, status=ok|error,
run_status=running|success|partial|error|abandoned|null,
summary, stage_metrics, extension_metrics,
files[{relative_path,file_identity,source_version,parse_status,
       parser_backend,worker_lane,cache_status,cache_miss_reason,
       truncated,content_sha256,parse_duration_ms,failure_class,
       fallback_backend,fallback_reason_code}],
decisions[{relative_path,action,reason,priority,input_chars,
           output_chars,truncated,error_code}],
warnings[Diagnostic], error: Diagnostic|null
```

`status=error` is used for a missing/corrupt/inaccessible run, requires non-null `error`, and sets `run_status=null` only when no trustworthy run state can be read. Successful inspection uses `ok`, `error=null`, and the persisted `run_status`. `include_content=false` is the production/default caller value and response items contain hashes/metadata only. `include_content=true` is accepted only when the DB/run carries the sanitized-fixture marker written by test setup; the real-directory comparison command always sends `false`. Python benchmark tools consume this DTO and never query Rust-owned tables.

---

## 4. Cargo Workspace Layout

Use one workspace rooted at `rust/Cargo.toml`:

```text
rust/
  Cargo.toml
  Cargo.lock
  scanner_contract/
    Cargo.toml
    src/lib.rs
  scanner_core/
    Cargo.toml
    src/
      lib.rs
      run.rs
      config.rs
      classifier.rs
      planner.rs
      metrics.rs
      compressor.rs
      process.rs
      windows_job.rs
      parsers/
      store/
  scanner_cli/
    Cargo.toml
    src/main.rs
  discovery/
    Cargo.toml
    src/lib.rs
    src/main.rs        # temporary compatibility binary; remove after cutover
  office_parser/
    Cargo.toml
    src/lib.rs
    src/main.rs        # retained as crash-isolated internal worker
```

Final release binaries:

```text
rust/target/release/ai-daily-scanner.exe
rust/target/release/ai-daily-office-parser.exe
```

The standalone discovery binary is transitional. The scanner core eventually calls the discovery library directly.

Use these package names consistently in manifests and commands:

- `ai-daily-scanner-contract`
- `ai-daily-scanner-core`
- `ai-daily-scanner` with binary `ai-daily-scanner`
- existing `ai-daily-discovery`
- existing `ai-daily-office-parser`

Rust library crate identifiers are `ai_daily_scanner_contract`, `ai_daily_scanner_core`, `ai_daily_discovery`, and `ai_daily_office_parser`. Do not let individual tasks invent alternate package names.

### Dependency gate

Do not add production crates until the user approves the dependency change. Proposed minimum additions:

| Crate | Scope | First task | Reason |
|---|---|---:|---|
| `thiserror` | production | 4 | typed internal errors mapped to stable external codes |
| `tempfile` | dev only | 4 | isolated DB and Unicode-path tests |
| `assert_cmd` | dev only | 4 | CLI contract and exit-code tests |
| `rayon` | production | 6 | bounded parallel parsing without maintaining a custom thread pool |
| `windows-sys` as a Windows-target dependency | production | 6 | race-free Job Object process-tree containment without shelling out to `taskkill` |
| `rusqlite` with `bundled` | production | 7 | Rust-owned SQLite without an external Windows DLL |
| `sha2` | production | 7 | stable profile/build fingerprints used by the Rust-owned cache |

Avoid `clap`, `anyhow`, PyO3, and a long-running daemon in v1. Standard argument parsing plus JSON stdin/stdout is sufficient.

---

## 5. Non-negotiable Invariants

- Current CLI command names and report output contracts remain stable.
- `daily`, `weekly --source scan`, and `monthly --source scan` consume the same final context seam.
- `weekly/monthly --source db` remain Python-only and are unaffected.
- `parser_backend` and `worker_lane` remain separate audit fields.
- `.xlsx` continues to report `rust_xlsx_bounded_v1`.
- `.docx/.pptx` continue to report `rust_office_oxide_v1` while using the existing worker.
- One file has one total deadline; fallback may only consume remaining time.
- Timeout does not trigger fallback by default.
- Rust-core startup, timeout, invalid JSON, nonzero exit, or version mismatch never triggers top-level Python fallback.
- Shadow comparison never writes the production scan DB and never enters the LLM path.
- Rust and Python never write the same SQLite database.
- No task modifies or commits `config/settings.yaml`, `config/settings.windows.yaml`, or API keys.
- No real business workbook is sent to an LLM without separate explicit approval.
- No production dependency is added without approval.
- No old Python core is deleted before the cutover gate passes.
- No permanent `python_legacy` compatibility mode remains after cleanup.

---

## 6. Execution And Commit Plan

### Task 0: Preflight And Baseline Evidence

**Commit:** none.

**Actions:**

1. Read this plan, `AGENTS.md`, the existing ADR, and `docs/scanner-backends.md`.
2. Inspect `git status`; preserve all existing user changes.
3. Create a feature branch only after confirming the current branch and dirty state:

```powershell
git switch -c codex/windows-rust-scanner-core
```

4. Confirm local configuration is ignored without printing its contents:

```powershell
git check-ignore config/settings.yaml config/settings.windows.yaml
```

5. Capture the baseline in `%TEMP%`:

```powershell
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
& $py -m pytest tests -q
cargo test --locked --manifest-path rust/discovery/Cargo.toml
cargo test --locked --manifest-path rust/office_parser/Cargo.toml
cargo build --release --locked --manifest-path rust/discovery/Cargo.toml
cargo build --release --locked --manifest-path rust/office_parser/Cargo.toml
& $py main.py doctor
```

6. Run a scanner-only real-directory baseline using an isolated DB and a
   locally supplied sample's modification date. Keep the path in the process
   environment rather than tracked documentation:

```powershell
$ErrorActionPreference = 'Stop'
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
if ([string]::IsNullOrWhiteSpace($env:AI_DAILY_REAL_SAMPLE_FILE)) {
  throw 'AI_DAILY_REAL_SAMPLE_FILE must name a local scanner-only sample'
}
$sample = Get-Item -LiteralPath $env:AI_DAILY_REAL_SAMPLE_FILE
$scanDate = $sample.LastWriteTime.ToString('yyyy-MM-dd')
$tempBase = [System.IO.Path]::GetFullPath($env:TEMP)
$tempRoot = Join-Path $tempBase ("ai-daily-rust-core-baseline-" + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $tempRoot
$hadOldIndex = Test-Path Env:\DAILY_REPORT_SCANNER__INDEX_DB_PATH
$oldIndex = if ($hadOldIndex) { $env:DAILY_REPORT_SCANNER__INDEX_DB_PATH } else { $null }
try {
  $env:DAILY_REPORT_SCANNER__INDEX_DB_PATH = Join-Path $tempRoot 'scan.sqlite3'
  & $py scripts\benchmark_scanner.py `
    --start-date $scanDate `
    --end-date $scanDate `
    --json-out (Join-Path $tempRoot 'scanner.json') `
    --markdown-out (Join-Path $tempRoot 'scanner.md')
  if ($LASTEXITCODE -ne 0) { throw "scanner baseline failed: $LASTEXITCODE" }

  $payload = Get-Content -LiteralPath (Join-Path $tempRoot 'scanner.json') -Raw | ConvertFrom-Json
  [pscustomobject]@{
    total_duration_ms = $payload.metrics.total_duration_ms
    discovered_count = $payload.metrics.discovered_count
    success_count = $payload.metrics.success_count
    error_count = $payload.metrics.error_count
  } | ConvertTo-Json
}
finally {
  if ($hadOldIndex) {
    $env:DAILY_REPORT_SCANNER__INDEX_DB_PATH = $oldIndex
  } else {
    Remove-Item Env:\DAILY_REPORT_SCANNER__INDEX_DB_PATH -ErrorAction SilentlyContinue
  }
  $resolvedTempRoot = [System.IO.Path]::GetFullPath($tempRoot)
  $insideTemp = $resolvedTempRoot.StartsWith(
    $tempBase.TrimEnd('\') + '\',
    [System.StringComparison]::OrdinalIgnoreCase
  )
  if (-not $insideTemp) { throw "refusing to remove non-temp path: $resolvedTempRoot" }
  Remove-Item -LiteralPath $resolvedTempRoot -Recurse -Force
}
```

The temporary DB contains extracted business content, so it is deleted together with `-wal`, `-shm`, JSON, and Markdown evidence in the verified temp directory. Only aggregate counts and timings may be printed or retained.

**Gate:** Do not start migration if the existing full test suite, Rust tests, doctor, or scanner baseline fails for a newly introduced reason. Record pre-existing failures exactly.

---

### Task 1: Harden The Existing Windows Rust Boundaries

**Purpose:** Fix evidence and cache correctness before using the old implementation as the migration oracle.

**Files:**

- Modify `src/services/rust_cli_contract.py`
- Modify `src/services/scan_discovery.py`
- Modify `src/services/scan_planner.py`
- Modify `src/services/file_scanner.py`
- Modify `src/services/scan_metrics.py` only if a structured per-run warning field is added; otherwise define Task 1 warning evidence as logs, not durable DB audit
- Modify `src/core/healthcheck.py`
- Modify `.github/workflows/ci.yml` so a Windows job builds both helpers before running Python integration tests
- Modify `tests/test_rust_cli_contract.py`
- Modify `tests/test_rust_discovery_contract.py`
- Modify `tests/test_scan_planner.py`
- Modify `tests/test_file_scanner.py`
- Modify `tests/test_document_parser.py`

**Required changes:**

1. Centralize Windows `.exe` resolution in `resolve_binary_path`; healthcheck and tests must use the same resolver.
2. Preserve successful Rust stderr in `RustCliJsonResult`; discovery warnings must at minimum be explicit structured logs rather than discarded. Do not claim durable audit unless a tested metrics/schema field is added in this task.
3. Add these semantic fields to the existing parser profile/cache key:
   - default file timeout
   - per-extension timeout map
   - resolved Rust binary size and `mtime_ns`
   Resolve binary metadata in the runtime/config adapter and inject it into `ScanPlanner`; keep the planner deterministic and free of filesystem I/O.
4. Correct the legacy `worker_lane_mode=subprocess` evidence so a Python Office parse cannot report a Rust parser backend.
5. Add real Windows integration tests that fail instead of skipping when a built `.exe` exists.

**Verification:**

```powershell
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
& $py -m pytest `
  tests/test_rust_cli_contract.py `
  tests/test_rust_discovery_contract.py `
  tests/test_scan_planner.py `
  tests/test_file_scanner.py `
  tests/test_document_parser.py -q
```

**Expected commit:**

```text
Harden Windows Rust scanner boundaries
```

**Rollback:** Revert this commit. No database schema changes occur.

---

### Task 2: Record The Architecture Decision And Contract Fixtures

**Files:**

- Add `docs/adr/0002-windows-first-rust-scanner-core.md`
- Add `docs/contracts/scanner-context-v1.md`
- Add `docs/contracts/scanner-profile-request-v1.schema.json`
- Add `docs/contracts/scanner-profile-normalized-v1.schema.json`
- Add strict JSON Schemas under `docs/contracts/` for context envelope, diagnostic/transport error, version, doctor, worker-version, worker-parse, and inspect-run DTOs
- Add `tests/fixtures/scanner_contract/v1/request.json`
- Add `tests/fixtures/scanner_contract/v1/response-ok.json`
- Add `tests/fixtures/scanner_contract/v1/response-partial.json`
- Add `tests/fixtures/scanner_contract/v1/response-error.json`
- Add `tests/fixtures/scanner_contract/v1/profile-daily.json`
- Add `tests/fixtures/scanner_contract/v1/profile-weekly.json`
- Add `tests/fixtures/scanner_contract/v1/profile-monthly.json`
- Add valid request/response fixtures for scanner version, doctor, Office/Python worker version, worker parse, and inspect-run
- Add invalid fixtures covering every optionality, nullability, unknown-field, status invariant, request-id, path, bound, enum, strict-type, route, canonicalization, source-version, and worker-identity class named in Section 3; request/response echo cases carry schema-valid related request/handshake payloads
- Add `tests/test_scanner_contract_fixtures.py` to enforce manifest completeness, Windows Draft 2020-12 validation, semantic/schema classification, synthetic paths, and frozen default provenance
- Include this implementation plan if it is still uncommitted

**ADR decisions that must be explicit:**

- Windows x64 is the production/release platform.
- Linux is compatibility-only.
- Python is the application shell; Rust is the scanner/context core.
- The process seam is one `build-context` request and one `ContextEnvelope` response.
- Rust owns scan/config semantics, cache, compression, and audit.
- Python owns secrets, LLM, report history, and rendering.
- No top-level silent fallback after cutover.
- Old parse cache is not migrated.
- Existing ADR 0001 remains historical for the helper phase; ADR 0002 supersedes its ownership boundary after cutover, not its performance-first timeout principle.

**Verification:**

```powershell
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
Get-ChildItem -LiteralPath tests\fixtures\scanner_contract\v1 -Filter *.json | ForEach-Object {
  & $py -m json.tool $_.FullName > $null
  if ($LASTEXITCODE -ne 0) { throw "invalid JSON fixture: $($_.FullName)" }
}
& $py -m pytest tests\test_scanner_contract_fixtures.py -q `
  --basetemp data\.pytest-tmp\task2-contract
if ($LASTEXITCODE -ne 0) { throw "contract fixture tests failed: $LASTEXITCODE" }
git diff --check
```

**Expected commit:**

```text
Document Windows-first Rust scanner core
```

---

### Task 3: Create The Cargo Workspace And Shared Contract

**Prerequisite:** Dependency gate approved for any new crates used in this task.

**Files:**

- Add `rust/Cargo.toml`
- Add `rust/Cargo.lock`
- Add `rust/scanner_contract/Cargo.toml`
- Add `rust/scanner_contract/src/lib.rs`
- Add `src/models/scanner_contract.py`
- Add `tests/test_scanner_contract.py`
- Modify `rust/discovery/Cargo.toml`
- Modify `rust/office_parser/Cargo.toml`
- Modify `config/settings.example.yaml`
- Modify `src/core/config.py`
- Modify `src/core/healthcheck.py`
- Modify `src/services/scan_discovery.py`
- Modify `src/services/office_parser.py`
- Modify `src/services/scan_planner.py`
- Modify the corresponding config, healthcheck, planner, discovery, and Office tests
- Modify `scripts/run_scanner_benchmark_ab.ps1`
- Modify active Rust build/path references in `README.md` and `docs/scanner-backends.md`; the full Windows-first rewrite remains Task 13
- Remove crate-local lock files only after the workspace lock is generated and verified
- Modify build commands in `.github/workflows/ci.yml` and `scripts/deploy_windows.ps1` only enough to keep existing behavior working

**Contract types:**

- `BuildContextRequest`
- `AdapterPaths`
- `RawScannerProfileV1` and `NormalizedScannerProfileV1`
- `ContextEnvelope`
- `ContextSummary`
- `Diagnostic`
- `TransportErrorResponse`
- `EngineStatus`
- `VersionResponse`
- `DoctorRequest`, `DoctorCheck`, and `DoctorResponse`
- `WorkerVersionResponse`
- `WorkerParseRequest`, backend-tagged `WorkerParserLimits`, and `WorkerParseResponse`
- `InspectRunRequest/Response`

Use strict Serde deserialization and snake_case enum values. Unknown request fields must fail.

Both Rust and Python must parse the same golden fixtures. Do not introduce separate hand-maintained examples with different field sets.

Add `Config.scanner_contract_profile()` (name may vary only if equally explicit) to read only scanner leaves actually present in the merged local settings, add the required schema version, and reject unknown contract candidates. Keep the existing default-expanded `scanner_config` unchanged for the legacy engine until Task 12. `RustContextClient` uses the raw contract method, never the legacy expanded dictionary; this is how Rust remains the sole default/normalization owner during shadow comparison.

Workspace conversion moves both helper artifacts to `rust/target/release`. Update every tracked default and test fixture in the same commit so the still-active Python legacy runtime continues to use Rust. If an ignored local settings file explicitly contains an old helper path, preserve the file and API key and change only the two non-secret helper-path values; alternatively use temporary environment overrides. Never copy or print the full local settings file.

The gate must prove the new paths in a clean checkout/CI job where the old crate-local `target` directories never existed. Locally, assert doctor and integration logs name `rust/target/release`; do not accept a pass obtained from stale binaries under `rust/discovery/target` or `rust/office_parser/target`.

**Verification:**

```powershell
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
cargo fmt --manifest-path rust/Cargo.toml --all --check
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
& $py -m pytest tests/test_scanner_contract.py -q
```

**Expected commit:**

```text
Create Rust scanner workspace and contract
```

**Rollback:** Revert the workspace commit and restore crate-local lock files and old binary paths.

---

### Task 4: Add The Rust Engine Shell And Python Client

**Prerequisite:** Dependency gate approved for Task 4 additions.

**Files:**

- Add `rust/scanner_core/Cargo.toml`
- Add `rust/scanner_core/src/lib.rs`
- Add `rust/scanner_core/src/run.rs`
- Add `rust/scanner_cli/Cargo.toml`
- Add `rust/scanner_cli/src/main.rs`
- Modify `src/models/scanner_contract.py`
- Add `src/services/rust_context_client.py`
- Add `src/services/json_process_client.py` as the new strict request/response subprocess primitive
- Add `src/workers/__init__.py`
- Add `src/workers/contracts.py`
- Add `src/workers/document_parser_worker.py` with `version` implemented and `parse` returning the transitional structured `NOT_IMPLEMENTED`
- Modify `rust/office_parser` to implement the shared requestless worker `version` handshake without changing its active legacy parse command
- Modify `tests/test_scanner_contract.py`
- Add `tests/test_rust_context_client.py`
- Add `tests/test_worker_version_handshake.py`
- Modify `rust/Cargo.toml`
- Leave the old `src/services/rust_cli_contract.py` solely for the still-running legacy helpers; do not reuse it for the v1 context protocol. Task 12 deletes it with those helpers.

**Engine commands:**

- `version`: machine-readable contract and build information
- `doctor`: validates DB parent/basic capability and successfully executes both worker version handshakes
- `build-context`: initially returns a structured `NOT_IMPLEMENTED` error until the core pipeline is complete; it is not wired into production yet
- `inspect-run`: initially read-only placeholder

**Python client tests must cover:**

- UTF-8 stdin/stdout
- timeout
- executable missing
- nonzero exit with valid error JSON
- nonzero exit with invalid JSON
- invalid UTF-8
- request id mismatch
- contract version mismatch
- unknown response fields
- stderr capture without leaking it into user-facing output

**Verification:**

```powershell
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
& $py -m pytest tests/test_scanner_contract.py tests/test_rust_context_client.py tests/test_worker_version_handshake.py -q
```

**Expected commit:**

```text
Add Rust context engine shell
```

**Gate:** Production still uses the existing Python chain.

---

### Task 5: Move Discovery, Classification, And Light Text Parsing Into Rust

**Files:**

- Add `rust/scanner_core/src/config.rs`
- Add `rust/scanner_core/src/classifier.rs`
- Add `rust/scanner_core/src/planner.rs`
- Add `rust/scanner_core/src/parsers/mod.rs`
- Add `rust/scanner_core/src/parsers/light_text.rs`
- Modify `rust/discovery/src/lib.rs` for reuse without changing current helper output
- Add `rust/scanner_core/tests/windows_discovery.rs`
- Add `rust/scanner_core/tests/light_text.rs`

**Required behavior:**

- Closed date range.
- Allowed extensions, ignored patterns, excluded directories.
- Chinese, spaces, drive-letter casing, UNC, and long-path normalization.
- Stable deterministic ordering.
- Preserve the existing discovery-library semantics exactly: `file_identity` is `bootstrap:` plus the library's normalized, case-folded absolute resolved path; `source_version` is ASCII `mtime_ns=<nonnegative integer>:size=<nonnegative integer>` and its size must equal discovered `size_bytes`. Freeze Windows drive/UNC/long-path fixtures before refactoring; v1 does not introduce a different identity algorithm or content hash.
- Bounded head/tail reads and UTF-8 handling for `.txt/.md/.csv/.json/.log`.
- Large-file guard before parser execution.
- Explicit diagnostics for unreadable entries; no stderr-only omission.

`planner.rs` in this task is a pure deterministic classifier/budget planner over discovered candidates. It performs no SQLite access and knows nothing about cache hits. Task 7 adds cache-aware scheduling around this pure planner; it must not reimplement the classification rules.

**Verification:**

```powershell
cargo test --manifest-path rust/Cargo.toml -p ai-daily-scanner-core --locked
cargo test --manifest-path rust/Cargo.toml --workspace --locked
```

**Expected commit:**

```text
Move discovery and text parsing into Rust core
```

---

### Task 6: Move Parser Routing And Worker Deadlines Into Rust

**Prerequisite:** Dependency gate approved for Task 6 additions, including `rayon` and the Windows API binding used by process containment.

**Files:**

- Add `rust/scanner_core/src/process.rs`
- Add `rust/scanner_core/src/windows_job.rs`
- Add `rust/scanner_core/src/parsers/office.rs`
- Add `rust/scanner_core/src/parsers/document.rs`
- Add `rust/scanner_core/src/fallback.rs`
- Modify `src/workers/contracts.py`
- Modify `src/workers/document_parser_worker.py` to replace the transitional parse error with the strict worker parse implementation
- Add `tests/test_document_parser_worker.py`
- Add Rust worker contract/fault-injection tests
- Extend Office and Python worker version-handshake tests to prove they run before cache lookup
- Modify `rust/office_parser` to consume shared contract types where practical
- Reuse `src/services/document_parser.py` as implementation behind the worker
- Migrate the existing `.xls` fallback and `.doc/.ppt` SharePoint-text fallback from `file_scanner.py`/`office_parser.py` into the worker-owned adapter before either legacy module is deleted

Define the Python worker-only parsed payload as `WorkerParsePayload` in `src/workers/contracts.py`; do not reuse the legacy `FileContext` name. The final application-level `src/models/schemas.py` must not remain the source of truth for Rust scanner internals.

**Routing ownership:**

- Rust text parser for text-like files.
- Rust Office worker for `.xlsx/.docx/.pptx`.
- Python document worker for PDF and explicitly enabled legacy formats.
- Python document worker for modern `.xlsx/.docx/.pptx` fallback only when the Rust failure class and configured policy permit it.
- Rust chooses the route, starts the process, owns the deadline, validates the response, and records all audit fields.
- Before discovery or cache lookup, Rust completes the requestless version handshake for every worker that the normalized routing policy could use. The resulting contract/build fingerprints are immutable for that run and feed the route-specific cache hash. A parse response whose build differs from the preflight handshake fails with a contract diagnostic and is not cached.

**Deadline rule:**

```text
one file deadline = primary attempt + any permitted fallback
```

Fallback never receives a new full timeout. On Windows, child workers must be placed in a Job Object or equivalent verified process-tree containment so timeout cleanup leaves no orphan processes.

The Windows implementation is not allowed to use a spawn-then-assign race. With target-specific `windows-sys`, it must create the worker suspended, create a non-inheritable Job handle with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, assign the process to the Job, then resume the primary thread. Timeout calls `TerminateJobObject` and waits for process-tree termination before returning. Tests must cover scanner termination by the Python outer watchdog, nested CI runner Job behavior, and a worker that spawns a grandchild. Assignment failure is explicit; it must not silently fall back to an uncontained process.

Use `cfg(windows)` for the Job Object backend and `cfg(not(windows))` for a compile-compatible non-production process backend. Linux compatibility runs pure/unit contract tests but does not claim production-equivalent worker-tree containment or run Windows worker E2E.

**Failure classes:**

- `deterministic`
- `environment_unavailable`
- `contract_failure`
- `recoverable_parser_failure`

Fallback mapping is explicit:

- `deterministic`: no fallback, including timeout by default and deterministic corrupt ZIP.
- `environment_unavailable`: Python Office fallback may run when enabled and time remains.
- `contract_failure`: Python Office fallback may run when enabled and time remains; the run is at least `partial`.
- `recoverable_parser_failure`: Python Office fallback may run according to configured order and remaining time.
- Timeout fallback is disabled by default; if explicitly enabled it may consume only the remaining file deadline.
- Any actual fallback or unrecovered per-file parse error makes the envelope `partial` when a trustworthy context still exists; it is never hidden inside `ok`.

**Verification must include:**

- valid `.xlsx/.docx/.pptx`
- corrupt ZIP
- worker missing
- invalid JSON
- wrong path/backend in response
- sleep past deadline
- worker crash
- fallback with remaining time
- timeout default no-fallback
- no orphan worker process on Windows
- explicitly enabled `.xls/.doc/.ppt` fixtures proving no legacy-format capability is lost

**Expected commit:**

```text
Move parser scheduling and deadlines into Rust core
```

---

### Task 7: Move Inventory, Parse Cache, Planner, And Metrics Into Rust

**Prerequisite:** Approval for Task 7 additions, including `rusqlite` and `sha2`.

**Files:**

- Add `rust/scanner_core/src/store/mod.rs`
- Add `rust/scanner_core/src/store/schema.rs`
- Add `rust/scanner_core/src/store/inventory.rs`
- Add `rust/scanner_core/src/store/cache.rs`
- Add `rust/scanner_core/src/metrics.rs`
- Add `rust/scanner_core/build.rs`
- Add Rust tests for schema, transactions, cache, concurrency, and recovery

**Database requirements:**

- New file `scan_index_v2.sqlite3`; do not modify the old DB.
- `PRAGMA user_version` with explicit migrations.
- Foreign keys enabled.
- WAL mode where supported.
- Bounded busy timeout.
- A scan run is created as `running`; persisted terminal states are `success|partial|error`, and recovery may mark an interrupted nonterminal run `abandoned` before a same-request restart.
- Parse-cache errors are not treated as fresh successful content.
- Python never writes this DB.

Freeze the v1 DDL in `rust/scanner_core/src/store/schema.rs` and a matching design table before implementation:

| Table | Required identity/relationship |
|---|---|
| `engine_lease` | singleton row with `owner_id`, PID, acquired/heartbeat/expiry timestamps |
| `scan_runs` | integer PK, unique `request_id`, canonical logical-request JSON, hash algorithm/hash, owner id, status `running|success|partial|error|abandoned`, timestamps, final envelope JSON nullable only for the nonterminal/recoverable `running|abandoned` states |
| `scan_run_attempts` | PK `(scan_run_id, attempt_number)`, owner id, normalized DB/adapter paths, engine fingerprint, worker contract/build fingerprints nullable only until/when that handshake fails, started/finished timestamps, attempt status |
| `run_diagnostics` | PK `(scan_run_id, sequence)`, full `Diagnostic` fields and severity; stores discovery/run warnings even when no file identity exists |
| `file_inventory` | PK `file_identity`, path/type/source version/size/mtime, last-seen run FK |
| `parse_cache` | PK `(file_identity, source_version, parse_profile_hash)`, success content/backend/truncated/version/timestamp |
| `scan_file_results` | PK `(scan_run_id, file_identity)`, cache/parser/lane/status/durations/fallback/error audit |
| `scan_stage_metrics` | PK `(scan_run_id, stage)`, count/duration fields needed by `inspect-run` |
| `scan_extension_metrics` | PK `(scan_run_id, extension)`, counts and durations |
| `context_runs` | integer PK, unique scan-run FK, profile hash, status, final context, context hash, counts |
| `context_decisions` | PK `(context_run_id, file_identity)`, action/reason/priority/input/output/truncated/error |

All FKs are explicit and indexed; run-status/time indexes support diagnosis. Schema and every migration run in a transaction. Add a migration test from each committed `user_version`, not only a fresh-create test.

#### Run ownership and idempotency

- Allow one active writer per scan DB in v1.
- Acquire/update `engine_lease` under `BEGIN IMMEDIATE`; the owner is a random UUID and heartbeats during long parsing.
- A live unexpired lease yields structured `SCAN_ALREADY_RUNNING`; do not mark another run abandoned merely because a second process starts.
- Only an expired lease whose heartbeat is older than the documented grace period may be reclaimed; reclaim marks that owner's `running` runs `abandoned` before taking ownership.
- Heartbeat interval and lease grace are explicit contract constants; grace is at least three heartbeat intervals and comfortably exceeds the DB busy timeout. A slow but heartbeating run must never be reclaimed. After the Python outer watchdog kills a wedged engine, the lease becomes reclaimable only after heartbeat expiry.
- `scan_runs.request_id` is unique. A retry with the same request id and identical hash returns the stored canonical terminal envelope byte-for-byte after schema validation; the same id with a different hash yields `REQUEST_ID_CONFLICT`; a live same-id run yields `REQUEST_IN_PROGRESS`.
- If a reclaimed row is `abandoned`, the same request id/hash may atomically move that row back to `running` under the new owner, append a new attempt using the current runtime fingerprints, and restart from the beginning; clear any noncanonical staging rows first. Different request ids create new rows. An abandoned row is never returned as a successful idempotent result.
- Parsing happens outside the final write transaction. Inventory changes, successful cache writes, file metrics, run diagnostics, stage/extension metrics, context run/decisions, canonical final envelope, terminal run/attempt status, and deletion of the matching `engine_lease` row commit in one transaction guarded by `owner_id`. A crash before that transaction leaves no reusable parse result and the lease expires normally.

#### Canonical request and envelope persistence

After strict request/profile validation, build `canonical_request_json` from this exact logical typed object with fixed field order:

```text
contract, protocol_version,
normalized absolute work_dir, start_date, end_date, report_mode,
normalized scanner profile, normalized context profile
```

Exclude `request_id` and all runtime-only infrastructure: `scan_db_path`, adapter paths, executable versions, and engine/worker builds. No secret or environment dump is present. Serialize typed UTF-8 JSON with sorted map/set keys, then compute `request_hash = SHA-256("request-v1\\0" + canonical_request_json bytes)` and store algorithm `sha256-request-v1`. Semantically identical reordered raw objects therefore hash identically; a different work directory, date range, report mode, protocol, or normalized profile conflicts under a reused id, while a release/build/path change alone does not.

Open the DB and check request-id/hash state before worker handshakes. A matching terminal row returns its stored envelope immediately, even if the active release or worker paths/builds have changed. For a new or abandoned run, one `BEGIN IMMEDIATE` transaction rechecks the id/hash and lease, creates/reactivates the run, and appends an attempt with current normalized DB/adapter paths and engine fingerprint. Then perform both current worker handshakes and fill their attempt fingerprints before discovery/cache lookup. A handshake failure finalizes a stored error envelope and attempt with the unavailable worker fingerprint left null. Attempt fingerprints feed cache keys and audit, never logical request identity.

`final_envelope_json` is the exact canonical `ContextEnvelope` emitted to Python, including summary, warnings, full error diagnostic, and nullable run ids. A completed retry reads, validates, and returns it rather than reconstructing an envelope using the current binary. `inspect-run` reads this envelope plus normalized relational metrics/diagnostics; relational rows and the envelope must agree in tests.

Errors before the DB can be opened or before a `scan_runs` row can be created (`INVALID_REQUEST` or `CACHE_OPEN_FAILED`, for example) can return a valid error envelope but cannot promise persisted idempotency. A finalization/write failure returns `CACHE_WRITE_FAILED`, releases resources by process/lease expiry, and reruns on retry because no terminal envelope was committed; document this exception rather than claiming it was stored.

**`parse_profile_hash` input must include:**

- exactly the Section 3.2 formula: protocol version, route-specific stack contract/build fingerprint, max-file guard, default/per-extension timeout, and the complete selected normalized `parse` object (including all budgets, backend, aggregate cap, and fallback policy/order)
- no raw/default-order JSON and no post-cache worker response value

The parse-cache primary key separately includes `file_identity`, `source_version`, and `parse_profile_hash`; do not duplicate file identity/source version inside the global profile hash.

Build fingerprints are not free-form version labels. CI/deploy supplies the Git commit as `AI_DAILY_BUILD_ID` for clean Rust builds. Local Rust builds use a deterministic build-script hash over the contract/core/discovery/Office source inputs and workspace lock file. The Python worker computes `worker_build` before serving requests as SHA-256 over sorted `(repository-relative UTF-8 path, file bytes)` entries for `src/workers/contracts.py`, `src/workers/document_parser_worker.py`, `src/services/document_parser.py`, every directly used Python parser helper retained after Task 12, and `requirements.lock`; it excludes absolute paths, mtimes, caches, settings, and secrets. The exact allowlist is a contract fixture and test, so adding a parser source without adding it to the fingerprint fails CI. Office and Python worker handshakes expose these fingerprints before cache lookup; changing any applicable member of a route stack invalidates affected entries. Do not use only executable path, package version, or a post-parse response as the final v2 fingerprint.

**Cache key must not include:**

- API keys
- LLM provider
- work directory root
- DB path
- worker count, unless it changes content semantics

**Cache tests:**

- first scan miss, second scan hit
- one-file modification reparses exactly one file
- timeout/profile/backend/build change invalidates cache
- same path with changed size/mtime invalidates cache
- error result retries on the next run
- DB lock produces a structured error
- transaction failure cannot leave a false successful run
- concurrent second invocation receives `SCAN_ALREADY_RUNNING` without changing the first run
- stale lease recovery works, while a live heartbeat is never reclaimed
- same request-id retry is idempotent and different-payload reuse is rejected
- terminal retry returns the byte-identical stored envelope even after the executable build changes
- abandoned same-id/same-hash restarts cleanly; finalization atomically clears its lease
- run-level discovery diagnostics and warnings round-trip through `inspect-run`

**Expected commit:**

```text
Move scanner index and cache into Rust core
```

---

### Task 8: Move Decisions, Aggregation, Compression, And Context Audit Into Rust

**Files:**

- Add `rust/scanner_core/src/decision.rs`
- Add `rust/scanner_core/src/compressor.rs`
- Add `rust/scanner_core/src/context_audit.rs`
- Add golden tests mirroring current Python behavior
- Modify `rust/scanner_core/src/run.rs`
- Implement `build-context` and `inspect-run`

**Required design change:**

Do not translate both current budget layers literally. Replace `ScanAggregator` plus `ContextCompressor` with one deterministic budgeting pipeline:

1. Normalize and sort file evidence.
2. Assign priority and action.
3. Apply per-file budget once.
4. Apply global context budget once.
5. Produce final context and normalized decisions.
6. Persist context run and decisions in one transaction.

**Golden cases:**

- `keep`
- `compress`
- `metadata_only`
- `omit`
- `error`
- large log tail
- Office/PDF priority
- parse error priority
- global budget exhaustion
- deterministic path-order tie break
- truncated source
- unreadable file size

**Response requirements:**

- `file_context` is ready for the existing LLM prompt.
- `summary` agrees with persisted run/decision rows.
- `worker_lane` and `cache_status` use real scanner evidence, never `unknown` when the engine knows them.
- `inspect-run` returns stable DTOs and does not expose table schema.

**Expected commit:**

```text
Move context decisions and compression into Rust core
```

**Gate:** `ai-daily-scanner.exe build-context` must now be complete on the synthetic fixture corpus, but production remains on Python legacy.

---

### Task 9: Add Explicit Legacy/Rust Adapters And Shadow Comparison

**Files:**

- Add a private `ContextEngine` protocol beside `ContextScheduler` or in a narrowly scoped module
- Add `src/services/python_legacy_context_engine.py`
- Modify `src/services/rust_context_client.py`
- Modify `src/services/context_scheduler.py`
- Modify `main.py`
- Modify `src/core/config.py`
- Modify `src/models/scanner_contract.py`
- Add `scripts/compare_context_engines.py`
- Add `tests/test_context_builder.py`
- Add `tests/test_context_engine_comparison.py`
- Modify `scripts/benchmark_scanner.py` to read Rust `inspect-run` DTOs without SQL access
- Modify `scripts/benchmark_context_scheduler.py` and `tests/test_benchmark_context_scheduler.py` to use the new application summary/engine seam without importing `FileScanner` or reading the old scan store
- Modify `src/services/__init__.py` so it does not re-export scanner internals after cutover

**Temporary configuration:**

```yaml
scanner:
  engine: "python_legacy"  # temporary: python_legacy | rust_v2
  rust_scanner_bin: "rust/target/release/ai-daily-scanner"
  rust_index_db_path: "data/db/scan_index_v2.sqlite3"
```

**Rules:**

- One production run selects exactly one complete adapter.
- `rust_v2` failure does not call `python_legacy`.
- Shadow comparison is an explicit script/test command only.
- Shadow uses separate temporary DBs.
- Shadow never calls the LLM or report renderer.
- The legacy implementation is frozen except for critical correctness fixes.
- In `rust_v2`, `ContextScheduler` must stop building decisions, compressing content, or writing scan/context audit. It validates the application request, calls `RustContextClient`, maps `ContextEnvelope` into the new `ContextBuildResult`, and returns the Rust run ids.
- `ContextScheduler` is the one application interface. Do not expose another public `ContextBuilder` with the same method shape; `RustContextClient` and the temporary legacy engine are internal adapters.
- Configure a Python-side whole-process watchdog for a wedged Rust process. It is an operational upper bound, does not replace Rust per-file deadlines, and never enables fallback.
- `compare_context_engines.py` must support `--redact-content` and `--ephemeral-db-root`. Redacted mode writes only hashes/metadata; both engine DBs and all sidecars stay under the verified ephemeral root. The caller owns try/finally cleanup.

**Main/LLM status tests:**

- `ok`: LLM fake called once.
- `partial`: warning rendered and LLM fake called once.
- `error`: command returns failure and LLM fake call count is zero.
- Missing executable, timeout, invalid JSON, nonzero exit, and contract mismatch all reach the `error` case without legacy fallback.

**Downstream compatibility tests:**

- daily scan returns `ContextBuildResult`; `main.py` reads its summary/status instead of `scan_result`
- weekly/monthly `--source scan` consume Rust context
- weekly/monthly `--source db` remain unchanged
- LLM tests use fakes and receive a non-empty final context
- report rendering and report DB tests remain unchanged

**Expected commit:**

```text
Add explicit Rust context adapter and shadow comparison
```

---

### Task 10: Pass The Parity, Fault, And Performance Gate

**Commit:** Evidence/test changes may be committed; `%TEMP%` benchmark artifacts are not committed.

#### Synthetic corpus

Create or generate sanitized fixtures for:

- `.txt`, `.md`, `.csv`, `.json`, `.log`
- `.xlsx`, `.docx`, `.pptx`
- `.pdf`
- corrupt Office ZIP
- oversized file
- slow worker fixture
- Chinese and space-containing paths

#### Parity requirements

Compare the same boundary on both sides: complete legacy `ContextScheduler` build through Python compression versus complete Rust `build-context` through Rust compression. Exclude LLM and report rendering from both. Do not compare a legacy scanner-only duration to a Rust scanner-plus-compressor duration.

- Discovered file sets are identical, with every difference explained.
- Success/error/timeout counts are identical unless an intentional v2 decision is documented.
- Same parser backend produces identical normalized content.
- Text and PDF normalized content hashes match.
- File decisions and ordering match, except for the explicitly approved double-budget consolidation.
- Final context stays within budget and is deterministic across repeated runs.
- All intentional output differences have a golden fixture and ADR note.

#### Cache requirements

- Cold run reparses expected files.
- Immediate warm run reuses every unchanged successful file.
- Modifying one fixture yields `reparsed_count == 1`.
- Changing any semantic profile field invalidates the expected cache entries.

#### Fault injection

- scanner executable missing
- contract mismatch
- malformed request/response JSON
- Office worker missing
- Python worker missing
- worker timeout and crash
- corrupt workbook
- unreadable entry
- SQLite lock
- interrupted transaction

Every failure must be explicit; none may trigger top-level Python fallback.

#### Performance gate

Capture at least five cold and five warm runs per engine on the same Windows machine and fixture corpus:

- cold median must not regress by more than 10%
- cold p95 must not regress by more than 20%
- warm median must not regress
- no freeze or orphan process
- parser backend and worker lane evidence must be present

Each cold sample must use a distinct verified temporary DB path. Each warm sample must immediately reuse the matching cold sample's DB. Do not clear or recursively delete the production DB to manufacture a cold run.

Do not use the single historical 158 ms/28 ms observations as the only performance evidence; they are reference points, not a statistically sufficient gate.

#### Real-directory scanner-only gate

Use the local directory supplied in `AI_DAILY_REAL_WORK_DIR` and the existing
ignore rule. Do not call the LLM. Real-directory mode must never serialize
`file_context`, excerpts, cell values, or cache contents into comparison JSON;
it may emit only hashes, counts, backend/lane names, durations, file-relative
identifiers, and difference metadata.

```powershell
$ErrorActionPreference = 'Stop'
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
if ([string]::IsNullOrWhiteSpace($env:AI_DAILY_REAL_WORK_DIR)) {
  throw 'AI_DAILY_REAL_WORK_DIR must name the local scanner-only directory'
}
if ([string]::IsNullOrWhiteSpace($env:AI_DAILY_REAL_SAMPLE_FILE)) {
  throw 'AI_DAILY_REAL_SAMPLE_FILE must name a local scanner-only sample'
}
$sample = Get-Item -LiteralPath $env:AI_DAILY_REAL_SAMPLE_FILE
$scanDate = $sample.LastWriteTime.ToString('yyyy-MM-dd')
$tempBase = [System.IO.Path]::GetFullPath($env:TEMP)
$tempRoot = Join-Path $tempBase ("ai-daily-real-compare-" + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $tempRoot
try {
  & $py scripts\compare_context_engines.py `
    --work-dir $env:AI_DAILY_REAL_WORK_DIR `
    --start-date $scanDate `
    --end-date $scanDate `
    --redact-content `
    --ephemeral-db-root $tempRoot `
    --output (Join-Path $tempRoot 'comparison.json')
  if ($LASTEXITCODE -ne 0) { throw "engine comparison failed: $LASTEXITCODE" }

  $comparison = Get-Content -LiteralPath (Join-Path $tempRoot 'comparison.json') -Raw | ConvertFrom-Json
  if ($comparison.PSObject.Properties.Name -contains 'file_context') {
    throw 'redacted comparison unexpectedly contains file_context'
  }
  [pscustomobject]@{
    inventory_difference_count = $comparison.inventory_difference_count
    content_hash_difference_count = $comparison.content_hash_difference_count
    fallback_count = $comparison.fallback_count
  } | ConvertTo-Json
}
finally {
  $resolvedTempRoot = [System.IO.Path]::GetFullPath($tempRoot)
  $insideTemp = $resolvedTempRoot.StartsWith(
    $tempBase.TrimEnd('\') + '\',
    [System.StringComparison]::OrdinalIgnoreCase
  )
  if (-not $insideTemp) { throw "refusing to remove non-temp path: $resolvedTempRoot" }
  Remove-Item -LiteralPath $resolvedTempRoot -Recurse -Force
}
```

Expected:

- legacy and Rust observe the same inventory snapshot after ignore rules; the current known state is one non-backup workbook, but the script must compare the live snapshot rather than hard-code that count
- zero unexplained inventory differences
- Rust engine reports `.xlsx = rust_xlsx_bounded_v1`
- zero top-level fallback
- no content leaves the machine or remains in `%TEMP%`

**Cutover gate:** All parity, cache, fault, real-directory, and performance criteria must pass. Otherwise stop on `python_legacy` and report the exact failed criterion.

---

### Task 11: Make Windows Rust Core The Production Default

**Files:**

- Modify `config/settings.example.yaml`
- Modify `src/core/config.py`
- Modify `src/core/healthcheck.py`
- Modify `main.py`
- Modify `scripts/deploy_windows.ps1`
- Modify `.github/workflows/ci.yml`
- Add `tests/test_windows_rust_core_e2e.py`
- Update relevant healthcheck, main, and config tests

**Default configuration:**

```yaml
scanner:
  engine: "rust_v2"
  rust_scanner_bin: "rust/target/release/ai-daily-scanner"
  rust_index_db_path: "data/db/scan_index_v2.sqlite3"
```

**Doctor:**

Add `python main.py doctor --strict` or an equivalent explicit production mode. It must fail if:

- Rust scanner binary is missing or cannot start
- Office worker is missing or cannot start
- contract version mismatches
- work directory is invalid
- scan DB parent is not writable
- Python worker cannot be invoked when configured formats require it

Strict doctor validates the effective loaded configuration. It must not require that the values come from a particular local YAML filename, and it must never print API-key material.

**Windows deployment script:**

- Build the Rust workspace by default.
- Task 11 is source-build only. Do not expose a prebuilt mode until Task 13 defines the manifest, integrity/provenance policy, staging validation, and rollback layout.
- Always finish with strict doctor.
- Preserve existing local settings and `.venv` idempotently.
- Do not accept, copy, or print API keys.

**Windows CI:**

One `windows-production` job must, in the same job:

1. Install Python dependencies.
2. Run Cargo fmt/clippy/test.
3. Build the release workspace.
4. Prepare a non-secret temporary Windows config.
5. Run strict doctor.
6. Run the full Python test suite.
7. Run Chinese/space path E2E.
8. Run cold/warm cache E2E.
9. In a clean staging checkout, run the real `deploy_windows.ps1` twice and prove the second run is idempotent; hash the non-secret temporary config before/after and prohibit any LLM call.

Linux becomes a clearly named compatibility job or scheduled/manual workflow. It does not produce production release artifacts.

**Expected commit:**

```text
Make Rust core the Windows production default
```

**Rollback before legacy deletion:** Explicitly set `scanner.engine=python_legacy` or revert the default-switch commit. Do not implement automatic fallback.

---

### Task 12: Delete The Python Legacy Scanner Core

Execute immediately after Task 11 acceptance; do not leave two production cores indefinitely.

**Delete after confirming no remaining production imports:**

- `src/services/cold_scanner_run.py`
- `src/services/context_compressor.py`
- `src/services/file_scanner.py`
- `src/services/light_text_parser.py`
- `src/services/office_parser.py`
- `src/services/scan_aggregator.py`
- `src/services/scan_discovery.py`
- `src/services/scan_index_inventory.py`
- `src/services/scan_index_models.py`
- `src/services/scan_index_schema.py`
- `src/services/scan_index_store.py`
- `src/services/scan_metrics.py`
- `src/services/scan_planner.py`
- `src/services/scan_worker_pool.py`
- `src/services/scanner_items.py`
- `src/services/scanner_parse_cache.py`
- `src/services/rust_cli_contract.py` after its legacy discovery/Office consumers are gone; the v1 client uses `json_process_client.py`
- `src/services/python_legacy_context_engine.py`
- `scripts/compare_context_engines.py`
- legacy-only tests for these implementations
- `tests/test_context_compressor.py`
- `tests/test_context_engine_comparison.py`

Also remove CI, `README.md`, and active architecture/backend documentation references to the migration-only comparison command. Preserve parity evidence only in the verified ephemeral evidence bundle and commit history; do not leave an executable entry point that imports the deleted legacy adapter.

**Keep:**

- `src/services/context_scheduler.py`, reduced to application orchestration
- `src/services/rust_context_client.py`
- `src/models/scanner_contract.py`
- `src/services/document_parser.py`, only behind the Python worker adapter
- `src/workers/document_parser_worker.py`
- `src/core/llm.py`
- `src/services/sqlite_store.py`
- `src/services/report_gen.py`

Also remove `FileContext` and `ScanResult` from `src/models/schemas.py` if no non-legacy consumer remains. If the Python document worker still needs equivalent fields, it must use the worker contract model introduced in Task 6 rather than keeping application scanner models alive accidentally.

Delete `main.py::build_file_context`, its `ScanResult` import, and any code path that converts legacy `ScanResult.contexts` into text. Update `src/models/__init__.py`, `src/services/__init__.py`, `tests/test_schemas.py`, `tests/test_context_scheduler.py`, benchmark tests, and all import sites in the same commit. Do not leave a passing test suite that imports deleted symbols only behind skipped tests.

**Rust cleanup:**

- Remove the standalone discovery binary if no consumer remains; retain its library.
- Retain the Office parser binary as an internal crash-isolated worker.
- Remove temporary `NOT_IMPLEMENTED`, shadow, and legacy contract branches.

**Configuration cleanup:**

- Remove `python_legacy` engine mode.
- Remove Python-only `worker_lane_mode`.
- Remove standalone discovery binary config.
- Remove unused `office_external_fallback`.
- Either implement `office_legacy_extensions_enabled` in Rust or delete it in favor of `allowed_extensions`; do not leave a no-op key.
- Keep only keys actually consumed by the Rust engine or application shell.

**Static deletion gate:**

```powershell
rg -n "FileScanner|ColdScannerRun|ScanPlanner|ScanIndexStore|ContextCompressor|ScanResult|FileContext|ScanAggregator|FileDiscoveryService|ParserSupervisor|python_legacy|rust_cli_contract" main.py src scripts tests config README.md docs/scanner-backends.md
rg -n "services\.(cold_scanner_run|context_compressor|file_scanner|light_text_parser|office_parser|scan_aggregator|scan_discovery|scan_index_|scan_metrics|scan_planner|scan_worker_pool|scanner_items|scanner_parse_cache|rust_cli_contract)" main.py src scripts tests config README.md docs/scanner-backends.md
```

Expected: no production references. Historical files under `docs/superpowers/**` may retain old names.

**Expected commit:**

```text
Remove the Python legacy scanner core
```

**Rollback:** Revert this deletion commit and the default-switch commit. Do not add a new compatibility shim.

---

### Task 13: Finish Windows Release Packaging And Active Documentation

**Files:**

- Add `scripts/package_windows.ps1`
- Add `scripts/verify_windows_package.ps1`
- Add `scripts/install_windows_release.ps1`
- Add `scripts/rollback_windows_release.ps1`
- Add `scripts/run_current_release.ps1`
- Modify `src/core/config.py`
- Modify `src/core/logger.py`
- Modify `src/core/healthcheck.py`
- Modify `tests/test_config.py`
- Modify `tests/test_logger.py`
- Modify `tests/test_healthcheck.py`
- Add `tests/test_windows_release_package.py`
- Add `.github/workflows/windows-release.yml`
- Add `docs/windows-deployment.md`
- Rewrite active Windows instructions in `README.md`
- Rewrite active scanner architecture in `docs/scanner-backends.md`
- Update `config/settings.example.yaml`
- Do not rewrite historical `docs/superpowers/**` records

**Release package:**

```text
ai-daily-report-windows-x64/
  rust/target/release/ai-daily-scanner.exe
  rust/target/release/ai-daily-office-parser.exe
  Python source
  templates/
  requirements.lock
  config/settings.example.yaml
  scripts/deploy_windows.ps1
  scripts/verify_windows_package.ps1
  scripts/install_windows_release.ps1
  scripts/run_current_release.ps1
  scripts/rollback_windows_release.ps1
  manifest.json
  SHA256SUMS
```

**Installed layout and production rollback:**

```text
<install-root>/
  current.json                 # atomically replaced pointer
  releases/
    <version-a>/               # previous verified package
    <version-b>/               # new verified package
  shared/
    config/settings.windows.yaml
    data/
    logs/
  run_current_release.ps1
  rollback_windows_release.ps1
```

- Install into a new version directory, validate it there, then atomically replace `current.json`; never overwrite the active release in place.
- Local config, report data, scan data, and logs live outside version directories. The launcher supplies absolute config/data/report/database/log paths and an explicit working directory, so behavior does not depend on caller cwd.
- Keep at least the previous verified package. Rollback validates the previous manifest and strict doctor, then atomically points `current.json` back.
- For this migration the old Python release keeps its old scan DB and Rust v2 keeps `scan_index_v2.sqlite3`, so rollback requires no destructive DB downgrade. Future incompatible v2 schema changes require a separate DB migration/backup policy before release.
- Git revert remains the source-checkout rollback; side-by-side pointer rollback is the installed-package rollback.

**External runtime path contract:**

- `DAILY_REPORT_INSTALL_ROOT`: absolute installation root; its presence enables installed-mode containment checks.
- `DAILY_REPORT_CONFIG_DIR`: absolute directory containing `settings.yaml`, `settings.windows.yaml`, and optional `.secrets.yaml`.
- `DAILY_REPORT_DATA_DIR`: absolute shared data root.
- `DAILY_REPORT_REPORTS_DIR`: absolute shared Markdown report directory.
- `DAILY_REPORT_DB_DIR`: absolute shared SQLite directory; Python report DB and Rust `scan_index_v2.sqlite3` are resolved below this directory.
- `DAILY_REPORT_LOG_DIR`: absolute shared log directory.
- Installed launch requires all six variables and rejects a relative, missing, non-directory, or unexpectedly version-local value. Source-checkout development without these variables retains the documented repository-relative behavior.
- `Config` resolves the five locations once and exposes absolute paths. `logger.setup_logger()` uses the resolved log directory instead of a module-root default. `collect_healthcheck()` reports the resolved configuration source and all runtime directories and, in strict installed mode, treats any path escaping `<install-root>/shared` as an error.
- `run_current_release.ps1` reads and schema-validates `current.json`, resolves the selected release without following an untrusted relative escape, sets the six variables, sets `current_dir` to the selected release, and invokes that release's `.venv\\Scripts\\python.exe` without a shell. The Rust request receives absolute worker/module/binary/database paths derived from these resolved values.
- The launcher never copies or rewrites local settings or `.secrets.yaml`. A release switch and rollback therefore reuse exactly the same shared configuration and data.

Path tests must cover drive-letter paths, spaces and non-ASCII characters, an invocation from outside the repository/install root, relative-value rejection, `..` escape rejection, missing shared directories, log placement, report/DB placement, strict doctor output, and preservation of shared config/data across a pointer rollback.

`manifest.json` must include:

- Git commit
- target triple
- Rust engine version/build
- contract version
- Cargo.lock hash
- a canonical, case-sensitive allowlist of every payload file with normalized relative path, byte size, and SHA-256; this covers Rust executables, every Python source file, templates, `requirements.lock`, example config, and every packaged PowerShell script

`manifest.json` excludes itself and `SHA256SUMS` to avoid a circular hash. `SHA256SUMS` covers `manifest.json` plus every manifest-listed payload and excludes only itself. The archive entry set must equal exactly `{manifest.json, SHA256SUMS} + manifest payload`; reject missing or additional entries, duplicate/case-colliding names, absolute/drive/UNC paths, `..` traversal, alternate data stream names, symlink/reparse entries, and files whose size or hash differs. Validate target, build, engine/worker handshake, and contract version only after structural and hash validation succeeds.

The initial verifier/installer is a trusted bootstrap outside the untrusted archive: a script from the source checkout, a previously verified installation, or an independently authenticated distribution. It first treats the archive only as data, validates entry names before extraction, extracts into a GUID staging directory, verifies the exact allowlist and all hashes, and only then executes or copies any packaged PowerShell/Python code. The package contains verified copies of install/launch/rollback scripts for subsequent local operation, but documentation must never tell users to run the copy inside an unverified archive. V1 does not claim that matching hashes prove publisher identity.

Do not commit binaries or `target/`. A prebuilt package must fail validation if any payload file, entry set, hash, target, build, or contract version is wrong.

SHA-256 values shipped beside the binaries provide corruption/integrity detection, not publisher authenticity. V1 must not auto-download a prebuilt artifact. A remotely downloaded artifact may be called trusted only after an independent anchor is implemented and verified, such as Authenticode or GitHub artifact attestation/provenance tied to the expected repository/tag. Validate an artifact in an isolated staging directory before installing or switching the pointer.

**Clean installed-package E2E (mandatory in the Windows release workflow):**

1. Create a GUID install root and synthetic shared settings/data/log directories outside the checkout; do not copy the developer's API key or business files.
2. Build two locally identified packages from the clean checkout and install version A with the trusted bootstrap.
3. From an unrelated cwd, run `run_current_release.ps1 doctor --strict` and a zero-network CLI smoke command; assert logs, report DB, scan DB, and any generated test report stay under `shared/`.
4. Install version B, validate it before the pointer switch, and assert `current.json` selects B while shared config/data hashes remain unchanged.
5. Run the root rollback script, revalidate A, atomically switch back to A, rerun strict doctor/smoke, and assert the shared paths and data hashes are unchanged.
6. Tamper in turn with a Python file, template, lock file, PowerShell script, Rust binary, manifest path, and add an extra archive entry; every case must fail before package code executes or `current.json` changes.
7. Restore any test environment variables and remove only the verified GUID roots in `finally`.

**Documentation state:**

- PowerShell and `.venv\Scripts\python.exe` are the primary commands.
- Windows quick start uses strict doctor.
- Linux is a compatibility appendix with no production promise.
- Active docs contain no `/home/george`, `/tmp`, or Linux-only command as the main path.
- Architecture language says “Python application shell + Rust scanner/context core.”

**Expected commit:**

```text
Document and package the Windows Rust core release
```

**Rollback:** Before any remote publication, atomically point `current.json` to the previous verified release and run strict doctor. Do not delete a published artifact, tag, release, attestation, or branch-protection setting without separate user authorization; record and reverse those external changes explicitly rather than assuming Git revert covers them.

---

## 7. Rollback Matrix

| Task | Runtime rollback | Generated/data handling |
|---|---|---|
| 1 | Revert the boundary-hardening commit | No schema change; old build artifacts are ignored |
| 2 | Revert docs/fixtures | No runtime state |
| 3 | Revert workspace paths/manifests/lock files together | `rust/target` may remain ignored; never rely on stale binaries |
| 4-6 | Revert experimental crates/client/worker commits | Production still uses legacy; remove only verified temporary fixtures if desired |
| 7 | Revert Rust store commit | Leave `scan_index_v2.sqlite3` untouched for audit; it is not the production DB yet |
| 8 | Revert context-core commit | v2 DB may remain; no Python production switch occurred |
| 9 | Set explicit legacy mode and revert adapter commit | Shadow DBs live only under verified temporary roots and are cleaned by callers |
| 10 | No runtime switch to roll back | Delete verified ephemeral comparison roots; do not delete production DBs |
| 11 | Before legacy deletion, explicitly select legacy and revert the default/deploy commit | Old scan DB remains intact |
| 12 | Revert Task 12 and Task 11 together in a source checkout | Do not create a new permanent compatibility shim |
| 13 | Switch the installed release pointer to the previous verified package | Shared config/data stay in place; remote state requires separate authorized reversal |

Every rollback must rerun the focused tests and appropriate doctor. A failed task is not “rolled back” merely because its code was reverted if CI, branch protection, release artifacts, local config, or installed release pointers remain changed.

---

## 8. Verification Matrix

### Every Rust-changing commit

```powershell
cargo fmt --manifest-path rust/Cargo.toml --all --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
```

### Every Python-changing commit

Run the focused tests named by the task, then:

```powershell
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
& $py -m pytest tests -q
& $py -m compileall main.py src tests
```

### Final Windows verification

```powershell
$py = (Resolve-Path -LiteralPath '.\.venv\Scripts\python.exe').Path
& $py main.py doctor --strict
& $py -m pytest tests -q
cargo fmt --manifest-path rust/Cargo.toml --all --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
pwsh -NoProfile -File scripts/deploy_windows.ps1
pwsh -NoProfile -File scripts/deploy_windows.ps1
git diff --check
git status --short --branch
```

Deployment must be idempotent: the second run must not overwrite local configuration, recreate the virtual environment unnecessarily, or alter keys.

### Final runtime evidence

- Strict doctor passes.
- Engine and contract versions are visible without secrets.
- Chinese/space path E2E passes.
- Cold scan succeeds.
- Warm scan proves cache reuse.
- Editing one fixture reparses one file.
- Timeout leaves no worker process.
- No top-level fallback exists.
- `daily`, `weekly --source scan`, and `monthly --source scan` pass with a fake LLM.
- `weekly/monthly --source db` remain unchanged.
- The locally configured real-directory scanner-only run succeeds and
  legacy/Rust observe the same live non-backup inventory snapshot.
- Real business content is not sent externally during migration acceptance.

---

## 9. Stop Conditions

Stop and report rather than continuing if any of these occurs:

- Unrelated user changes overlap a planned file and cannot be preserved safely.
- A production dependency is required but not approved.
- Existing baseline tests fail and the cause is not understood.
- Contract v1 needs a breaking change after the shadow phase starts.
- Rust and legacy inventory differ without an explanation.
- The new cache cannot prove correct invalidation.
- A timeout leaves an orphan process.
- A database transaction can leave a false successful run.
- Performance exceeds the stated regression budget.
- Windows strict deployment cannot be made deterministic.
- Verification would require transmitting business files to an external service without approval.

Do not mark the migration complete merely because `ai-daily-scanner.exe` builds. Completion requires the Python legacy core to be deleted and all final gates to pass.

---

## 10. Definition Of Done

The project may be described as “Python 外围、Rust 核心、Windows-first” only when all statements below are true:

- Python makes one deep `build-context` call and receives final deterministic context.
- Rust owns all scanner decisions, cache, timeout, compression, and audit semantics.
- Python owns no scan-index tables and imports no legacy scanner implementation.
- Missing or incompatible Rust core causes an explicit deployment/runtime failure.
- No top-level Python scanner fallback exists.
- Windows CI builds Rust and executes Python/Rust E2E in the same job.
- Windows deployment builds or verifies both required executables and runs strict doctor.
- Linux is documented and tested only as compatibility.
- All synthetic, fault, cold/warm, and real-directory scanner gates pass.
- The Git worktree is clean after the final commit.

---

## 11. Copy-Paste Handoff Prompt

Use this prompt for the executing Codex:

```text
请严格执行 docs/superpowers/plans/2026-07-15-windows-first-rust-scanner-core.md。

先只做 Task 0：读取 AGENTS.md、计划、现有 ADR 和当前代码；检查并保留工作区改动；建立 codex/windows-rust-scanner-core 分支；运行完整基线并报告结果。Task 0 未通过时不要改代码。

之后按 Task 1 到 Task 13 顺序逐项执行，每个 Task 单独提交、单独验证。不得读取或输出 config/settings.yaml 中的 API Key，不得覆盖本机配置，不得把本机业务目录中的业务文件发送给 LLM。新增 Rust 生产依赖前先向我确认。Rust v2 任何启动、超时、契约或进程错误都必须明确失败，禁止静默回退 Python。只有 Task 10 全部门禁通过后才能切换默认，切换验收后立即删除 Python legacy core。每轮汇报当前 Task、提交、测试证据、剩余门禁和是否允许继续。
```
