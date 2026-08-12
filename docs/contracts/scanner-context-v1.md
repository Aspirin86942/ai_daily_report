# Native scanner domain contract

## Authority and transport

The JSON Schemas in this directory and the matching Rust/Pydantic types define
the scanner domain shapes. Production does not exchange JSON text: PyO3
converts Rust structs directly to Python dictionaries, lists, and scalars.
Schema filenames retain their stable domain version so fixtures can validate
both languages against the same shapes.

All contract models deny unknown fields. Required nullable fields must be
present with either a value or null. Diagnostics never contain file content,
credentials, prompts, environment dumps, or raw worker output.

## Python interface

```python
scanner = NativeScanner(config)
scanner.build_context(ScanRequest) -> ScanResult
scanner.doctor() -> DoctorResponse
```

`ScanRequest` contains exactly:

- `report_mode`: `daily|weekly|monthly`;
- `start_date` and `end_date`, inclusive ISO dates with start not after end;
- optional `compression_profile`.

Report source selection and free-form user input are application concerns and
must not enter the scanner interface.

`ScanResult` contains a `ContextEnvelope` and optional `ScannerEvidence`.
Evidence is required whenever the envelope has a scan run id, and its scan/run
identity and summary must match the envelope.

## Rust interface

```rust
Scanner::open(ScannerConfig) -> Result<Scanner, ScannerError>
Scanner::build_context(&ScanRequest) -> Result<ScannerOperation<ContextResult>, ScannerError>
Scanner::doctor() -> Result<ScannerOperation<DoctorResponse>, ScannerError>
```

A `Scanner` instance owns its v3 store and lazy worker pools. Only one
top-level context build may run at a time; a concurrent attempt returns a
retryable busy error. The PyO3 adapter releases the GIL while Rust performs the
operation and catches panics before they cross FFI.

Expected file, budget, deadline, and worker failures are represented in the
envelope status, diagnostics, and warnings. Invalid configuration or request
values become `ValueError`. Initialization, SQLite invariants, busy state, and
panic failures become structured `NativeScannerError(error_code, message,
retryable)`.

## Context envelope

The envelope exposes report-facing context without storage details:

- `status`: `ok|partial|error`;
- `file_context`;
- summary counts and stage durations;
- warnings and optional error;
- scan/context run ids when a run was created.

Status invariants:

- `ok`: non-null context, no error, and no failed/timed-out files;
- `partial`: usable context, no outer error, and one or more bounded file
  failures/warnings;
- `error`: non-null outer error and no usable report context.

Context rendering is deterministic. Operational cache state does not alter
context bytes for identical accepted content.

## Complete scanner evidence

Evidence comes from the same in-memory domain result used by the terminal
SQLite transaction. It includes:

- stage and extension metrics;
- per-file audit and context decisions;
- parser backend and worker lane as separate dimensions;
- artifact id, reuse kind, and reused context run id;
- session/attempt/transport/cache details;
- peak worker RSS and execution counters;
- warnings and the same summary/run identities as the envelope.

Consumers must use this result directly. They do not reopen the scanner store
to reconstruct evidence.

## Scanner settings

Python passes only explicitly configured mutable leaves. Rust owns defaults,
validation, unit conversions, routing policy, fallback order, and normalized
identity. The only path leaves are `index_db_path` and `office_worker_path`;
Python executable and module root derive from the exact running environment.

Unknown or removed keys fail closed. Cache identity includes the native build,
normalized settings, backend/lane, worker contract/build, budgets, timeouts,
and source fingerprint.

## Worker v2 envelope

Both isolated workers use `contract=ai_daily_worker`, `protocol_version=2`,
and `worker_contract_version=ai_daily_worker_v2`.

The first frame is `hello` with worker kind, worker version/build, and unique
supported operations. Subsequent NDJSON frames are strict request/response
envelopes with matching request id and operation. A response is exactly one of:

- `status=ok`, non-null result, null error;
- `status=error`, null result, non-null `WorkerDiagnostic`.

Supported operations are:

- Office worker: `office_parse`;
- Python document worker: `pdf_classify`, `pdf_parse`,
  `python_office_parse`, `python_sharepoint_parse`.

The worker pool recycles a process after its request limit, idle TTL, RSS
limit, crash, timeout, dirty protocol, or capability mismatch. A source change
returns outer retryable `SOURCE_VERSION_CHANGED`; it is never cached or
silently replayed.

## Scanner database

The scanner store accepts only `PRAGMA user_version=3`. A missing file is
created at v3. Any existing other version returns
`SCANNER_DB_SCHEMA_MISMATCH` without modifying the file. Scanner run history is
not migrated across this reset.

## Windows paths and fixture safety

- Accept drive-rooted, UNC, Unicode, space-containing, and normalized long
  Windows paths.
- Returned diagnostic paths preserve display casing; cache identity uses the
  normalized source identity.
- Audit-relative paths must not be rooted or escape with `..`.
- Fixtures contain only synthetic paths and text and must never be replaced
  with production content.

JSON Schema cannot express every semantic rule, including date ordering,
count sums, canonical sorting, result/envelope identity equality, worker build
matching, route/backend/lane consistency, and source-version equality.
`invalid-cases.json` marks whether each case is rejected by schema or semantic
validation; both Rust and Python must accept every valid fixture and reject
every invalid case at its declared layer.
