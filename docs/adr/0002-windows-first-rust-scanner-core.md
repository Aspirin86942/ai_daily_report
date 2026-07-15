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
