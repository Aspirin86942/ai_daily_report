# Rust Office Cold-Scan Hybrid Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the phase 1 Rust Office cold-scan hybrid fallback policy with explicit `failure_class`, cache-profile isolation, benchmark evidence, and focused tests.

**Architecture:** Keep Python as scanner orchestration and Rust as the Office parser primary path. Add an explicit Python-side failure classifier inside `src/services/office_parser.py`, carry the resulting `failure_class` through `OfficeParseAudit`, `ReparseDetail`, benchmark JSON, and benchmark Markdown, and add `office_fallback_policy_version=hybrid_v1` to parser profiles so parse cache cannot hide policy changes.

**Tech Stack:** Python 3.10+, dataclasses, Pydantic `FileContext`, pytest, current Rust CLI parser contract, SQLite-backed scanner metrics, PowerShell + `conda run -n test`.

---

## Scope Check

This plan implements only the stage 1 spec in `docs/superpowers/specs/2026-06-10-rust-office-cold-scan-hybrid-fallback-design.md`.

In scope:

- Rust Office parser failure classification.
- `failure_class` in audit, scanner reparse detail, benchmark JSON, and benchmark Markdown.
- Parser profile isolation through `office_fallback_policy_version`.
- Focused Python tests and existing Rust parser verification.

Out of scope:

- Batch Office parser.
- Long-running Rust worker.
- Rust scanner core.
- Full Rust rewrite.
- Python wheel / bundled Rust binary release path.
- LLM, template, daily, weekly, or monthly report logic.

## File Structure

- `src/services/office_parser.py`
  - Owns Rust Office parser subprocess contract, Python fallback orchestration, and the new failure classifier.
- `src/services/scan_metrics.py`
  - Owns stable scanner benchmark data shapes; add `failure_class` to `ReparseDetail`.
- `src/services/file_scanner.py`
  - Carries `OfficeParseAudit.failure_class` from Office parsing into `ReparseDetail`.
- `src/services/scan_planner.py`
  - Owns parser profile fields; add `office_fallback_policy_version`.
- `src/core/config.py`
  - Exposes optional `scanner.office_fallback_policy_version` with default `hybrid_v1`.
- `scripts/benchmark_scanner.py`
  - Renders `failure_class` in JSON-derived Markdown and adds a short explanatory note for `environment_unavailable`.
- `config/settings.example.yaml`
  - Documents `office_fallback_policy_version: "hybrid_v1"`.
- `tests/test_office_parser.py`
  - Unit tests for failure classification and fallback decisions.
- `tests/test_file_scanner.py`
  - Integration-style scanner tests proving `failure_class` reaches reparse detail.
- `tests/test_scan_planner.py`
  - Parser profile tests proving cache key isolation includes the policy version.
- `tests/test_config.py`
  - Config tests proving `scanner_config` exposes the policy version.
- `tests/test_benchmark_scanner.py`
  - Benchmark JSON and Markdown tests for `failure_class`.

---

### Task 1: Add Office Failure Classification

**Files:**
- Modify: `src/services/office_parser.py`
- Modify: `tests/test_office_parser.py`

- [ ] **Step 1: Write failing tests for explicit failure classes**

Append these tests to `tests/test_office_parser.py` after `test_parse_office_with_fallback_skips_python_for_deterministic_bad_xlsx_zip`:

