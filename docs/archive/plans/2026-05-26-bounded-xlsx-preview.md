# Bounded XLSX Preview Implementation Plan

Status: IMPLEMENTED

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Rust bounded XLSX preview parser that reads only the configured sheet/row/column/character budget and avoids full workbook-to-Markdown conversion.

**Architecture:** `.xlsx` gets a dedicated `rust_xlsx_bounded_v1` fast path inside `rust/office_parser`; other Office formats keep `rust_office_oxide_v1`. Python accepts the new backend in payload validation and skips slow fallback for deterministic bad XLSX zip errors.

**Tech Stack:** Rust 2021, `zip`, `quick-xml`, Python pytest, existing scanner benchmark scripts.

---

## File Structure

- Modify: `rust/office_parser/Cargo.toml`
  - Add direct `quick-xml` and `zip` dependencies used by the bounded XLSX parser.
- Modify: `rust/office_parser/src/lib.rs`
  - Add `RUST_XLSX_BOUNDED_BACKEND`, bounded XLSX parser, Markdown renderer, and Rust unit tests.
- Modify: `src/services/office_parser.py`
  - Add backend constant, payload validation allowance, and deterministic bad-XLSX fallback skip.
- Modify: `tests/test_office_parser.py`
  - Add Python regression tests for new backend validation and fallback skip.
- Create: `docs/superpowers/specs/2026-05-26-bounded-xlsx-preview-design.md`
  - Approved design.
- Create: `docs/superpowers/plans/2026-05-26-bounded-xlsx-preview.md`
  - This implementation plan.

## Task 1: Rust Bounded XLSX Tests

**Files:**
- Modify: `rust/office_parser/src/lib.rs`

- [x] **Step 1: Write failing Rust tests**

Add tests that build a minimal XLSX zip in memory, write it to a temp file, call `parse_office_file()`, and assert:

```rust
assert_eq!(context.parser_backend, RUST_XLSX_BOUNDED_BACKEND);
assert!(context.content.contains("## Sheet: Data"));
assert!(context.content.contains("| Name | Amount |"));
assert!(!context.content.contains("hidden-over-budget"));
assert!(context.truncated);
```

- [x] **Step 2: Run Rust tests to verify RED**

Run:

```powershell
cd rust/office_parser
cargo test
```

Expected: compile or test failure because `RUST_XLSX_BOUNDED_BACKEND` and parser functions do not exist yet.

## Task 2: Rust Parser Implementation

**Files:**
- Modify: `rust/office_parser/Cargo.toml`
- Modify: `rust/office_parser/src/lib.rs`

- [x] **Step 1: Add dependencies**

Add:

```toml
quick-xml = "0.40"
zip = { version = "8.1", features = ["deflate"], default-features = false }
```

- [x] **Step 2: Implement `.xlsx` dispatch**

Change `parse_office_file()` so `.xlsx` calls:

```rust
parse_bounded_xlsx(request, &file_type)
```

Other extensions continue through `office_oxide::Document::open()`.

- [x] **Step 3: Implement bounded workbook reading**

Implement helpers to parse workbook sheets, relationships, selected worksheet rows/cells, needed shared strings, and Markdown rendering with char budget.

- [x] **Step 4: Run Rust tests to verify GREEN**

Run:

```powershell
cd rust/office_parser
cargo test
```

Expected: all Rust tests pass.

## Task 3: Python Validation And Fallback Tests

**Files:**
- Modify: `tests/test_office_parser.py`
- Modify: `src/services/office_parser.py`

- [x] **Step 1: Write failing Python tests**

Add one test proving `RustOfficeParserRunner` accepts a `.xlsx` payload whose `parser_backend` is `rust_xlsx_bounded_v1`.

Add one test proving `parse_office_with_fallback()` does not call Python fallback when Rust returns:

```text
RUST_XLSX_BOUNDED_PARSE_FAILED: ZIP error: invalid Zip archive: Could not find EOCD
```

- [x] **Step 2: Run Python tests to verify RED**

Run:

```powershell
conda run -n test python -m pytest tests/test_office_parser.py -v
```

Expected: validation/fallback tests fail before production code changes.

- [x] **Step 3: Implement Python support**

Add:

```python
RUST_XLSX_BOUNDED_BACKEND = "rust_xlsx_bounded_v1"
```

Allow this backend only for `.xlsx` payloads and skip fallback for deterministic bounded XLSX zip errors.

- [x] **Step 4: Run Python tests to verify GREEN**

Run:

```powershell
conda run -n test python -m pytest tests/test_office_parser.py -v
```

Expected: all office parser Python tests pass.

## Task 4: Integration Verification

**Files:**
- No additional source files unless tests reveal an integration gap.

- [x] **Step 1: Run targeted scanner tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py tests/test_office_parser.py -v
```

Expected: all targeted tests pass.

- [x] **Step 2: Build Rust parser**

Run:

```powershell
cd rust/office_parser
cargo build --release
```

Expected: release binary builds successfully.

- [x] **Step 3: Run benchmark evidence**

Run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run_scanner_benchmark_ab.ps1 -SkipBuild
```

Expected: benchmark JSON/Markdown files are written and `.xlsx` backend summary shows `rust_xlsx_bounded_v1`.

## Self-Review

- Spec coverage: tasks cover Rust bounded parser, Python validation, fallback behavior, tests, and benchmark verification.
- Placeholder scan: no `TBD`, `TODO`, or undefined later step.
- Type consistency: backend name is consistently `rust_xlsx_bounded_v1`; Rust constant name is `RUST_XLSX_BOUNDED_BACKEND`; Python constant name matches.
