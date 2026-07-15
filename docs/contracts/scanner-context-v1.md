# Scanner/context process contract v1

This document freezes the semantic contract for the Windows-first Rust
scanner/context core. JSON field shapes are defined by the sibling
`*.schema.json` files; executable examples are registered in
`tests/fixtures/scanner_contract/v1/fixture-manifest.json`; intentionally
rejected examples live in `invalid-cases.json`.

When sources differ, the strict JSON Schema controls field presence and basic
types, this document controls cross-field semantics, and the golden fixtures
control canonical examples. A contract change must update all three in one
commit and must be versioned when it is not backward compatible.

## Transport

Except for requestless `version` commands, a caller writes exactly one UTF-8
JSON object to stdin and receives exactly one UTF-8 JSON object on stdout.
Human-readable diagnostics use stderr. Any stderr warning that affects context
completeness must also be represented as a structured warning in stdout.

| Exit | Required stdout |
|---|---|
| `0` | A schema-valid `ok` or `partial` response with the exact request id |
| `1` | A schema-valid `error` response with the exact request id |
| `2` | A requestless `TransportErrorResponse` for undecodable input |
| Other/nonconforming | No trusted response; Python constructs `RUST_CORE_CRASHED` |

Python validates contract name, protocol version, status, schema, and request
id before trusting a response. It never silently invokes the legacy scanner on
a transport failure.

All DTOs reject unknown fields. A field described as nullable is still a
required key and contains either its declared value or JSON `null`; omission is
invalid. Integers use strict wire typing, so booleans, strings, and floats do
not satisfy integer fields. Draft 2020-12's data model considers an integral
`4.0` value an integer; the typed Rust/Python parsers therefore enforce the
stricter lexical rule frozen by `profile_integer_float`.

## DTO registry

| DTO | Contract | Schema |
|---|---|---|
| `BuildContextRequest` | `ai_daily_context` | `build-context-request-v1.schema.json` |
| `ContextEnvelope` | `ai_daily_context` | `context-envelope-v1.schema.json` |
| `Diagnostic` | nested | `diagnostic-v1.schema.json` |
| `TransportErrorResponse` | `ai_daily_transport` | `transport-error-v1.schema.json` |
| `VersionResponse` | `ai_daily_context` | `version-response-v1.schema.json` |
| `DoctorRequest` / `DoctorResponse` | `ai_daily_context` | `doctor-*-v1.schema.json` |
| `WorkerVersionResponse` | `ai_daily_worker` | `worker-version-response-v1.schema.json` |
| `WorkerParseRequest` / `WorkerParseResponse` | `ai_daily_worker` | `worker-parse-*-v1.schema.json` |
| `InspectRunRequest` / `InspectRunResponse` | `ai_daily_context` | `inspect-run-*-v1.schema.json` |

Every protocol-bearing object uses `protocol_version: 1`.

## Build context request

`BuildContextRequest` has exactly these top-level keys:

```text
contract, protocol_version, request_id, work_dir, start_date, end_date,
report_mode, compression_profile, scan_db_path, scanner_profile, adapters
```

- `request_id` is a UUID created once per logical build. A transport retry
  reuses it; a new report run creates a new UUID.
- Dates are `YYYY-MM-DD` closed-interval endpoints and `start_date <= end_date`.
- `report_mode` is `daily`, `weekly`, or `monthly`.
- `compression_profile` is required and nullable. A non-null value must be the
  frozen profile for the selected report mode.
- `work_dir`, `scan_db_path`, `office_worker_path`, `python_executable`, and
  `python_module_root` are absolute before serialization.
- `python_document_worker_module` is a dotted Python module name.
- The request never contains a secret, LLM endpoint, prompt, report content,
  environment dump, or scanner-produced file content.

### Raw scanner profile

`scanner_profile` is a flat strict object. Only `schema_version` is required
and equals `scanner_profile_v1`. Every other leaf in
`scanner-profile-request-v1.schema.json` is optional; absence selects the Rust
default and explicit `null` is invalid. Python adds the schema version and
copies only scanner leaves actually present in effective configuration. It
does not inject infrastructure keys or expand defaults.

The following infrastructure-like leaves are specifically forbidden inside
the raw profile: `discovery_backend`, `rust_discovery_bin`,
`rust_office_parser_bin`, `index_db_path`, `worker_lane_mode`, and
`office_external_fallback`.

### Frozen defaults

These values were checked against `config/settings.example.yaml`,
`ScanPlanner.build_parser_profile()`, and
`ContextProfile.for_report_mode()` before being frozen:

| Area | v1 value |
|---|---|
| Discovery | extensions `.xlsx,.xls,.pptx,.pdf,.txt,.md,.docx,.csv,.json,.log`; ignores `~$*,*.tmp`; no excluded dirs |
| Execution | workers `4`; max file `50 MiB`; discovery/file timeout `30/30 s`; `.pdf/.xlsx/.xls` timeout `45/60/60 s`; aggregate chars `50000` |
| Routing | profile `v1`; text `light_text_v1`; Office `rust_office_oxide_v1`; PDF `pdf_text_v1` |
| Fallback | enabled; order `python_office_v1,python_sharepoint_text_v1`; after-timeout `false`; legacy extensions `false`; policy `hybrid_v1` |
| Shared text | head/tail reads `262144/262144` bytes |
| Regular parse | text/excerpt `6000`; PDF pages `5`; Excel sheets/rows/cols `5/50/20`; DOCX paragraphs/tables/table rows/table cols `200/20/50/12`; PPTX slides `50`; notes `true`; document excerpt `6000` |
| Summary parse | text/excerpt `2000`; PDF pages `2`; Excel `2/10/12`; DOCX `80/8/20/8`; PPTX slides `15`; notes `true`; document excerpt `2000` |
| Daily context | `daily_balanced_v1`; global/per-file chars `50000/8000` |
| Weekly context | `weekly_balanced_v1`; global/per-file chars `50000/5000` |
| Monthly context | `monthly_balanced_v1`; global/per-file chars `60000/4000` |
| Context thresholds | `65536/1048576/10485760` bytes; priority `default_v1`; compression `markdown_context_v1` |

Raw validation bounds are encoded in the profile schema: workers `1..64`, max
file `1..4096 MiB`, timeouts `1..3600 s`, read budgets `1..67108864`, character
budgets `1..10000000`, PDF pages `1..10000`, Excel sheets `1..1024`, row
budgets `1..1048576`, column budgets `1..16384`, DOCX paragraphs
`1..1000000`, and DOCX tables/PPTX slides `1..100000`. Arrays contain at most
256 items and contract strings at most 1024 Unicode scalar values. Extensions
are lowercase, start with `.`, contain no separator, colon, or NUL, and are at
most 32 characters.

### Normalized scanner profile

Rust resolves the raw profile into the fully required, non-null shape in
`scanner-profile-normalized-v1.schema.json`. Daily selects regular parser
budgets; weekly and monthly select summary parser budgets. Context thresholds
must satisfy `small < medium < large`, and global context chars must be greater
than or equal to per-file chars.

Canonical UTF-8 JSON uses typed-struct field order. Set-like arrays and timeout
map keys are trimmed, sorted, and deduplicated. Fallback order is trimmed and
deduplicated without reordering because its order is semantic. Worker version
arrays are sorted and unique.

Before cache lookup, Rust validates every worker handshake that a normalized
route may use and freezes its build identity for the run. Route fingerprints
are:

```text
text-like     = engine_build
modern Office = engine_build + office_worker_build
                + python_worker_build when fallback is enabled
PDF/legacy    = engine_build + python_worker_build
```

The parse profile hash is SHA-256 over protocol version, the route-specific
stack fingerprint, and canonical JSON of
`{max_file_size_bytes,file_timeout_ms,file_timeout_by_extension_ms,parse}`.
The context profile hash is SHA-256 over protocol version, engine build, and
canonical normalized context JSON. File cache identity remains
`(file_identity, source_version, parse_profile_hash)`.

## Context envelope

`ContextEnvelope` always contains exactly:

```text
contract, protocol_version, request_id, engine_version, engine_build,
status, file_context, summary, scan_run_id, context_run_id, warnings, error
```

`ContextSummary` always contains all twelve counters/timings in its schema.
They are non-negative, and
`success_count + timeout_count + error_file_count <= source_file_count`.

| Status | Context | Run ids | Warnings | Error | Application action |
|---|---|---|---|---|---|
| `ok` | non-empty, including explicit no-files text | both positive | operational only | `null` | continue to LLM |
| `partial` | non-empty and trustworthy | both positive | at least one degradation/completeness warning | `null` | display warnings, continue to LLM |
| `error` | exactly empty | independently nullable | zero or more prior warnings | one diagnostic | stop before LLM |

Configured exclusions, compression, `metadata_only`, and budget-driven `omit`
do not by themselves make a run partial. Any unplanned read/parse error,
declared degradation, or actual fallback that affects completeness does.
Scan-backed daily/weekly/monthly commands stop on `error`; database-backed
weekly/monthly aggregation does not call the scanner.

## Diagnostics

Every error and warning has exactly six required keys:

```text
error_code, message, retryable, stage, file_path, backend
```

`stage` is one of `request`, `discovery`, `cache`, `parse`, `context`,
`process`, `doctor`, `inspect`, or `internal`. `file_path` is an absolute path
only for a file-scoped diagnostic and otherwise is null. `backend` is a
non-empty string only when known and otherwise is null. Empty sentinels and
placeholder paths/backends are invalid. Stable error-code values are frozen in
`diagnostic-v1.schema.json`; callers classify errors by code, never message
prefixes.

The only requestless error is `TransportErrorResponse`: contract
`ai_daily_transport`, status `error`, and an `INVALID_REQUEST` diagnostic at
stage `request` with null path/backend.