```python
def test_parse_office_with_fallback_allows_timeout_fallback_when_enabled(tmp_path):
    sample = tmp_path / "slow.xlsx"
    sample.write_bytes(b"fake")
    rust_context = FileContext(
        file_path=str(sample),
        file_type=".xlsx",
        content="",
        error="RUST_OFFICE_TIMEOUT: file parse exceeded 3s",
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )

    class FakeRunner:
        def parse(self, file_path, file_type, limits, timeout_seconds):
            return rust_context, 3

    def fake_python_fallback(file_path, file_type, limits):
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="timeout fallback content",
            error=None,
            parser_backend=PYTHON_OFFICE_BACKEND,
            truncated=False,
        )

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".xlsx",
        limits={},
        scanner_cfg={
            "office_parser_backend": RUST_OFFICE_BACKEND,
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": [PYTHON_OFFICE_BACKEND],
            "office_fallback_after_timeout": True,
        },
        timeout_seconds=3,
        rust_runner=FakeRunner(),
        python_fallback=fake_python_fallback,
    )

    assert outcome.context.content == "timeout fallback content"
    assert outcome.audit.failure_class == "deterministic"
    assert outcome.audit.fallback_reason == "RUST_OFFICE_TIMEOUT: file parse exceeded 3s"


def test_parse_office_with_fallback_marks_start_failure_as_environment_unavailable(
    tmp_path,
):
    sample = tmp_path / "report.docx"
    sample.write_bytes(b"fake")
    rust_context = FileContext(
        file_path=str(sample),
        file_type=".docx",
        content="",
        error="RUST_OFFICE_START_FAILED: no such file or directory",
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )

    class FakeRunner:
        def parse(self, file_path, file_type, limits, timeout_seconds):
            return rust_context, 4

    def fake_python_fallback(file_path, file_type, limits):
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="python fallback",
            error=None,
            parser_backend=PYTHON_OFFICE_BACKEND,
            truncated=False,
        )

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".docx",
        limits={},
        scanner_cfg={
            "office_parser_backend": RUST_OFFICE_BACKEND,
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": [PYTHON_OFFICE_BACKEND],
            "office_fallback_after_timeout": False,
        },
        timeout_seconds=12,
        rust_runner=FakeRunner(),
        python_fallback=fake_python_fallback,
    )

    assert outcome.context.content == "python fallback"
    assert outcome.audit.failure_class == "environment_unavailable"
    assert outcome.audit.fallback_backend == PYTHON_OFFICE_BACKEND


def test_parse_office_with_fallback_marks_invalid_payload_as_contract_failure(
    tmp_path,
):
    sample = tmp_path / "report.pptx"
    sample.write_bytes(b"fake")
    rust_context = FileContext(
        file_path=str(sample),
        file_type=".pptx",
        content="",
        error="RUST_OFFICE_INVALID_PAYLOAD: parser_backend mismatch",
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )

    class FakeRunner:
        def parse(self, file_path, file_type, limits, timeout_seconds):
            return rust_context, 8

    def fake_python_fallback(file_path, file_type, limits):
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="fallback content",
            error=None,
            parser_backend=PYTHON_OFFICE_BACKEND,
            truncated=False,
        )

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".pptx",
        limits={},
        scanner_cfg={
            "office_parser_backend": RUST_OFFICE_BACKEND,
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": [PYTHON_OFFICE_BACKEND],
            "office_fallback_after_timeout": False,
        },
        timeout_seconds=12,
        rust_runner=FakeRunner(),
        python_fallback=fake_python_fallback,
    )

    assert outcome.context.content == "fallback content"
    assert outcome.audit.failure_class == "contract_failure"


def test_parse_office_with_fallback_marks_recoverable_parser_failure(tmp_path):
    sample = tmp_path / "report.docx"
    sample.write_bytes(b"fake")
    rust_context = FileContext(
        file_path=str(sample),
        file_type=".docx",
        content="",
        error="RUST_OFFICE_PARSE_FAILED: unexpected parser error",
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )

    class FakeRunner:
        def parse(self, file_path, file_type, limits, timeout_seconds):
            return rust_context, 5

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".docx",
        limits={},
        scanner_cfg={
            "office_parser_backend": RUST_OFFICE_BACKEND,
            "office_parser_fallback_enabled": False,
            "office_parser_fallback_order": [PYTHON_OFFICE_BACKEND],
            "office_fallback_after_timeout": False,
        },
        timeout_seconds=12,
        rust_runner=FakeRunner(),
        python_fallback=lambda file_path, file_type, limits: pytest.fail(
            "fallback is disabled"
        ),
    )

    assert outcome.context is rust_context
    assert outcome.audit.failure_class == "recoverable_parser_failure"
    assert outcome.audit.fallback_backend == ""
```

Update existing tests in `tests/test_office_parser.py` to assert the new field:

```python
assert outcome.audit.failure_class == "recoverable_parser_failure"
```

for `test_parse_office_with_fallback_uses_python_when_rust_fails`, and:

```python
assert outcome.audit.failure_class == "deterministic"
```

for both timeout default no-fallback and deterministic bad `.xlsx` ZIP tests.

- [ ] **Step 2: Run office parser tests to verify RED**

Run:

```powershell
conda run -n test python -m pytest tests/test_office_parser.py -q
```

