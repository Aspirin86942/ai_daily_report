# Office / PDF Parser Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement bounded `office_v1` / `pdf_text_v1` parser backends while preserving `light_text_v1`.

**Architecture:** Add a focused `src/services/document_parser.py` for Office/PDF extraction and keep `FileScanner` responsible for routing, size gate, timeout lane, cache, metrics, and aggregation. Extend parser profile and benchmark metadata so cache keys and reports distinguish parser backend from subprocess execution lane.

**Tech Stack:** Python 3.10+, pytest, pydantic, openpyxl, python-docx, python-pptx, pdfplumber, SQLite parse cache.

---

## Files

- Create: `src/services/document_parser.py`
  - Owns bounded `.docx/.xlsx/.pptx/.pdf` extraction and returns `FileContext`.
- Modify: `src/services/file_scanner.py`
  - Routes Office/PDF to document parser, enforces parent-process size gate, records `worker_lane`.
- Modify: `src/services/scan_planner.py`
  - Adds Office/PDF parser backend versions and bounded budgets to parser profile.
- Modify: `src/services/scan_metrics.py`
  - Adds optional `worker_lane` to `ReparseDetail`.
- Modify: `scripts/benchmark_scanner.py`
  - Counts parser backend separately from subprocess lane and displays new backends.
- Modify: `tests/test_document_parser.py`
  - New parser unit tests with generated fixtures.
- Modify: `tests/test_file_scanner.py`
  - Integration tests for routing, cache metadata, max-size gate, subprocess compatibility.
- Modify: `tests/test_scan_planner.py`
  - Parser profile budget tests.
- Modify: `tests/test_benchmark_scanner.py`
  - Backend/lane summary tests.

## Task 1: Document Parser Unit Tests And Backend

**Files:**
- Create: `tests/test_document_parser.py`
- Create: `src/services/document_parser.py`

- [ ] **Step 1: Write failing parser tests**

Add tests for:

```python
def test_parse_docx_extracts_paragraphs_and_tables(tmp_path: Path): ...
def test_parse_xlsx_limits_sheets_rows_columns_and_marks_truncated(tmp_path: Path): ...
def test_parse_pptx_extracts_slide_text_and_notes(tmp_path: Path): ...
def test_parse_pdf_extracts_text_layer(tmp_path: Path): ...
def test_parse_pdf_without_text_layer_returns_auditable_error(tmp_path: Path): ...
```

Expected initial failure: `ModuleNotFoundError` for `src.services.document_parser`.

- [ ] **Step 2: Verify red**

Run:

```powershell
conda run -n test python -m pytest tests/test_document_parser.py -v
```

Expected: fails because `document_parser` does not exist.

- [ ] **Step 3: Implement minimal document parser**

Create:

```python
OFFICE_PARSER_BACKEND = "office_v1"
PDF_TEXT_PARSER_BACKEND = "pdf_text_v1"
@dataclass(frozen=True, slots=True)
class DocumentParserOptions: ...
def parse_document_file(file_path: Path, file_type: str, limits: Mapping[str, Any], options: DocumentParserOptions | None = None) -> FileContext: ...
```

Implement bounded parsers for `.docx`, `.xlsx`, `.pptx`, `.pdf`; return stable error prefixes and `truncated=True` on budget truncation.

- [ ] **Step 4: Verify green**

Run:

```powershell
conda run -n test python -m pytest tests/test_document_parser.py -v
```

Expected: all new parser tests pass.

## Task 2: Parser Profile Budgets

**Files:**
- Modify: `src/services/scan_planner.py`
- Modify: `tests/test_scan_planner.py`

- [ ] **Step 1: Write failing planner tests**

Add tests that assert full and summary parser profiles include:

```python
"office_parser_backend": "office_v1"
"pdf_parser_backend": "pdf_text_v1"
"excel_max_sheets"
"excel_max_columns"
"docx_max_paragraphs"
"docx_max_tables"
"docx_table_max_rows"
"docx_table_max_cols"
"pptx_max_slides"
"pptx_include_notes"
"document_excerpt_max_chars"
```

Expected initial failure: missing keys.

