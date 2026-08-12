# Adopt an in-process Windows native scanner

## Status

Accepted on 2026-08-12. This revision supersedes the earlier process-based
scanner decision while retaining its Windows-first and crash-isolation goals.

## Context

The previous report path split scanner orchestration across multiple Python
layers and a standalone Rust process. Validation, parameter forwarding, DTO
shrinking, JSON transport, and later evidence reads made the interface nearly
as complex as the implementation. Office/PDF parsers still need crash
isolation, but deterministic scanner orchestration does not.

The deployment target is Windows x64 with exact CPython 3.13.13. Cache identity,
worker process lifetime, rollback, and native packaging are production
contracts rather than incidental portability details.

## Decision

The production chain is:

```text
CLI → ReportRunner → NativeScanner → PyO3 Scanner
→ scanner_core → worker v2 pools
```

- Python remains the application shell and owns CLI, local configuration,
  secrets, LLM integration, report models, report SQLite, templates, and
  Markdown publication.
- `ReportRunner.run(ReportRunRequest) -> ReportRunOutcome` remains the single
  report interface and preserves daily/weekly/monthly behavior.
- `NativeScanner` is the only Python adapter at the scanner seam. It lazily
  imports `ai_daily_scanner_native`, maps the small request, validates one
  result, and maps stable errors.
- Rust `Scanner` is the scanner deep module. It hides normalized settings,
  store, scheduler/planner/parser assembly, caches, evidence, and two lazy
  worker pools behind `open`, `build_context`, and `doctor`.
- One report scan performs one PyO3 call. Rust releases the GIL during work.
  Top-level context builds are serialized while work inside one build remains
  concurrent.
- Context and complete evidence return together from the current run and are
  persisted from the same domain data.
- Office/PDF remain crash-isolated under one strict worker-v2 envelope. Pools
  enforce hello capabilities, request limits, idle TTL, RSS limits, timeout,
  dirty-protocol recovery, and controlled restart.
- Routing and ordinary fallback are compile-time Rust policy. Timeout fallback
  is the sole runtime policy switch and defaults off.
- The scanner database is fresh-only `user_version=3`; other versions are
  rejected without migration or modification.

## Error contract

Expected file, worker, timeout, and budget failures remain in the typed scan
status and diagnostics. Invalid request/configuration values become
`ValueError`. Native initialization, SQLite invariants, busy state, and caught
panic failures become `NativeScannerError` with `error_code`, `message`, and
`retryable`.

Release builds use unwind panic strategy and catch panics before FFI. Error
messages must not disclose file contents, credentials, prompts, or environment
dumps.

## Database and cache identity

The v3 store is owned for the life of the Rust `Scanner`. Cache identity
includes native build identity, normalized mutable settings, backend/lane,
worker contract/build, budgets, timeouts, and source fingerprint.

Cold and warm runs over unchanged inputs must produce identical context bytes.
Cache state is operational evidence returned in `ScannerEvidence`, not report
prompt content.

## Windows release and rollback

A release contains an exact `cp313-win_amd64` wheel, the Office worker
executable, Python application files, and a hash/build-identity manifest.
Packaging validates wheel installation/import in a disposable CPython 3.13.13
venv and rejects the wrong Python version.

Deployment is side by side. Before an authorized cutover, operators stop report
processes, use the SQLite backup API to archive the old scanner database with
integrity/hash evidence, and point the new release at a new v3 database. The
old scanner database and report SQLite are retained unchanged.

Rollback stops the new process, restores the previous release and its original
database pointer, and leaves the new v3 database for diagnosis. No backward
database conversion is provided.

## Consequences

- Callers learn two high-leverage interfaces instead of the scanner's internal
  assembly and transport details.
- Scanner process startup and scanner JSON serialization are eliminated.
- Only document workers can remain as child processes.
- The native wheel is tied to Windows x64 and exact CPython 3.13.13; no abi3,
  alternate Python, or Linux compatibility layer is promised.
- Old scanner history is not readable through runtime compatibility surfaces;
  Git history and read-only database archives provide audit recovery.
- Actual install, process control, configuration pointer changes, database
  archival, push, and release remain separately authorized actions.

## Verification

Acceptance requires Python and Rust suites, format/clippy/release build, exact
wheel install/import, worker-v2 failure/recycle tests, v3 schema gates, fixed
corpus cold/warm evidence, package manifest verification, strict doctor, CLI
help, compileall, dependency audits, and a clean diff check.