Expected: FAIL because `OfficeParseAudit` does not expose `failure_class` and `parse_office_with_fallback()` does not classify failures yet.

- [ ] **Step 3: Implement failure constants, audit field, and classifier**

In `src/services/office_parser.py`, add these constants near the backend constants:

```python
OFFICE_FAILURE_DETERMINISTIC = "deterministic"
OFFICE_FAILURE_ENVIRONMENT_UNAVAILABLE = "environment_unavailable"
OFFICE_FAILURE_CONTRACT = "contract_failure"
OFFICE_FAILURE_RECOVERABLE = "recoverable_parser_failure"
OFFICE_FALLBACK_POLICY_VERSION = "hybrid_v1"
```

Update `OfficeParseAudit`:

```python
@dataclass(frozen=True, slots=True)
class OfficeParseAudit:
    attempted_backend: str = ""
    fallback_backend: str = ""
    fallback_reason: str = ""
    rust_duration_ms: int = 0
    fallback_duration_ms: int = 0
    failure_class: str = ""
```

Add this dataclass and classifier below `PythonFallback`:

```python
@dataclass(frozen=True, slots=True)
class OfficeFallbackDecision:
    failure_class: str
    allow_fallback: bool
    reason: str


def classify_office_failure(
    *,
    file_type: str,
    rust_backend: str,
    rust_error: str,
    scanner_cfg: Mapping[str, Any],
) -> OfficeFallbackDecision:
    fallback_enabled = bool(scanner_cfg.get("office_parser_fallback_enabled", True))
    fallback_after_timeout = bool(scanner_cfg.get("office_fallback_after_timeout", False))
    normalized_type = file_type.lower()

    if rust_error.startswith("RUST_OFFICE_TIMEOUT:"):
        return OfficeFallbackDecision(
            failure_class=OFFICE_FAILURE_DETERMINISTIC,
            allow_fallback=fallback_enabled and fallback_after_timeout,
            reason="timeout",
        )

    if (
        normalized_type == ".xlsx"
        and rust_backend == RUST_XLSX_BOUNDED_BACKEND
        and rust_error.startswith("RUST_XLSX_BOUNDED_PARSE_FAILED: ZIP error:")
    ):
        return OfficeFallbackDecision(
            failure_class=OFFICE_FAILURE_DETERMINISTIC,
            allow_fallback=False,
            reason="deterministic_xlsx_zip_error",
        )

    if rust_error.startswith("RUST_OFFICE_START_FAILED:"):
        return OfficeFallbackDecision(
            failure_class=OFFICE_FAILURE_ENVIRONMENT_UNAVAILABLE,
            allow_fallback=fallback_enabled,
            reason="rust_binary_unavailable",
        )

    if rust_error.startswith("RUST_OFFICE_INVALID_JSON:") or rust_error.startswith(
        "RUST_OFFICE_INVALID_PAYLOAD:"
    ):
        return OfficeFallbackDecision(
            failure_class=OFFICE_FAILURE_CONTRACT,
            allow_fallback=fallback_enabled,
            reason="rust_python_contract_failed",
        )

    return OfficeFallbackDecision(
        failure_class=OFFICE_FAILURE_RECOVERABLE,
        allow_fallback=fallback_enabled,
        reason="rust_parse_failed",
    )
```

- [ ] **Step 4: Use classifier inside fallback orchestration**

In `parse_office_with_fallback()`, replace the current `fallback_enabled`, `fallback_after_timeout`, `is_timeout`, and `_should_skip_python_fallback()` decision block with:

```python
    fallback_reason = rust_context.error or ""
    attempted_backend = rust_context.parser_backend or RUST_OFFICE_BACKEND
    decision = classify_office_failure(
        file_type=normalized_type,
        rust_backend=attempted_backend,
        rust_error=fallback_reason,
        scanner_cfg=scanner_cfg,
    )

    if not decision.allow_fallback:
        return OfficeParseOutcome(
            context=rust_context,
            audit=OfficeParseAudit(
                attempted_backend=attempted_backend,
                fallback_reason=fallback_reason,
                rust_duration_ms=rust_duration_ms,
                failure_class=decision.failure_class,
            ),
        )
```

Update both fallback return branches in the same function so each `OfficeParseAudit` constructor includes:

```python
                failure_class=decision.failure_class,
```