- [ ] **Step 2: Verify red**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_planner.py -v
```

Expected: new tests fail on missing Office/PDF profile fields.

- [ ] **Step 3: Implement profile fields**

Update `ScanPlanner.build_parser_profile()` to import backend constants and normalize positive integer budgets with existing `normalize_positive_int`.

- [ ] **Step 4: Verify green**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_planner.py -v
```

Expected: planner tests pass.

## Task 3: Scanner Routing, Cache, And Lane Metadata

**Files:**
- Modify: `src/services/file_scanner.py`
- Modify: `src/services/scan_metrics.py`
- Modify: `tests/test_file_scanner.py`
- Modify: `tests/test_scan_metrics.py` if constructor assertions require update.

- [ ] **Step 1: Write failing scanner tests**

Add tests for:

```python
def test_scan_files_uses_document_backend_for_docx_in_direct_mode(...): ...
def test_document_backend_enforces_max_file_size_before_subprocess(...): ...
def test_scan_files_preserves_document_parser_metadata_from_cache(...): ...
def test_scan_files_keeps_legacy_subprocess_when_worker_lane_mode_subprocess(...): ...
def test_parser_profile_change_reparses_document_file(...): ...
```

Expected initial failures: Office/PDF still routes through old subprocess path or returns missing backend metadata.

- [ ] **Step 2: Verify red**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py -v
```

Expected: new tests fail for missing document backend routing / metadata.

- [ ] **Step 3: Implement routing and metadata**

Update `FileScanner` so:

- Parent-process `_extract_uncached_content()` applies `_build_file_too_large_context()` before any direct or subprocess route.
- `worker_lane_mode="subprocess"` keeps existing `_extract_content_with_timeout()` behavior.
- direct mode routes `.docx/.xlsx/.pptx/.pdf` through `_extract_document_content_with_timeout()`, which uses subprocess timeout supervision but returns backend `office_v1` or `pdf_text_v1`.
- `_extract_content()` also uses `parse_document_file()` so legacy subprocess worker returns the same backend metadata.
- `ReparseDetail` stores `worker_lane` without breaking old callers.

- [ ] **Step 4: Verify green**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py tests/test_scan_metrics.py -v
```

Expected: scanner and metrics tests pass.

## Task 4: Benchmark Backend Summary

**Files:**
- Modify: `scripts/benchmark_scanner.py`
- Modify: `tests/test_benchmark_scanner.py`

- [ ] **Step 1: Write failing benchmark tests**

Add assertions that:

- `office_v1` and `pdf_text_v1` appear under `by_extension`.
- `subprocess_count` is based on `worker_lane`, not backend string.
- `not_parsed_count` still counts `not_parsed`.
- Markdown report renders new backend rows.

- [ ] **Step 2: Verify red**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_scanner.py -v
```

Expected: fails until benchmark reads `worker_lane`.

- [ ] **Step 3: Implement summary update**

Update `build_parser_backend_summary()` to use `detail.worker_lane` when present and preserve compatibility for old details.

- [ ] **Step 4: Verify green**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_scanner.py -v
```

Expected: benchmark tests pass.

## Task 5: Final Verification

**Files:**
- All changed files.

- [ ] **Step 1: Run focused suite**

```powershell
conda run -n test python -m pytest tests/test_document_parser.py tests/test_file_scanner.py tests/test_scan_planner.py tests/test_benchmark_scanner.py tests/test_scan_metrics.py -v
```

Expected: all pass.

- [ ] **Step 2: Run full suite**

```powershell
conda run -n test python -m pytest tests -q
```

Expected: all pass.

- [ ] **Step 3: Run compile check**

```powershell
conda run -n test python -m compileall main.py src tests
```

Expected: compile succeeds.

- [ ] **Step 4: Inspect diff**

```powershell
git diff --stat
git status --short
```

Expected: only intended code, tests, spec, and plan files changed; no `.codegraph` marker change.

## Self-Review

- Spec coverage: each Office/PDF parser strategy, parser profile, cache metadata, benchmark summary, and no-OCR boundary maps to a task.
- Placeholder scan: no `TODO` / `TBD` placeholders.
- Type consistency: parser backend constants are defined once in `document_parser.py` and imported by planner/scanner/benchmark tests.
