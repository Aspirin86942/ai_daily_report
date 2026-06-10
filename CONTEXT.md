# AI Daily Report

AI-assisted audit report generator context. This glossary keeps project-specific runtime and scanner terms precise so design documents, benchmarks, and implementation plans use the same language.

## Language

**Cold scanner run**:
A scanner run where parse cache is intentionally unavailable for the target date range, so discovered files that need content must be parsed again. It does not require clearing the operating system file cache, and it excludes LLM calls and report rendering.
_Avoid_: Absolute cold start, OS-cache cold run, uncached app startup

**Hybrid Office fallback policy**:
A rule set that decides whether a Rust Office parser failure should fall back to Python by error category first, with file extension used only as a secondary condition.
_Avoid_: Extension-only fallback, always fallback, Rust-only failure mode

**Deterministic Office parse failure**:
An Office parse failure class that should be treated as repeatable for the current scanner run, so the scanner records an auditable failure instead of spending cold-run budget on fallback parsing. Timeout belongs to this class by default because scanner performance is the primary requirement for cold Office parsing.
_Avoid_: Retryable parse failure, fallback candidate

**Environment-unavailable Office parse failure**:
A Rust Office parser failure caused by the parser binary or runtime environment being unavailable, not by the Office file content. It may fall back to Python for report usefulness, but it must not be interpreted as Rust parser performance evidence.
_Avoid_: Rust parser slow case, file parse failure

**Office parser contract failure**:
A Rust Office parser failure where the CLI completed but its stdout or `FileContext` payload does not satisfy the Python scanner contract. It may fall back to Python for report usefulness, but it should be treated as a Rust-Python boundary defect rather than an Office file content problem.
_Avoid_: File content failure, environment-unavailable failure

**Rust CLI JSON contract**:
The stdin/stdout JSON contract shared by Rust helper CLIs and Python adapters, including binary path resolution, request payload shape, timeout handling, stdout JSON decoding, stderr/error mapping, and response payload validation. It is the seam where Rust helper execution becomes trusted Python scanner data.
_Avoid_: Raw subprocess call, Rust helper wrapper, CLI glue