Keep `_should_skip_python_fallback()` for one transition commit only if tests still reference it. If no references remain after the classifier change, delete `_should_skip_python_fallback()` in this task.

- [ ] **Step 5: Export constants in tests**

Update the import list in `tests/test_office_parser.py` to include:

```python
    OFFICE_FAILURE_CONTRACT,
    OFFICE_FAILURE_DETERMINISTIC,
    OFFICE_FAILURE_ENVIRONMENT_UNAVAILABLE,
    OFFICE_FAILURE_RECOVERABLE,
```

Replace string assertions with the imported constants:

```python
assert outcome.audit.failure_class == OFFICE_FAILURE_DETERMINISTIC
assert outcome.audit.failure_class == OFFICE_FAILURE_ENVIRONMENT_UNAVAILABLE
assert outcome.audit.failure_class == OFFICE_FAILURE_CONTRACT
assert outcome.audit.failure_class == OFFICE_FAILURE_RECOVERABLE
```

- [ ] **Step 6: Run office parser tests to verify GREEN**

Run:

```powershell
conda run -n test python -m pytest tests/test_office_parser.py -q
```

Expected: PASS.

- [ ] **Step 7: Commit Task 1**

```powershell
git add src/services/office_parser.py tests/test_office_parser.py
git commit -m "Add Rust Office failure classification"
```

---

### Task 2: Propagate Failure Class Through Scanner Metrics

**Files:**
- Modify: `src/services/scan_metrics.py`
- Modify: `src/services/file_scanner.py`
- Modify: `tests/test_file_scanner.py`

- [ ] **Step 1: Write failing scanner propagation assertions**

In `tests/test_file_scanner.py`, update the fake `OfficeParseAudit` constructor values in existing Office audit tests to include `failure_class`.

For `test_scan_files_records_python_fallback_audit_for_office_file`, use:

```python
            audit=OfficeParseAudit(
                attempted_backend="rust_office_oxide_v1",
                fallback_backend="python_office_v1",
                fallback_reason="RUST_OFFICE_PARSE_FAILED: bad zip",
                rust_duration_ms=11,
                fallback_duration_ms=19,
                failure_class="recoverable_parser_failure",
            ),
```

Add this assertion near the existing audit assertions:

```python
    assert detail.failure_class == "recoverable_parser_failure"
```

For the Rust success audit test, use:

```python
            audit=OfficeParseAudit(
                attempted_backend="rust_office_oxide_v1",
                rust_duration_ms=7,
                failure_class="",
            ),
```

Add this assertion:

```python
    assert detail.failure_class == ""
```

- [ ] **Step 2: Run scanner tests to verify RED**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py -q
```

Expected: FAIL because `ReparseDetail` does not expose `failure_class`.

- [ ] **Step 3: Add failure class to ReparseDetail**

In `src/services/scan_metrics.py`, add the field to `ReparseDetail` after `fallback_duration_ms`:

```python
    failure_class: str = ""
```

Add it to `to_dict()`:

```python
            "failure_class": self.failure_class,
```

- [ ] **Step 4: Populate failure class from Office audit**

In `src/services/file_scanner.py::_record_reparse_detail()`, add the field to the `ReparseDetail` constructor:

```python
                failure_class=office_audit.failure_class
                if office_audit is not None
                else "",
```

- [ ] **Step 5: Run scanner propagation tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py -q
```

Expected: PASS.

- [ ] **Step 6: Commit Task 2**

```powershell
git add src/services/scan_metrics.py src/services/file_scanner.py tests/test_file_scanner.py
git commit -m "Propagate Office failure class through scanner metrics"
```

---

### Task 3: Add Fallback Policy Version To Parser Profile

**Files:**
- Modify: `src/services/scan_planner.py`
- Modify: `src/core/config.py`
- Modify: `config/settings.example.yaml`
- Modify: `tests/test_scan_planner.py`
- Modify: `tests/test_config.py`

- [ ] **Step 1: Write failing parser profile tests**

In `tests/test_scan_planner.py::test_build_parser_profile_uses_summary_limits_when_requested`, add this expected key:

```python
        "office_fallback_policy_version": "hybrid_v1",
```

In `tests/test_build_parser_profile_includes_document_parser_defaults`, add:

```python
    assert profile["office_fallback_policy_version"] == "hybrid_v1"
```

In `test_build_parser_profile_includes_rust_office_backend_and_fallback_keys`, add this config input:

```python
            "office_fallback_policy_version": "hybrid_v2",
```

Add these assertions:

```python
    assert profile["office_fallback_policy_version"] == "hybrid_v2"
    assert '"office_fallback_policy_version":"hybrid_v2"' in serialized
```

- [ ] **Step 2: Write failing config test**

Append this test to `tests/test_config.py`:

```python
def test_scanner_config_exposes_office_fallback_policy_version(tmp_path: Path):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / "settings.linux.yaml").write_text(
        "\n".join(
            [
                "scanner:",
                "  allowed_extensions:",
                "    - .docx",
                "  ignored_patterns: []",
                "  max_workers: 1",
                "  excel_max_rows: 50",
                "  pdf_max_pages: 5",
                "  text_max_chars: 6000",
                "  office_fallback_policy_version: hybrid_v2",
            ]
        ),
        encoding="utf-8",
    )
    cfg = object.__new__(Config)
    cfg._settings = Config._build_settings(config_dir, system_name="Linux")

    assert cfg.scanner_config["office_fallback_policy_version"] == "hybrid_v2"
```

If `Path` or `Config` is not already imported in `tests/test_config.py`, add:

```python
from pathlib import Path

from src.core.config import Config
```

- [ ] **Step 3: Run profile/config tests to verify RED**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_planner.py tests/test_config.py -q
```

Expected: FAIL because `office_fallback_policy_version` is not in `scanner_config` or parser profile.

- [ ] **Step 4: Add default policy version to ScanPlanner**

In `src/services/scan_planner.py`, add near other defaults:

```python
DEFAULT_OFFICE_FALLBACK_POLICY_VERSION = "hybrid_v1"
```

In `_add_document_profile()`, after `office_legacy_extensions_enabled`, add:

```python
        profile["office_fallback_policy_version"] = str(
            self.scanner_cfg.get(
                "office_fallback_policy_version",
                DEFAULT_OFFICE_FALLBACK_POLICY_VERSION,
            )
        ).strip()
```

- [ ] **Step 5: Expose policy version from Config**

In `src/core/config.py::scanner_config`, after `office_legacy_extensions_enabled`, add:

```python
            "office_fallback_policy_version": str(
                getattr(
                    self._settings.scanner,
                    "office_fallback_policy_version",
                    "hybrid_v1",
                )
            ).strip(),
```

- [ ] **Step 6: Document policy version in example config**

In `config/settings.example.yaml`, under the existing `scanner:` Office parser fields, add:

```yaml
  office_fallback_policy_version: "hybrid_v1"
```

Place it near `office_fallback_after_timeout` so the fallback controls stay together.

- [ ] **Step 7: Run profile/config tests to verify GREEN**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_planner.py tests/test_config.py -q
```

Expected: PASS.

- [ ] **Step 8: Commit Task 3**

```powershell
git add src/services/scan_planner.py src/core/config.py config/settings.example.yaml tests/test_scan_planner.py tests/test_config.py
git commit -m "Add Office fallback policy version to parser profile"
```

---

### Task 4: Render Failure Class In Benchmark Output

**Files:**
- Modify: `scripts/benchmark_scanner.py`
- Modify: `tests/test_benchmark_scanner.py`

- [ ] **Step 1: Write failing benchmark JSON test**

In `tests/test_benchmark_scanner.py`, update `_make_reparse_detail()` to accept:

```python
    failure_class: str = "",
```

and pass it into the `ReparseDetail` constructor:

```python
        failure_class=failure_class,
```

In `test_build_benchmark_payload_preserves_office_fallback_audit_fields`, add:

```python
            failure_class="recoverable_parser_failure",
```

and assert:

```python
    assert payload["reparse_details"][0]["failure_class"] == (
        "recoverable_parser_failure"
    )
```

- [ ] **Step 2: Write failing benchmark Markdown test**

In `tests/test_benchmark_scanner.py::test_render_markdown_report_contains_stage_and_extension_metrics`, update the sample reparse detail dictionary to include:

```python
                "failure_class": "environment_unavailable",
```

Add assertions:

```python
    assert "failure_class" in markdown
    assert "environment_unavailable" in markdown
    assert "cannot evaluate Rust parser performance" in markdown
```

- [ ] **Step 3: Run benchmark tests to verify RED**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected: FAIL because Markdown does not render `failure_class` or the environment-unavailable explanation yet.