## Version and doctor

Scanner `version` reads no stdin and returns the exact command list
`version, doctor, build-context, inspect-run`, engine/build/target identity, and
both expected worker contract versions.

`doctor` accepts one request containing request id, absolute scan DB path, and
the same adapter object as a build request. It probes the DB parent and both
worker handshakes only. It does not parse a business file, mutate the scan DB,
or call an LLM. Its checks have exact `name/status/message` keys and status
`ok`, `warning`, or `error`. Doctor `ok` and `partial` require null `error`;
`partial` requires at least one warning; `error` requires a diagnostic.

## Worker contracts

Both workers expose requestless `version` output with exact worker identity,
build fingerprint, supported backend set, and supported extension set. Arrays
are canonical sorted unique values. Rust completes and validates all required
handshakes before discovery and cache lookup.

`WorkerParseRequest` has exactly:

```text
contract, protocol_version, request_id, file_path, file_type, backend,
remaining_timeout_ms, max_file_size_bytes, parser_limits,
expected_source_version
```

The tagged limits are:

- `OfficeLimits` for `rust_office_oxide_v1`, `rust_xlsx_bounded_v1`, and
  `python_office_v1`;
- `PdfLimits` for `pdf_text_v1`;
- `SharePointTextLimits` for `python_sharepoint_text_v1`.

The valid backend/type/lane combinations are strict:

| Backend | File types | Worker lane |
|---|---|---|
| `rust_xlsx_bounded_v1` | `.xlsx` | `rust_office_process` |
| `rust_office_oxide_v1` | `.docx`, `.pptx` | `rust_office_process` |
| `python_office_v1` | `.xls` and permitted modern Office fallback | `python_document_process` |
| `pdf_text_v1` | `.pdf` | `python_document_process` |
| `python_sharepoint_text_v1` | `.doc`, `.ppt` | `python_document_process` |

The permitted modern Python Office fallback types are `.xlsx`, `.docx`, and
`.pptx`. A worker never receives a pre-read text payload.

Worker success requires `error=null`, exact request id/path/type/backend,
preflight-matching worker build, matching source version, and a response before
the remaining deadline. `parser_backend` equals the requested backend.
`worker_lane` is `rust_office_process` for the Office binary and
`python_document_process` for the Python worker. `rust_core` is valid only in
scan audit rows and never in a worker response. Worker error requires empty
content and one diagnostic. A source version change is never cached.

## Inspect run

`InspectRunRequest` requires an absolute DB path, positive scan run id, and
boolean `include_content`. Production and real-directory benchmark callers
always send `false`. `true` is accepted only for a DB/run carrying the
sanitized-fixture marker created by tests.

An inspect response contains the persisted run state plus normalized audit
DTOs, never SQLite table details:

- Stage metrics: exact `stage,item_count,duration_ms`; stage is
  `discovery|cache|parse|context`.
- Extension metrics: exact `extension,file_count,parse_duration_ms,
  success_count,error_count,timeout_count`.
- File audit: exact `relative_path,file_identity,source_version,parse_status,
  parser_backend,worker_lane,cache_status,cache_miss_reason,truncated,
  content_sha256,parse_duration_ms,failure_class,fallback_backend,
  fallback_reason_code`.
- Context decision: exact `relative_path,action,reason,priority,input_chars,
  output_chars,truncated,error_code`; action is
  `keep|compress|metadata_only|omit|error`.

File `parser_backend` and `worker_lane` are independent evidence fields. With
`include_content=false`, file items expose hashes and metadata only. Successful
inspection has status `ok`, a persisted non-null run status, and null error.
Missing, corrupt, or inaccessible runs use status `error`; `run_status` is null
only when no trustworthy state can be read.

## Windows paths and data safety

- Accept Unicode, spaces, drive-rooted paths, UNC paths, and normalized long
  path prefixes.
- Preserve display casing in returned `file_path`.
- Use normalized case-folded paths only for `file_identity`.
- Return absolute paths where the DTO requires absolute paths; audit-relative
  paths must not be rooted or escape with `..`.
- Never place file content, API keys, credentials, prompts, or environment
  dumps in diagnostics.

The committed fixtures contain only synthetic paths and synthetic text. They
must never be replaced with production documents or copied business content.

## Semantic validation corpus

JSON Schema cannot express every v1 rule, including lexical rejection of
integral float tokens, date ordering, count sums, canonical sorting, exact
request/response echo checks, worker build identity, and source-version
equality. Every item in `invalid-cases.json` therefore declares
`validation_layer=schema|semantic`. Windows Task 2 tests prove schema cases are
rejected by Draft 2020-12 and semantic cases remain individually schema-valid.

Relational semantic cases also carry `related_payloads`: schema-valid request
and, when needed, worker-handshake DTOs against which response request id,
path, type, backend, source version, and build must be compared. Task 3 parsers
in both languages must accept every manifest/related payload and reject every
invalid case at its declared layer; later command tests add process exit and
side-effect proof.
