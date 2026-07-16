# Adopt a Windows-first Rust scanner/context core

## Status

Accepted on 2026-07-15.

## Context

The application currently uses Python to orchestrate discovery, parser routing,
timeouts, cache access, aggregation, compression, and audit while delegating
selected discovery and Office parsing work to Rust helpers. That split leaves
the most important scanner semantics distributed across process boundaries and
makes cache identity, timeout ownership, and fallback evidence harder to audit.

The deployed environment is Windows x64. Its path rules, process-tree timeout
behavior, Task Scheduler launch context, and native release artifacts are
therefore production requirements rather than incidental portability details.

## Decision

- Windows x64 is the production and release platform. Linux remains a
  compatibility-only target and does not define production behavior.
- Python remains the application shell. It owns the CLI, local configuration
  loading, secrets, LLM integration, report models, report history, Jinja2, and
  Markdown rendering.
- Rust becomes the deterministic scanner/context core. It owns scanner-profile
  defaults and validation, discovery, classification, parser routing, worker
  deadlines, inventory, parse cache, planning, aggregation, deterministic
  compression, scan/context audit, and scanner metrics.
- One report scan crosses the production process seam exactly once: Python
  sends one strict `BuildContextRequest` to `build-context`, and Rust returns
  one strict `ContextEnvelope`.
- Python transports only explicitly configured scanner-profile leaves. It does
  not apply Rust-owned defaults, unit conversion, classification, or fallback
  selection.
- The production scanner cache and audit database has one writer: Rust. Python
  reads diagnostic data only through versioned CLI DTOs such as `inspect-run`.
- After cutover there is no silent top-level fallback from the Rust core to the
  retired Python scanner chain. Invalid output, crash, timeout, or contract
  mismatch is an explicit scanner error and stops scan-backed report commands
  before any LLM call.
- The old parse cache is not migrated. The ownership and profile semantics have
  changed, so v2 starts cold while the legacy database remains untouched until
  the legacy implementation is deleted.
- `.xlsx` uses the bounded Office-worker backend
  `rust_xlsx_bounded_v1`; `.docx` and `.pptx` use
  `rust_office_oxide_v1`. Both use the strict `OfficeLimits` request shape and
  the `rust_office_process` lane. A successful worker response reports the
  exact requested backend, keeping `parser_backend` separate from
  `worker_lane`.
- `cache_status` remains mandatory audit evidence in `inspect-run`, but it is
  excluded from `file_context`. Cache state is operational metadata, so an
  unchanged cold and warm run must produce identical context bytes.
- `parser_profile_version` is part of every route's parse-cache fingerprint.
  Changing that semantic version invalidates otherwise unchanged entries.
- Every new or resumed run performs both live worker version handshakes before
  discovery or parse-cache lookup. Persisted fingerprints are audit evidence
  only and are never substituted for a current handshake.

## Accepted parity difference

Task 10 freezes the sanitized corpus in `scripts/scanner_cutover_gate.py` and
its expected root-normalized complete-context hashes in
`tests/fixtures/scanner_cutover/task10_expected_context_hashes.json`. On that
corpus, both engines must discover the same inventory, produce identical
normalized hashes for text/PDF and any same-backend parse, make the same file
decisions, stay within budget, and be independently byte-deterministic. The
two frozen final context hashes intentionally differ: Rust replaces the legacy
`ScanAggregator` plus `ContextCompressor` double-budget path with the single
budgeting/rendering pipeline approved by this ADR. Any renderer drift changes
the golden and requires an explicit ADR review; no parser-content, decision,
fallback, or nondeterminism difference is accepted under that exception.

## Task 10 cutover evidence (2026-07-16)

Warm-start work retained every frozen safety boundary: each new run still
performs both live worker handshakes before discovery/cache lookup, error rows
are retried, and Windows workers still use suspended creation followed by Job
assignment and resume. The measured implementation overlaps the independent
handshakes, uses a stdlib-only `-S` Python version path, avoids a Rayon pool for
zero/one parse candidates, creates the heartbeat connection only after the
first interval, and uses `synchronous=NORMAL` for staging transactions before
restoring `FULL` for the atomic terminal transaction. On Windows venvs it may
create a hash-verified, content-addressed copy of the base CPython image beside
the existing launcher; it never replaces the launcher or `pyvenv.cfg`, and an
unverifiable existing target falls back to the configured executable. The
Windows one-request Python worker explicitly flushes its contract response
before using the native process exit path, avoiding interpreter-finalizer work
without reusing a process or skipping a live handshake.

The final full cutover run used 21 alternating cold/warm pairs per engine and
remained red:

| boundary | Python legacy | Rust v2 | criterion |
|---|---:|---:|---|
| cold median | 2055.858 ms | 1360.819 ms | pass (`<= +10%`) |
| cold p95 | 2256.308 ms | 1437.662 ms | pass (`<= +20%`) |
| warm median | 59.271 ms | 60.931 ms | fail (`+1.660 ms`, `+2.80%`) |

Parity, cache semantics, the frozen fault matrix, real-directory comparison,
and process cleanup all passed in that same run. The real-directory evidence
contained one eligible `.xlsx`, reported `rust_xlsx_bounded_v1` on
`rust_office_process`, and retained only aggregate metadata. The performance
run completed without a freeze in 76.909 seconds, contained parser-backend and
worker-lane evidence for both engines, and left no new scanner/worker/orphan
process. Earlier scheduling-favorable samples are not accepted in place of
this final-source full run. Therefore the Task 10 cutover gate is not satisfied,
`python_legacy` remains the default, and Task 11 must not start from this
evidence.

## Contract authority

The exact v1 wire shapes are frozen by the JSON Schemas under
`docs/contracts/`, the semantic rules in `scanner-context-v1.md`, and the
golden corpus under `tests/fixtures/scanner_contract/v1/`. Rust and Python must
consume the same fixtures. Unknown fields are rejected, and required nullable
fields must be present with either a value or JSON `null`.

## Relationship to ADR 0001

ADR 0001 remains the historical decision for the helper phase. This ADR
supersedes its ownership boundary after cutover: Rust, not Python, chooses and
audits parser routes and fallback. It does not supersede ADR 0001's
performance-first timeout principle. One file still has one total deadline,
fallback may consume only remaining time, and timeout fallback stays disabled
unless explicitly configured.

## Consequences

- The Windows release builds one workspace and ships the scanner plus Office
  worker artifacts together.
- Scanner behavior becomes testable through one typed boundary and one
  canonical fixture corpus.
- Cache keys include normalized parse semantics and route-specific build
  fingerprints known before cache lookup.
- Linux failures may block compatibility claims but do not redefine the Windows
  release contract.
- Rollback after Python legacy deletion is a Git revert and rebuild, not a
  runtime compatibility shim.

## Security constraints

The Rust request never receives API keys, LLM endpoints, prompts, free-form
user report input, report output, or environment dumps. Contract fixtures use
only synthetic paths and content. `doctor` performs capability checks without
opening a business document, mutating the scanner database, or calling an LLM.