- [ ] **Step 4: Render failure_class in Markdown reparse details**

In `scripts/benchmark_scanner.py::render_markdown_report()`, change the Reparse Details header from:

```python
            "| extension | cache_miss_reason | parse_duration_ms | parse_status | "
            "attempted_backend | fallback_backend | fallback_reason | "
            "rust_duration_ms | fallback_duration_ms | path |",
            "|---|---|---:|---|---|---|---|---:|---:|---|",
```

to:

```python
            "| extension | cache_miss_reason | parse_duration_ms | parse_status | "
            "attempted_backend | fallback_backend | fallback_reason | "
            "failure_class | rust_duration_ms | fallback_duration_ms | path |",
            "|---|---|---:|---|---|---|---|---|---:|---:|---|",
```

Inside the row loop, add:

```python
            failure_class = str(item.get("failure_class", "")).replace("|", "/")
```

Change the row format to include `failure_class`:

```python
                "| {extension} | {cache_miss_reason} | {parse_duration_ms} | "
                "{parse_status} | {attempted_backend} | {fallback_backend} | "
                "{fallback_reason} | {failure_class} | {rust_duration_ms} | "
                "{fallback_duration_ms} | {path} |".format(
                    extension=item.get("extension", ""),
                    cache_miss_reason=item.get("cache_miss_reason", ""),
                    parse_duration_ms=item.get("parse_duration_ms", 0),
                    parse_status=item.get("parse_status", ""),
                    attempted_backend=item.get("attempted_backend", ""),
                    fallback_backend=item.get("fallback_backend", ""),
                    fallback_reason=fallback_reason,
                    failure_class=failure_class,
                    rust_duration_ms=item.get("rust_duration_ms", 0),
                    fallback_duration_ms=item.get("fallback_duration_ms", 0),
                    path=item.get("path", ""),
                )
```

Update the empty row to:

```python
        lines.append("| (none) |  | 0 |  |  |  |  |  | 0 | 0 |  |")
```

- [ ] **Step 5: Add environment-unavailable explanation**

Still in `render_markdown_report()`, after the Reparse Details block and before `return "\n".join(lines) + "\n"`, add:

```python
    failure_classes = {
        str(item.get("failure_class", ""))
        for item in reparse_details
        if item.get("failure_class")
    }
    if "environment_unavailable" in failure_classes:
        lines.extend(
            [
                "",
                "## Office Failure Class Notes",
                "",
                "- `environment_unavailable`: Rust Office parser did not start, "
                "so this run cannot evaluate Rust parser performance for those files.",
            ]
        )
```

- [ ] **Step 6: Run benchmark tests to verify GREEN**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected: PASS.

- [ ] **Step 7: Commit Task 4**

```powershell
git add scripts/benchmark_scanner.py tests/test_benchmark_scanner.py
git commit -m "Render Office failure class in scanner benchmark"
```

---

### Task 5: Final Verification And Benchmark Evidence

**Files:**
- Modify only if verification exposes a defect:
  - `src/services/office_parser.py`
  - `src/services/scan_metrics.py`
  - `src/services/file_scanner.py`
  - `src/services/scan_planner.py`
  - `src/core/config.py`
  - `scripts/benchmark_scanner.py`
  - corresponding tests

- [ ] **Step 1: Run focused Python tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_office_parser.py tests/test_file_scanner.py tests/test_scan_planner.py tests/test_config.py tests/test_benchmark_scanner.py -q
```

Expected: PASS.

- [ ] **Step 2: Run Rust Office parser tests**

Run:

```powershell
Push-Location rust\office_parser
cargo test
cargo build --release
Pop-Location
```

Expected: `cargo test` PASS and release binary built at `rust/office_parser/target/release/ai-daily-office-parser.exe` on Windows or `rust/office_parser/target/release/ai-daily-office-parser` on Linux.

- [ ] **Step 3: Run scanner benchmark with isolated index DB**

Run in PowerShell:

```powershell
$env:DAILY_REPORT_SCANNER__INDEX_DB_PATH = "$env:TEMP\ai-daily-report-hybrid-office-scan-index.sqlite3"
conda run -n test python scripts\benchmark_scanner.py `
  --start-date 2026-05-11 `
  --end-date 2026-05-25 `
  --json-out "$env:TEMP\scanner-hybrid-office.json" `
  --markdown-out "$env:TEMP\scanner-hybrid-office.md"
Remove-Item Env:\DAILY_REPORT_SCANNER__INDEX_DB_PATH
```

Expected:

- JSON exists at `$env:TEMP\scanner-hybrid-office.json`.
- Markdown exists at `$env:TEMP\scanner-hybrid-office.md`.
- Office rows in `reparse_details` contain `failure_class`.
- `.xlsx` successes show `parser_backend = rust_xlsx_bounded_v1`.
- `environment_unavailable` rows, if present, are explained as not valid Rust parser performance evidence.

- [ ] **Step 4: Inspect benchmark evidence**

Run:

```powershell
conda run -n test python -c "import json, os, pathlib; p=pathlib.Path(os.environ['TEMP'])/'scanner-hybrid-office.json'; data=json.loads(p.read_text(encoding='utf-8')); rows=[r for r in data.get('reparse_details', []) if r.get('extension') in {'.docx','.xlsx','.pptx','.doc','.xls','.ppt'}]; print(json.dumps([{k:r.get(k) for k in ['extension','parser_backend','attempted_backend','fallback_backend','fallback_reason','failure_class','rust_duration_ms','fallback_duration_ms','parse_duration_ms','parse_status']} for r in rows], ensure_ascii=False, indent=2))"
```

Expected: output clearly separates Rust successes, Python fallback rows, deterministic no-fallback rows, and environment / contract failure rows.

- [ ] **Step 5: Run full Python suite**

Run:

```powershell
conda run -n test python -m pytest tests -q
```

Expected: PASS.

- [ ] **Step 6: Run compile and whitespace checks**

Run:

```powershell
conda run -n test python -m compileall main.py src tests
git diff --check
```

Expected: compileall PASS and `git diff --check` returns exit code 0.

- [ ] **Step 7: Update docs only if implementation changed the user-facing contract**

If `failure_class` appears in benchmark JSON and Markdown exactly as planned, update `docs/scanner-backends.md` by adding this bullet under Benchmark Evidence:

```markdown
- `failure_class` classifies Rust Office parser failures as `deterministic`, `environment_unavailable`, `contract_failure`, or `recoverable_parser_failure`; `environment_unavailable` rows mean the Rust parser did not start and should not be used as Rust parser performance evidence.
```

Run:

```powershell
rg -n "failure_class|environment_unavailable" docs\scanner-backends.md scripts\benchmark_scanner.py tests
git diff --check
```

Expected: docs and code agree on `failure_class` terminology, and whitespace check passes.

- [ ] **Step 8: Commit Task 5**

```powershell
git add src/services/office_parser.py src/services/scan_metrics.py src/services/file_scanner.py src/services/scan_planner.py src/core/config.py scripts/benchmark_scanner.py config/settings.example.yaml docs/scanner-backends.md tests/test_office_parser.py tests/test_file_scanner.py tests/test_scan_planner.py tests/test_config.py tests/test_benchmark_scanner.py
git commit -m "Verify Rust Office hybrid fallback policy"
```

If Task 5 only produced benchmark files under `%TEMP%` and no repository files changed, skip the commit and record the verification evidence in the final implementation summary.

---

## Self-Review

Spec coverage:

- Cold scanner run is covered by Task 5 benchmark with isolated index DB.
- Hybrid Office fallback policy is covered by Task 1 classifier and tests.
- Deterministic no-fallback is covered by timeout and bad `.xlsx` ZIP tests.
- Environment-unavailable fallback is covered by Task 1 and Task 4 benchmark explanation.
- Contract failure fallback is covered by Task 1.
- Parser profile isolation is covered by Task 3.
- Benchmark output is covered by Task 4 and Task 5.
- Batch parser and Rust scanner core are excluded from all tasks.

Placeholder scan:

- No plan step relies on unspecified behavior.
- Each code-changing step names the exact file and the concrete code to add or replace.
- Each verification step has a command and expected result.

Type consistency:

- `failure_class` is added first to `OfficeParseAudit`, then to `ReparseDetail`, then to benchmark dictionaries.
- `office_fallback_policy_version` is added first to `Config.scanner_config`, then consumed by `ScanPlanner`.
- Failure class values match the spec exactly: `deterministic`, `environment_unavailable`, `contract_failure`, `recoverable_parser_failure`.
