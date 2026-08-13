# Rust Office Oxide Parser Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current Python Office per-file subprocess parser with a Rust `office_oxide` CLI primary backend, while retaining Python fallback, timeout isolation, parse cache correctness, and benchmark auditability.

**Architecture:** Python remains the scanner owner. A new Rust CLI under `rust/office_parser/` reads a JSON request from stdin and emits a `FileContext`-compatible JSON object. Python adds `src/services/office_parser.py` for Rust runner, fallback decisions, and audit metadata; `FileScanner` routes Office files through it and keeps PDF on the existing `pdf_text_v1` path.

**Tech Stack:** Python 3.10+, Pydantic, pytest, Dynaconf YAML config, Rust stable, Cargo, `serde`, `serde_json`, `office_oxide`, optional `sharepoint-to-text` after smoke validation.

---

## Scope Check

This plan is one coherent subsystem: scanner Office parser backend replacement. It deliberately leaves PDF Rust parsing, OCR, long-running Rust workers, and automatic `.doc/.ppt` scan-range expansion out of scope.

## File Structure

- Create: `rust/office_parser/Cargo.toml`
  Rust package metadata and dependencies for the Office parser CLI.
- Create: `rust/office_parser/src/lib.rs`
  Request/response structs, extension validation, truncation, `office_oxide` parse entry, and Rust unit tests.
- Create: `rust/office_parser/src/main.rs`
  Small stdin/stdout JSON CLI wrapper.
- Create: `tests/test_office_parser.py`
  Python unit tests for Rust runner, fallback decisions, timeout behavior, and sharepoint fallback adapter.
- Create: `src/services/office_parser.py`
  Python Rust runner, fallback orchestration, audit metadata, sharepoint adapter, and constants.
- Modify: `src/core/config.py`
  Expose Office parser config defaults as built-in pickleable values.
- Modify: `config/settings.example.yaml`
  Document Rust Office parser defaults without touching local secrets.
- Modify: `src/services/scan_planner.py`
  Include Office backend, fallback, and Rust parser options in the cache profile.
- Modify: `tests/test_config.py`
  Cover new config defaults and pickleability.
- Modify: `tests/test_scan_planner.py`
  Cover new parser profile cache keys.
- Modify: `src/services/scan_metrics.py`
  Extend `ReparseDetail` with attempted/fallback timing and reason fields.
- Modify: `scripts/benchmark_scanner.py`
  Render fallback fields in JSON/Markdown and backend summary.
- Modify: `tests/test_benchmark_scanner.py`
  Lock benchmark payload and Markdown changes.
- Modify: `src/services/file_scanner.py`
  Route Office files to Rust/fallback path, keep PDF on existing path, and preserve cache writes.
- Modify: `tests/test_file_scanner.py`
  Cover scanner routing, fallback audit, cache backend isolation, and legacy extension behavior.
- Modify: `README.md`
  Add concise build and benchmark instructions for Rust Office parser.

## Task 1: Rust Office Parser Package Skeleton

**Files:**
- Create: `rust/office_parser/Cargo.toml`
- Create: `rust/office_parser/src/lib.rs`
- Create: `rust/office_parser/src/main.rs`

- [ ] **Step 1: Create failing Rust tests for request validation and truncation**

Create `rust/office_parser/src/lib.rs` with this initial test-first content:

```rust
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const RUST_OFFICE_BACKEND: &str = "rust_office_oxide_v1";

#[derive(Debug, Deserialize)]
pub struct OfficeParseRequest {
    pub file_path: PathBuf,
    pub file_type: String,
    pub limits: BTreeMap<String, serde_json::Value>,
    pub parser_backend: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileContextOut {
    pub file_path: String,
    pub file_type: String,
    pub content: String,
    pub error: Option<String>,
    pub parser_backend: String,
    pub truncated: bool,
}

pub fn normalize_file_type(file_type: &str) -> String {
    file_type.trim().to_ascii_lowercase()
}

pub fn is_supported_office_type(file_type: &str) -> bool {
    matches!(
        normalize_file_type(file_type).as_str(),
        ".docx" | ".xlsx" | ".pptx" | ".doc" | ".xls" | ".ppt"
    )
}

pub fn positive_limit(
    limits: &BTreeMap<String, serde_json::Value>,
    key: &str,
    default_value: usize,
) -> usize {
    limits
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_value)
}

pub fn truncate_content(content: &str, max_chars: usize) -> (String, bool) {
    let max_chars = max_chars.max(1);
    let mut output = String::new();
    let mut truncated = false;
    for (index, character) in content.chars().enumerate() {
        if index >= max_chars {
            truncated = true;
            break;
        }
        output.push(character);
    }
    (output, truncated)
}

pub fn unsupported_context(request: &OfficeParseRequest) -> FileContextOut {
    let file_type = normalize_file_type(&request.file_type);
    FileContextOut {
        file_path: request.file_path.to_string_lossy().to_string(),
        file_type: file_type.clone(),
        content: String::new(),
        error: Some(format!("RUST_OFFICE_UNSUPPORTED_EXTENSION: {file_type}")),
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_office_type_is_case_insensitive() {
        assert!(is_supported_office_type(".DOCX"));
        assert!(is_supported_office_type(".xlsx"));
        assert!(is_supported_office_type(".PPT"));
        assert!(!is_supported_office_type(".pdf"));
    }

    #[test]
    fn positive_limit_uses_default_for_missing_invalid_or_zero_values() {
        let mut limits = BTreeMap::new();
        limits.insert("good".to_string(), serde_json::json!(12));
        limits.insert("zero".to_string(), serde_json::json!(0));
        limits.insert("text".to_string(), serde_json::json!("bad"));

        assert_eq!(positive_limit(&limits, "good", 6), 12);
        assert_eq!(positive_limit(&limits, "zero", 6), 6);
        assert_eq!(positive_limit(&limits, "text", 6), 6);
        assert_eq!(positive_limit(&limits, "missing", 6), 6);
    }

    #[test]
    fn truncate_content_preserves_utf8_boundaries() {
        let (content, truncated) = truncate_content("甲乙丙丁", 3);

        assert_eq!(content, "甲乙丙");
        assert!(truncated);
    }

    #[test]
    fn unsupported_context_is_file_context_compatible() {
        let request = OfficeParseRequest {
            file_path: PathBuf::from("/tmp/report.pdf"),
            file_type: ".PDF".to_string(),
            limits: BTreeMap::new(),
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
        };

        let context = unsupported_context(&request);

        assert_eq!(context.file_path, "/tmp/report.pdf");
        assert_eq!(context.file_type, ".pdf");
        assert_eq!(
            context.error,
            Some("RUST_OFFICE_UNSUPPORTED_EXTENSION: .pdf".to_string())
        );
        assert_eq!(context.parser_backend, RUST_OFFICE_BACKEND);
        assert!(!context.truncated);
    }
}
```

- [ ] **Step 2: Add package metadata**

Create `rust/office_parser/Cargo.toml`:

```toml
[package]
name = "ai-daily-office-parser"
version = "0.1.0"
edition = "2021"

[dependencies]
office_oxide = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Create `rust/office_parser/src/main.rs`:

```rust
use std::io::{self, Read};

use ai_daily_office_parser::{parse_office_file, OfficeParseRequest};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let request: OfficeParseRequest = serde_json::from_str(&input)?;
    let context = parse_office_file(&request);
    println!("{}", serde_json::to_string(&context)?);
    Ok(())
}
```

This should fail because `parse_office_file` is not defined yet.

- [ ] **Step 3: Run Rust tests to verify the initial package compiles far enough**

Run:

```bash
cd rust/office_parser && cargo test
```

Expected: FAIL with an unresolved import or missing function for `parse_office_file`.

- [ ] **Step 4: Implement the Rust parse entry with `office_oxide`**

Add this function to `rust/office_parser/src/lib.rs` below `unsupported_context`:

```rust
pub fn parse_office_file(request: &OfficeParseRequest) -> FileContextOut {
    let file_type = normalize_file_type(&request.file_type);
    if !is_supported_office_type(&file_type) {
        return unsupported_context(request);
    }

    let max_chars = positive_limit(&request.limits, "document_excerpt_max_chars", 6000);

    match office_oxide::Document::open(&request.file_path) {
        Ok(document) => {
            let markdown = document.to_markdown();
            let content = if markdown.trim().is_empty() {
                "No Office text extracted".to_string()
            } else {
                markdown
            };
            let (content, truncated) = truncate_content(&content, max_chars);
            FileContextOut {
                file_path: request.file_path.to_string_lossy().to_string(),
                file_type,
                content,
                error: None,
                parser_backend: RUST_OFFICE_BACKEND.to_string(),
                truncated,
            }
        }
        Err(error) => FileContextOut {
            file_path: request.file_path.to_string_lossy().to_string(),
            file_type,
            content: String::new(),
            error: Some(format!("RUST_OFFICE_PARSE_FAILED: {error}")),
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
            truncated: false,
        },
    }
}
```

- [ ] **Step 5: Run Rust tests and build**

Run:

```bash
cd rust/office_parser && cargo test
cd rust/office_parser && cargo build --release
```

Expected: both commands PASS and `rust/office_parser/target/release/ai-daily-office-parser` exists. If the `office_oxide` API name differs from the docs, adjust only this function and re-run the same commands.

- [ ] **Step 6: Commit**

```bash
git add rust/office_parser/Cargo.toml rust/office_parser/Cargo.lock rust/office_parser/src/lib.rs rust/office_parser/src/main.rs
git commit -m "Add Rust Office parser CLI"
```

## Task 2: Config and Parser Profile Keys

**Files:**
- Modify: `src/core/config.py`
- Modify: `src/services/scan_planner.py`
- Modify: `config/settings.example.yaml`
- Test: `tests/test_config.py`
- Test: `tests/test_scan_planner.py`

- [ ] **Step 1: Write config tests for Office parser defaults**

Add to `tests/test_config.py`:

```python
def test_scanner_config_exposes_office_parser_defaults_when_keys_absent():
    """Office parser 缺省时应优先 Rust，并保留 Python fallback。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".docx"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["office_parser_backend"] == "rust_office_oxide_v1"
    assert scanner_config["rust_office_parser_bin"] == (
        "rust/office_parser/target/release/ai-daily-office-parser"
    )
    assert scanner_config["office_parser_fallback_enabled"] is True
    assert scanner_config["office_parser_fallback_order"] == [
        "python_office_v1",
        "python_sharepoint_text_v1",
    ]
    assert scanner_config["office_fallback_after_timeout"] is False
    assert scanner_config["office_external_fallback"] == "disabled"
    assert scanner_config["office_legacy_extensions_enabled"] is False
```

Extend `test_scanner_config_uses_builtin_containers_and_is_picklable()` by adding these YAML lines:

```python
                "  office_parser_backend: rust_office_oxide_v1",
                "  rust_office_parser_bin: rust/office_parser/target/release/ai-daily-office-parser",
                "  office_parser_fallback_enabled: true",
                "  office_parser_fallback_order:",
                "    - python_office_v1",
                "    - python_sharepoint_text_v1",
                "  office_fallback_after_timeout: false",
                "  office_external_fallback: disabled",
                "  office_legacy_extensions_enabled: false",
```

Add assertions in the same test:

```python
    assert scanner_config["office_parser_backend"] == "rust_office_oxide_v1"
    assert isinstance(scanner_config["office_parser_fallback_order"], list)
    assert scanner_config["office_parser_fallback_enabled"] is True
    assert scanner_config["office_fallback_after_timeout"] is False
```

- [ ] **Step 2: Run config tests and verify they fail**

Run:

```bash
conda run -n test python -m pytest tests/test_config.py -q
```

Expected: FAIL because the new Office parser config keys are missing.

- [ ] **Step 3: Implement config defaults**

In `src/core/config.py`, add these constants near imports:

```python
DEFAULT_OFFICE_PARSER_BACKEND = "rust_office_oxide_v1"
DEFAULT_RUST_OFFICE_PARSER_BIN = (
    "rust/office_parser/target/release/ai-daily-office-parser"
)
DEFAULT_OFFICE_FALLBACK_ORDER = [
    "python_office_v1",
    "python_sharepoint_text_v1",
]
```

Inside `Config.scanner_config`, add these keys to the initial `cfg` dict after `rust_discovery_bin`:

```python
            "office_parser_backend": str(
                getattr(
                    self._settings.scanner,
                    "office_parser_backend",
                    DEFAULT_OFFICE_PARSER_BACKEND,
                )
            ).strip(),
            "rust_office_parser_bin": getattr(
                self._settings.scanner,
                "rust_office_parser_bin",
                DEFAULT_RUST_OFFICE_PARSER_BIN,
            ),
            "office_parser_fallback_enabled": bool(
                getattr(self._settings.scanner, "office_parser_fallback_enabled", True)
            ),
            "office_parser_fallback_order": self._to_builtin_value(
                getattr(
                    self._settings.scanner,
                    "office_parser_fallback_order",
                    DEFAULT_OFFICE_FALLBACK_ORDER,
                )
            ),
            "office_fallback_after_timeout": bool(
                getattr(self._settings.scanner, "office_fallback_after_timeout", False)
            ),
            "office_external_fallback": str(
                getattr(self._settings.scanner, "office_external_fallback", "disabled")
            ).strip().lower(),
            "office_legacy_extensions_enabled": bool(
                getattr(self._settings.scanner, "office_legacy_extensions_enabled", False)
            ),
```

- [ ] **Step 4: Write parser profile tests**

Add to `tests/test_scan_planner.py`:

```python
def test_build_parser_profile_includes_rust_office_backend_and_fallback_keys():
    """Office backend/fallback 配置必须进入 cache key，避免跨 backend 复用旧内容。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "office_parser_backend": "rust_office_oxide_v1",
            "rust_office_parser_bin": "rust/office_parser/target/release/ai-daily-office-parser",
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": [
                "python_office_v1",
                "python_sharepoint_text_v1",
            ],
            "office_fallback_after_timeout": False,
            "office_external_fallback": "disabled",
            "office_legacy_extensions_enabled": False,
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["office_parser_backend"] == "rust_office_oxide_v1"
    assert profile["rust_office_parser_bin"] == (
        "rust/office_parser/target/release/ai-daily-office-parser"
    )
    assert profile["office_parser_fallback_enabled"] is True
    assert profile["office_parser_fallback_order"] == [
        "python_office_v1",
        "python_sharepoint_text_v1",
    ]
    assert profile["office_fallback_after_timeout"] is False
    assert profile["office_external_fallback"] == "disabled"
    assert profile["office_legacy_extensions_enabled"] is False
```

- [ ] **Step 5: Run planner tests and verify they fail**

Run:

```bash
conda run -n test python -m pytest tests/test_scan_planner.py::test_build_parser_profile_includes_rust_office_backend_and_fallback_keys -q
```

Expected: FAIL because the new keys are not present in the parser profile.

- [ ] **Step 6: Implement parser profile keys**

In `src/services/scan_planner.py`, update `_add_document_profile()` after existing `pdf_parser_backend` assignment:

```python
        profile["rust_office_parser_bin"] = self.scanner_cfg.get(
            "rust_office_parser_bin",
            "rust/office_parser/target/release/ai-daily-office-parser",
        )
        profile["office_parser_fallback_enabled"] = bool(
            self.scanner_cfg.get("office_parser_fallback_enabled", True)
        )
        profile["office_parser_fallback_order"] = list(
            self.scanner_cfg.get(
                "office_parser_fallback_order",
                ["python_office_v1", "python_sharepoint_text_v1"],
            )
        )
        profile["office_fallback_after_timeout"] = bool(
            self.scanner_cfg.get("office_fallback_after_timeout", False)
        )
        profile["office_external_fallback"] = str(
            self.scanner_cfg.get("office_external_fallback", "disabled")
        ).strip().lower()
        profile["office_legacy_extensions_enabled"] = bool(
            self.scanner_cfg.get("office_legacy_extensions_enabled", False)
        )
```

Update default expectations in existing tests where they assert the whole profile:

```python
        "office_parser_backend": "rust_office_oxide_v1",
        "rust_office_parser_bin": "rust/office_parser/target/release/ai-daily-office-parser",
        "office_parser_fallback_enabled": True,
        "office_parser_fallback_order": ["python_office_v1", "python_sharepoint_text_v1"],
        "office_fallback_after_timeout": False,
        "office_external_fallback": "disabled",
        "office_legacy_extensions_enabled": False,
```

- [ ] **Step 7: Update example config**

Add under `scanner:` in `config/settings.example.yaml`:

```yaml
  # Office parser defaults: Rust primary, Python fallback.
  office_parser_backend: "rust_office_oxide_v1"
  rust_office_parser_bin: "rust/office_parser/target/release/ai-daily-office-parser"
  office_parser_fallback_enabled: true
  office_parser_fallback_order:
    - "python_office_v1"
    - "python_sharepoint_text_v1"
  office_fallback_after_timeout: false
  office_external_fallback: "disabled"
  # Keep legacy .doc/.ppt out of default scanning until explicitly enabled.
  office_legacy_extensions_enabled: false
```

- [ ] **Step 8: Run config and planner tests**

Run:

```bash
conda run -n test python -m pytest tests/test_config.py tests/test_scan_planner.py -q
```

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/core/config.py src/services/scan_planner.py tests/test_config.py tests/test_scan_planner.py config/settings.example.yaml
git commit -m "Add Office parser config profile keys"
```

## Task 3: Python Office Parser Runner and Fallback Orchestration

**Files:**
- Create: `src/services/office_parser.py`
- Test: `tests/test_office_parser.py`

- [ ] **Step 1: Write tests for Rust runner success, invalid payload, timeout, and fallback**

Create `tests/test_office_parser.py`:

```python
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.models.schemas import FileContext
from src.services.office_parser import (
    OFFICE_RUST_FILE_TYPES,
    RUST_OFFICE_BACKEND,
    OfficeParseAudit,
    OfficeParseOutcome,
    RustOfficeParserRunner,
    parse_office_with_fallback,
    parse_with_sharepoint_text,
)


def test_office_rust_file_types_include_legacy_office_extensions():
    assert OFFICE_RUST_FILE_TYPES == {
        ".docx",
        ".xlsx",
        ".pptx",
        ".doc",
        ".xls",
        ".ppt",
    }


def test_rust_runner_returns_file_context_from_valid_payload(tmp_path, monkeypatch):
    sample = tmp_path / "report.xlsx"
    sample.write_bytes(b"fake")

    completed = SimpleNamespace(
        returncode=0,
        stdout=(
            '{"file_path":"'
            + str(sample)
            + '","file_type":".xlsx","content":"ok","error":null,'
            '"parser_backend":"rust_office_oxide_v1","truncated":false}'
        ),
        stderr="",
    )

    def fake_run(*args, **kwargs):
        assert str(args[0][0]).endswith("ai-daily-office-parser")
        assert kwargs["timeout"] == 12
        assert '"file_type": ".xlsx"' in kwargs["input"]
        return completed

    monkeypatch.setattr("src.services.office_parser.subprocess.run", fake_run)

    context, duration_ms = RustOfficeParserRunner(
        "rust/office_parser/target/release/ai-daily-office-parser"
    ).parse(sample, ".xlsx", {"document_excerpt_max_chars": 6000}, 12)

    assert context == FileContext(
        file_path=str(sample),
        file_type=".xlsx",
        content="ok",
        error=None,
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )
    assert duration_ms >= 0


def test_rust_runner_returns_error_context_for_invalid_json(tmp_path, monkeypatch):
    sample = tmp_path / "bad.docx"
    sample.write_bytes(b"fake")
    completed = SimpleNamespace(returncode=0, stdout="not-json", stderr="")
    monkeypatch.setattr(
        "src.services.office_parser.subprocess.run",
        lambda *args, **kwargs: completed,
    )

    context, _ = RustOfficeParserRunner("parser").parse(
        sample,
        ".docx",
        {"document_excerpt_max_chars": 6000},
        12,
    )

    assert context.content == ""
    assert context.error is not None
    assert context.error.startswith("RUST_OFFICE_INVALID_JSON:")
    assert context.parser_backend == RUST_OFFICE_BACKEND


def test_rust_runner_returns_timeout_context(tmp_path, monkeypatch):
    sample = tmp_path / "slow.pptx"
    sample.write_bytes(b"fake")

    def fake_run(*args, **kwargs):
        raise TimeoutError("expired")

    monkeypatch.setattr("src.services.office_parser.subprocess.run", fake_run)

    context, _ = RustOfficeParserRunner("parser").parse(
        sample,
        ".pptx",
        {"document_excerpt_max_chars": 6000},
        9,
    )

    assert context.error == "RUST_OFFICE_TIMEOUT: file parse exceeded 9s"
    assert context.parser_backend == RUST_OFFICE_BACKEND


def test_parse_office_with_fallback_uses_python_when_rust_fails(tmp_path, monkeypatch):
    sample = tmp_path / "report.docx"
    sample.write_bytes(b"fake")
    rust_context = FileContext(
        file_path=str(sample),
        file_type=".docx",
        content="",
        error="RUST_OFFICE_PARSE_FAILED: bad zip",
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )

    class FakeRunner:
        def parse(self, file_path, file_type, limits, timeout_seconds):
            return rust_context, 5

    def fake_python_fallback(file_path, file_type, limits):
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="python fallback",
            error=None,
            parser_backend="python_office_v1",
            truncated=False,
        )

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".docx",
        limits={"document_excerpt_max_chars": 6000},
        scanner_cfg={
            "office_parser_backend": RUST_OFFICE_BACKEND,
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": ["python_office_v1"],
            "office_fallback_after_timeout": False,
        },
        timeout_seconds=12,
        rust_runner=FakeRunner(),
        python_fallback=fake_python_fallback,
    )

    assert outcome.context.content == "python fallback"
    assert outcome.audit == OfficeParseAudit(
        attempted_backend=RUST_OFFICE_BACKEND,
        fallback_backend="python_office_v1",
        fallback_reason="RUST_OFFICE_PARSE_FAILED: bad zip",
        rust_duration_ms=5,
        fallback_duration_ms=0,
    )


def test_parse_office_with_fallback_does_not_fallback_after_timeout_by_default(tmp_path):
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

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".xlsx",
        limits={},
        scanner_cfg={
            "office_parser_backend": RUST_OFFICE_BACKEND,
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": ["python_office_v1"],
            "office_fallback_after_timeout": False,
        },
        timeout_seconds=3,
        rust_runner=FakeRunner(),
        python_fallback=lambda file_path, file_type, limits: pytest.fail(
            "fallback should not run"
        ),
    )

    assert outcome.context.error == "RUST_OFFICE_TIMEOUT: file parse exceeded 3s"
    assert outcome.audit.fallback_backend == ""


def test_parse_with_sharepoint_text_reports_missing_dependency(tmp_path, monkeypatch):
    sample = tmp_path / "legacy.doc"
    sample.write_bytes(b"fake")

    def fake_import(name):
        raise ModuleNotFoundError(name)

    context = parse_with_sharepoint_text(
        sample,
        ".doc",
        {"document_excerpt_max_chars": 6000},
        import_module=fake_import,
    )

    assert context.content == ""
    assert context.error == "PYTHON_SHAREPOINT_TEXT_UNAVAILABLE: sharepoint2text"
    assert context.parser_backend == "python_sharepoint_text_v1"
```

- [ ] **Step 2: Run tests and verify they fail**

Run:

```bash
conda run -n test python -m pytest tests/test_office_parser.py -q
```

Expected: FAIL because `src.services.office_parser` does not exist.

- [ ] **Step 3: Implement `src/services/office_parser.py`**

Create `src/services/office_parser.py`:

```python
"""Office parser backend orchestration: Rust primary plus Python fallback."""

from __future__ import annotations

import importlib
import json
import subprocess
from dataclasses import dataclass
from pathlib import Path
from time import perf_counter
from typing import Any, Callable, Mapping

from ..models.schemas import FileContext

RUST_OFFICE_BACKEND = "rust_office_oxide_v1"
PYTHON_OFFICE_BACKEND = "python_office_v1"
PYTHON_SHAREPOINT_TEXT_BACKEND = "python_sharepoint_text_v1"
NOT_PARSED_BACKEND = "not_parsed"
OFFICE_RUST_FILE_TYPES = {".docx", ".xlsx", ".pptx", ".doc", ".xls", ".ppt"}
DEFAULT_RUST_OFFICE_PARSER_BIN = (
    "rust/office_parser/target/release/ai-daily-office-parser"
)


@dataclass(frozen=True, slots=True)
class OfficeParseAudit:
    attempted_backend: str = ""
    fallback_backend: str = ""
    fallback_reason: str = ""
    rust_duration_ms: int = 0
    fallback_duration_ms: int = 0


@dataclass(frozen=True, slots=True)
class OfficeParseOutcome:
    context: FileContext
    audit: OfficeParseAudit


class RustOfficeParserRunner:
    """Run the Rust Office parser CLI and validate its FileContext payload."""

    def __init__(self, binary_path: str | Path):
        self.binary_path = Path(binary_path)

    def parse(
        self,
        file_path: Path,
        file_type: str,
        limits: Mapping[str, Any],
        timeout_seconds: float,
    ) -> tuple[FileContext, int]:
        started_at = perf_counter()
        normalized_type = file_type.lower()
        request = {
            "file_path": str(file_path),
            "file_type": normalized_type,
            "limits": dict(limits),
            "parser_backend": RUST_OFFICE_BACKEND,
        }
        try:
            completed = subprocess.run(
                [str(self._resolve_binary_path())],
                input=json.dumps(request, ensure_ascii=False, indent=2),
                text=True,
                capture_output=True,
                timeout=float(timeout_seconds),
                check=False,
            )
        except subprocess.TimeoutExpired:
            return (
                _error_context(
                    file_path,
                    normalized_type,
                    f"RUST_OFFICE_TIMEOUT: file parse exceeded {timeout_seconds:g}s",
                ),
                _elapsed_ms(started_at),
            )
        except TimeoutError:
            return (
                _error_context(
                    file_path,
                    normalized_type,
                    f"RUST_OFFICE_TIMEOUT: file parse exceeded {timeout_seconds:g}s",
                ),
                _elapsed_ms(started_at),
            )
        except OSError as exc:
            return (
                _error_context(file_path, normalized_type, f"RUST_OFFICE_START_FAILED: {exc}"),
                _elapsed_ms(started_at),
            )

        if completed.returncode != 0:
            message = completed.stderr.strip() or f"exit code {completed.returncode}"
            return (
                _error_context(file_path, normalized_type, f"RUST_OFFICE_PARSE_FAILED: {message}"),
                _elapsed_ms(started_at),
            )

        try:
            payload = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            return (
                _error_context(file_path, normalized_type, f"RUST_OFFICE_INVALID_JSON: {exc}"),
                _elapsed_ms(started_at),
            )

        try:
            return FileContext(**payload), _elapsed_ms(started_at)
        except Exception as exc:
            return (
                _error_context(file_path, normalized_type, f"RUST_OFFICE_INVALID_PAYLOAD: {exc}"),
                _elapsed_ms(started_at),
            )

    def _resolve_binary_path(self) -> Path:
        if self.binary_path.is_absolute():
            return self.binary_path
        project_root = Path(__file__).resolve().parent.parent.parent
        return project_root / self.binary_path


PythonFallback = Callable[[Path, str, Mapping[str, Any]], FileContext]


def parse_office_with_fallback(
    *,
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
    scanner_cfg: Mapping[str, Any],
    timeout_seconds: float,
    rust_runner: RustOfficeParserRunner | None = None,
    python_fallback: PythonFallback | None = None,
) -> OfficeParseOutcome:
    normalized_type = file_type.lower()
    backend = str(scanner_cfg.get("office_parser_backend", RUST_OFFICE_BACKEND))
    if backend != RUST_OFFICE_BACKEND:
        context = _run_python_fallback(file_path, normalized_type, limits, python_fallback)
        return OfficeParseOutcome(
            context=context,
            audit=OfficeParseAudit(attempted_backend=context.parser_backend or ""),
        )

    runner = rust_runner or RustOfficeParserRunner(
        scanner_cfg.get("rust_office_parser_bin", DEFAULT_RUST_OFFICE_PARSER_BIN)
    )
    rust_context, rust_duration_ms = runner.parse(
        file_path,
        normalized_type,
        limits,
        timeout_seconds,
    )
    if rust_context.error is None:
        return OfficeParseOutcome(
            context=rust_context,
            audit=OfficeParseAudit(
                attempted_backend=RUST_OFFICE_BACKEND,
                rust_duration_ms=rust_duration_ms,
            ),
        )

    fallback_enabled = bool(scanner_cfg.get("office_parser_fallback_enabled", True))
    fallback_after_timeout = bool(scanner_cfg.get("office_fallback_after_timeout", False))
    is_timeout = rust_context.error.startswith("RUST_OFFICE_TIMEOUT:")
    if not fallback_enabled or (is_timeout and not fallback_after_timeout):
        return OfficeParseOutcome(
            context=rust_context,
            audit=OfficeParseAudit(
                attempted_backend=RUST_OFFICE_BACKEND,
                fallback_reason=rust_context.error or "",
                rust_duration_ms=rust_duration_ms,
            ),
        )

    fallback_started = perf_counter()
    fallback_context = _run_python_fallback(
        file_path,
        normalized_type,
        limits,
        python_fallback,
    )
    fallback_duration_ms = _elapsed_ms(fallback_started)
    if fallback_context.error is None:
        return OfficeParseOutcome(
            context=fallback_context,
            audit=OfficeParseAudit(
                attempted_backend=RUST_OFFICE_BACKEND,
                fallback_backend=fallback_context.parser_backend or "",
                fallback_reason=rust_context.error or "",
                rust_duration_ms=rust_duration_ms,
                fallback_duration_ms=fallback_duration_ms,
            ),
        )

    merged_error = (
        "OFFICE_PARSE_FAILED: "
        f"rust={rust_context.error}; python={fallback_context.error}"
    )
    return OfficeParseOutcome(
        context=FileContext(
            file_path=str(file_path),
            file_type=normalized_type,
            content="",
            error=merged_error,
            parser_backend=RUST_OFFICE_BACKEND,
            truncated=False,
        ),
        audit=OfficeParseAudit(
            attempted_backend=RUST_OFFICE_BACKEND,
            fallback_backend=fallback_context.parser_backend or "",
            fallback_reason=rust_context.error or "",
            rust_duration_ms=rust_duration_ms,
            fallback_duration_ms=fallback_duration_ms,
        ),
    )


def parse_with_sharepoint_text(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
    *,
    import_module: Callable[[str], Any] = importlib.import_module,
) -> FileContext:
    normalized_type = file_type.lower()
    try:
        sharepoint2text = import_module("sharepoint2text")
    except ModuleNotFoundError:
        return FileContext(
            file_path=str(file_path),
            file_type=normalized_type,
            content="",
            error="PYTHON_SHAREPOINT_TEXT_UNAVAILABLE: sharepoint2text",
            parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
            truncated=False,
        )

    try:
        result = next(sharepoint2text.read_file(str(file_path)))
        text = result.get_full_text()
    except Exception as exc:
        return FileContext(
            file_path=str(file_path),
            file_type=normalized_type,
            content="",
            error=f"PYTHON_SHAREPOINT_TEXT_FAILED: {exc}",
            parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
            truncated=False,
        )

    max_chars = _positive_limit(limits, "document_excerpt_max_chars", 6000)
    content, truncated = _truncate_text(text or "No Office text extracted", max_chars)
    return FileContext(
        file_path=str(file_path),
        file_type=normalized_type,
        content=content,
        error=None,
        parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
        truncated=truncated,
    )


def _run_python_fallback(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
    python_fallback: PythonFallback | None,
) -> FileContext:
    if python_fallback is not None:
        return python_fallback(file_path, file_type, limits)
    return parse_with_sharepoint_text(file_path, file_type, limits)


def _error_context(file_path: Path, file_type: str, error: str) -> FileContext:
    return FileContext(
        file_path=str(file_path),
        file_type=file_type,
        content="",
        error=error,
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )


def _elapsed_ms(started_at: float) -> int:
    return max(0, int(round((perf_counter() - started_at) * 1000)))


def _positive_limit(limits: Mapping[str, Any], key: str, default: int) -> int:
    try:
        value = int(limits.get(key, default))
    except (TypeError, ValueError):
        return default
    return value if value > 0 else default


def _truncate_text(text: str, max_chars: int) -> tuple[str, bool]:
    if len(text) <= max_chars:
        return text, False
    return text[:max_chars], True
```

- [ ] **Step 4: Run office parser tests**

Run:

```bash
conda run -n test python -m pytest tests/test_office_parser.py -q
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/services/office_parser.py tests/test_office_parser.py
git commit -m "Add Python Rust Office parser runner"
```

## Task 4: Scanner Routing and Cache Integration

**Files:**
- Modify: `src/services/file_scanner.py`
- Test: `tests/test_file_scanner.py`

- [ ] **Step 1: Write scanner routing tests**

Add to `tests/test_file_scanner.py` near existing document backend tests:

```python
def test_scan_files_uses_rust_office_backend_for_xlsx_in_direct_mode(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """Office 文件应优先进入 Rust Office backend，而不是旧 Python document subprocess。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {
            "allowed_extensions": [".xlsx"],
            "worker_lane_mode": "direct",
            "office_parser_backend": "rust_office_oxide_v1",
            "rust_office_parser_bin": "parser",
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": ["python_office_v1"],
            "office_fallback_after_timeout": False,
        },
    )
    sample = scanner.work_dir / "report.xlsx"
    sample.write_bytes(b"xlsx bytes are not parsed by this routing test")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=43")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )

    def fake_parse_office_with_fallback(**kwargs):
        return file_scanner_module.OfficeParseOutcome(
            context=file_scanner_module.FileContext(
                file_path=str(sample),
                file_type=".xlsx",
                content="rust parsed",
                error=None,
                parser_backend="rust_office_oxide_v1",
                truncated=False,
            ),
            audit=file_scanner_module.OfficeParseAudit(
                attempted_backend="rust_office_oxide_v1",
                rust_duration_ms=4,
            ),
        )

    monkeypatch.setattr(
        file_scanner_module,
        "parse_office_with_fallback",
        fake_parse_office_with_fallback,
    )
    monkeypatch.setattr(
        scanner,
        "_extract_document_content_with_timeout",
        lambda file_path, limits: pytest.fail("old document subprocess should not run"),
    )

    result = scanner.scan_files(date.today(), date.today())

    assert result.success_count == 1
    assert result.contexts[0].parser_backend == "rust_office_oxide_v1"
    assert scanner.last_reparse_details[0].worker_lane == "subprocess"
    assert scanner.last_reparse_details[0].attempted_backend == "rust_office_oxide_v1"
    assert scanner.last_reparse_details[0].rust_duration_ms == 4


def test_scan_files_records_python_fallback_audit_for_office_file(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """Rust 失败但 Python fallback 成功时，benchmark 明细必须保留 fallback 原因。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {
            "allowed_extensions": [".docx"],
            "worker_lane_mode": "direct",
            "office_parser_backend": "rust_office_oxide_v1",
            "office_parser_fallback_enabled": True,
        },
    )
    sample = scanner.work_dir / "report.docx"
    sample.write_bytes(b"docx")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=4")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )

    def fake_parse_office_with_fallback(**kwargs):
        return file_scanner_module.OfficeParseOutcome(
            context=file_scanner_module.FileContext(
                file_path=str(sample),
                file_type=".docx",
                content="python fallback",
                error=None,
                parser_backend="python_office_v1",
                truncated=False,
            ),
            audit=file_scanner_module.OfficeParseAudit(
                attempted_backend="rust_office_oxide_v1",
                fallback_backend="python_office_v1",
                fallback_reason="RUST_OFFICE_PARSE_FAILED: bad zip",
                rust_duration_ms=5,
                fallback_duration_ms=7,
            ),
        )

    monkeypatch.setattr(
        file_scanner_module,
        "parse_office_with_fallback",
        fake_parse_office_with_fallback,
    )

    result = scanner.scan_files(date.today(), date.today())

    detail = scanner.last_reparse_details[0]
    assert result.contexts[0].parser_backend == "python_office_v1"
    assert detail.parser_backend == "python_office_v1"
    assert detail.attempted_backend == "rust_office_oxide_v1"
    assert detail.fallback_backend == "python_office_v1"
    assert detail.fallback_reason == "RUST_OFFICE_PARSE_FAILED: bad zip"
    assert detail.rust_duration_ms == 5
    assert detail.fallback_duration_ms == 7


def test_pdf_stays_on_existing_document_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """PDF 不属于 office_oxide 范围，应继续走现有 PDF document parser。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".pdf"], "worker_lane_mode": "direct"},
    )
    sample = scanner.work_dir / "report.pdf"
    sample.write_bytes(b"%PDF")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=4")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )
    monkeypatch.setattr(
        scanner,
        "_extract_document_content_with_timeout",
        lambda file_path, limits: file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".pdf",
            content="pdf text",
            error=None,
            parser_backend="pdf_text_v1",
            truncated=False,
        ),
    )
    monkeypatch.setattr(
        file_scanner_module,
        "parse_office_with_fallback",
        lambda **kwargs: pytest.fail("office parser should not run for pdf"),
    )

    result = scanner.scan_files(date.today(), date.today())

    assert result.contexts[0].parser_backend == "pdf_text_v1"
```

- [ ] **Step 2: Run routing tests and verify they fail**

Run:

```bash
conda run -n test python -m pytest \
  tests/test_file_scanner.py::test_scan_files_uses_rust_office_backend_for_xlsx_in_direct_mode \
  tests/test_file_scanner.py::test_scan_files_records_python_fallback_audit_for_office_file \
  tests/test_file_scanner.py::test_pdf_stays_on_existing_document_backend -q
```

Expected: FAIL because `file_scanner_module.OfficeParseOutcome` and new routing are not wired.

- [ ] **Step 3: Import Office parser symbols and add audit state**

In `src/services/file_scanner.py`, import:

```python
from .office_parser import (
    OFFICE_RUST_FILE_TYPES,
    OfficeParseAudit,
    OfficeParseOutcome,
    parse_office_with_fallback,
)
```

In `FileScanner.__init__`, after `self.last_reparse_details = []`, add:

```python
        self._office_parse_audits: dict[str, OfficeParseAudit] = {}
```

At the start of `scan_files()`, after `self.last_reparse_details = []`, add:

```python
        self._office_parse_audits = {}
```

- [ ] **Step 4: Route Office files through Rust/fallback**

In `_extract_uncached_content()`, place this branch before `_should_parse_document_direct(file_type)`:

```python
        if self._should_parse_office_rust(file_type):
            return self._extract_office_content_with_timeout(
                file_path,
                file_type,
                effective_limits,
            )
```

Add methods to `FileScanner`:

```python
    def _should_parse_office_rust(self, file_type: str) -> bool:
        """Office 文件使用 Rust 主 backend；PDF 保留现有 PDF parser。"""
        if str(self.scanner_cfg.get("worker_lane_mode", "direct")).lower() != "direct":
            return False
        return file_type.lower() in OFFICE_RUST_FILE_TYPES

    def _extract_office_content_with_timeout(
        self,
        file_path: Path,
        file_type: str,
        limits: dict,
    ) -> FileContext:
        """运行 Rust Office parser，并记录 fallback 审计信息。"""
        timeout_seconds = self.parser_supervisor.resolve_timeout(file_type)
        outcome = parse_office_with_fallback(
            file_path=file_path,
            file_type=file_type,
            limits=limits,
            scanner_cfg=self.scanner_cfg,
            timeout_seconds=timeout_seconds,
            python_fallback=self._parse_python_office_fallback,
        )
        self._office_parse_audits[str(file_path)] = outcome.audit
        return outcome.context

    def _parse_python_office_fallback(
        self,
        file_path: Path,
        file_type: str,
        limits: Mapping[str, object],
    ) -> FileContext:
        """现代 Office 复用现有 Python backend；legacy Office 由 office_parser 处理。"""
        normalized_type = file_type.lower()
        if normalized_type in {".docx", ".xlsx", ".pptx"}:
            return parse_document_file(
                file_path=file_path,
                file_type=normalized_type,
                limits=limits,
                options=DocumentParserOptions(
                    office_parser_backend="python_office_v1",
                    pdf_parser_backend=self.scanner_cfg.get(
                        "pdf_parser_backend",
                        "pdf_text_v1",
                    ),
                    include_pptx_notes=bool(
                        self.scanner_cfg.get("pptx_include_notes", True)
                    ),
                ),
            )
        from .office_parser import parse_with_sharepoint_text

        return parse_with_sharepoint_text(file_path, normalized_type, limits)
```

Add `Mapping` to the `typing` import line:

```python
from typing import List, Mapping, Optional
```

- [ ] **Step 5: Preserve Office audit in reparse detail**

In `_record_reparse_detail()`, before appending `ReparseDetail`, add:

```python
        office_audit = self._office_parse_audits.get(str(self._item_path(item)))
```

Then add these fields to the `ReparseDetail(...)` constructor:

```python
                attempted_backend=office_audit.attempted_backend if office_audit else "",
                fallback_backend=office_audit.fallback_backend if office_audit else "",
                fallback_reason=office_audit.fallback_reason if office_audit else "",
                rust_duration_ms=office_audit.rust_duration_ms if office_audit else 0,
                fallback_duration_ms=(
                    office_audit.fallback_duration_ms if office_audit else 0
                ),
```

- [ ] **Step 6: Update `_infer_worker_lane()`**

In `_infer_worker_lane()`, add this branch before text-like checks:

```python
        if file_type.lower() in OFFICE_RUST_FILE_TYPES:
            return "subprocess"
```

- [ ] **Step 7: Run scanner routing tests**

Run:

```bash
conda run -n test python -m pytest \
  tests/test_file_scanner.py::test_scan_files_uses_rust_office_backend_for_xlsx_in_direct_mode \
  tests/test_file_scanner.py::test_scan_files_records_python_fallback_audit_for_office_file \
  tests/test_file_scanner.py::test_pdf_stays_on_existing_document_backend -q
```

Expected: PASS after Task 5 extends `ReparseDetail`; if this fails now because `ReparseDetail` lacks fields, continue to Task 5 and rerun.

- [ ] **Step 8: Commit after Task 5 makes tests pass**

Do not commit Task 4 until Task 5 extends `ReparseDetail`; the new scanner tests depend on those fields.

## Task 5: ReparseDetail and Benchmark Fallback Fields

**Files:**
- Modify: `src/services/scan_metrics.py`
- Modify: `scripts/benchmark_scanner.py`
- Test: `tests/test_benchmark_scanner.py`
- Test: `tests/test_file_scanner.py`

- [ ] **Step 1: Write benchmark tests for fallback fields**

In `tests/test_benchmark_scanner.py`, update `_make_reparse_detail()` signature:

```python
def _make_reparse_detail(
    extension: str,
    parser_backend: str,
    truncated: bool,
    path: str = "D:\\work\\report.md",
    worker_lane: str = "",
    attempted_backend: str = "",
    fallback_backend: str = "",
    fallback_reason: str = "",
    rust_duration_ms: int = 0,
    fallback_duration_ms: int = 0,
) -> ReparseDetail:
```

Add these fields inside the returned `ReparseDetail(...)`:

```python
        attempted_backend=attempted_backend,
        fallback_backend=fallback_backend,
        fallback_reason=fallback_reason,
        rust_duration_ms=rust_duration_ms,
        fallback_duration_ms=fallback_duration_ms,
```

Add a new test:

```python
def test_build_benchmark_payload_preserves_office_fallback_audit_fields():
    """benchmark JSON 应保留 Rust 尝试和 Python fallback 审计字段。"""
    detail = _make_reparse_detail(
        extension=".docx",
        parser_backend="python_office_v1",
        worker_lane="subprocess",
        truncated=False,
        path="D:\\work\\report.docx",
        attempted_backend="rust_office_oxide_v1",
        fallback_backend="python_office_v1",
        fallback_reason="RUST_OFFICE_PARSE_FAILED: bad zip",
        rust_duration_ms=5,
        fallback_duration_ms=7,
    )

    payload = build_benchmark_payload(
        scan_result=ScanResult(
            total_files=1,
            success_count=1,
            error_count=0,
            contexts=[],
        ),
        run_detail={
            "run_id": 8,
            "discovered_count": 1,
            "reused_count": 0,
            "reparsed_count": 1,
            "total_duration_ms": 20,
            "discovery_duration_ms": 1,
            "inventory_cache_duration_ms": 1,
            "parse_duration_ms": 18,
            "aggregation_duration_ms": 0,
            "success_count": 1,
            "error_count": 0,
            "timeout_count": 0,
        },
        extension_metrics=[],
        reparse_details=[detail],
        start_date=date(2026, 5, 24),
        end_date=date(2026, 5, 25),
        summary_mode=False,
        discovery_backend="rust",
    )

    assert payload["reparse_details"][0]["attempted_backend"] == "rust_office_oxide_v1"
    assert payload["reparse_details"][0]["fallback_backend"] == "python_office_v1"
    assert payload["reparse_details"][0]["fallback_reason"] == (
        "RUST_OFFICE_PARSE_FAILED: bad zip"
    )
    assert payload["reparse_details"][0]["rust_duration_ms"] == 5
    assert payload["reparse_details"][0]["fallback_duration_ms"] == 7
```

Extend `test_render_markdown_report_contains_stage_and_extension_metrics()` so its `reparse_details[0]` includes:

```python
                "attempted_backend": "rust_office_oxide_v1",
                "fallback_backend": "python_office_v1",
                "fallback_reason": "RUST_OFFICE_PARSE_FAILED: bad zip",
                "rust_duration_ms": 5,
                "fallback_duration_ms": 7,
```

Add this assertion:

```python
    assert "rust_office_oxide_v1" in markdown
    assert "python_office_v1" in markdown
    assert "RUST_OFFICE_PARSE_FAILED: bad zip" in markdown
```

- [ ] **Step 2: Run benchmark tests and verify they fail**

Run:

```bash
conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected: FAIL because `ReparseDetail` does not expose fallback fields yet.

- [ ] **Step 3: Extend `ReparseDetail`**

In `src/services/scan_metrics.py`, add fields to `ReparseDetail`:

```python
    attempted_backend: str = ""
    fallback_backend: str = ""
    fallback_reason: str = ""
    rust_duration_ms: int = 0
    fallback_duration_ms: int = 0
```

Add them to `to_dict()`:

```python
            "attempted_backend": self.attempted_backend,
            "fallback_backend": self.fallback_backend,
            "fallback_reason": self.fallback_reason,
            "rust_duration_ms": max(0, int(self.rust_duration_ms)),
            "fallback_duration_ms": max(0, int(self.fallback_duration_ms)),
```

- [ ] **Step 4: Render fallback fields in Markdown**

In `scripts/benchmark_scanner.py`, update the Reparse Details table header from:

```python
        "| extension | cache_miss_reason | parse_duration_ms | parse_status | path |",
        "|---|---|---:|---|---|",
```

to:

```python
        "| extension | cache_miss_reason | parse_duration_ms | parse_status | attempted_backend | fallback_backend | fallback_reason | rust_duration_ms | fallback_duration_ms | path |",
        "|---|---|---:|---|---|---|---|---:|---:|---|",
```

Update the row format in the reparse details loop:

```python
            lines.append(
                "| {extension} | {cache_miss_reason} | {parse_duration_ms} | "
                "{parse_status} | {attempted_backend} | {fallback_backend} | "
                "{fallback_reason} | {rust_duration_ms} | "
                "{fallback_duration_ms} | {path} |".format(
                    extension=item["extension"],
                    cache_miss_reason=item["cache_miss_reason"],
                    parse_duration_ms=item["parse_duration_ms"],
                    parse_status=item["parse_status"],
                    attempted_backend=item.get("attempted_backend", ""),
                    fallback_backend=item.get("fallback_backend", ""),
                    fallback_reason=str(item.get("fallback_reason", "")).replace("|", "/"),
                    rust_duration_ms=item.get("rust_duration_ms", 0),
                    fallback_duration_ms=item.get("fallback_duration_ms", 0),
                    path=item["path"],
                )
            )
```

- [ ] **Step 5: Run benchmark and scanner tests**

Run:

```bash
conda run -n test python -m pytest tests/test_benchmark_scanner.py tests/test_file_scanner.py -q
```

Expected: PASS, including the Task 4 scanner routing tests.

- [ ] **Step 6: Commit Task 4 and Task 5 together**

```bash
git add src/services/file_scanner.py src/services/scan_metrics.py scripts/benchmark_scanner.py tests/test_file_scanner.py tests/test_benchmark_scanner.py
git commit -m "Wire Rust Office parser into scanner metrics"
```

## Task 6: SharePoint-to-Text Validation and Optional Dependency Wiring

**Files:**
- Modify: `requirements.txt`
- Modify: `tests/test_office_parser.py`
- Modify: `src/services/office_parser.py`

- [ ] **Step 1: Validate package install in the test environment**

Run:

```bash
conda run -n test python -m pip install "sharepoint-to-text>=1.1,<2"
conda run -n test python -m pip check
conda run -n test python - <<'PY'
import sharepoint2text
print(sharepoint2text.__name__)
PY
```

Expected: all commands PASS and stdout includes `sharepoint2text`. If install or `pip check` fails, skip Step 3 and keep the graceful missing-dependency behavior from Task 3.

- [ ] **Step 2: Add dependency only after validation passes**

If Step 1 passed, append to `requirements.txt`:

```text
sharepoint-to-text>=1.1,<2
```

- [ ] **Step 3: Add a unit test with a fake sharepoint2text module object**

Add to `tests/test_office_parser.py`:

```python
def test_parse_with_sharepoint_text_extracts_and_truncates_content(tmp_path):
    sample = tmp_path / "legacy.ppt"
    sample.write_bytes(b"fake")

    class FakeExtraction:
        def get_full_text(self):
            return "甲乙丙丁"

    class FakeSharePoint2Text:
        @staticmethod
        def read_file(path):
            assert path == str(sample)
            yield FakeExtraction()

    context = parse_with_sharepoint_text(
        sample,
        ".ppt",
        {"document_excerpt_max_chars": 3},
        import_module=lambda name: FakeSharePoint2Text,
    )

    assert context.content == "甲乙丙"
    assert context.error is None
    assert context.parser_backend == "python_sharepoint_text_v1"
    assert context.truncated is True
```

- [ ] **Step 4: Run office parser tests**

Run:

```bash
conda run -n test python -m pytest tests/test_office_parser.py -q
```

Expected: PASS.

- [ ] **Step 5: Commit**

If Step 1 passed:

```bash
git add requirements.txt src/services/office_parser.py tests/test_office_parser.py
git commit -m "Add SharePoint text Office fallback"
```

If Step 1 failed:

```bash
git add src/services/office_parser.py tests/test_office_parser.py
git commit -m "Keep SharePoint text fallback optional"
```

## Task 7: Rust/Python Contract Tests for Real Fixtures

**Files:**
- Modify: `tests/test_document_parser.py`
- Test fixture generation inside pytest temp dirs

- [ ] **Step 1: Add a smoke test that runs the Rust CLI for generated OOXML fixtures**

Add to `tests/test_document_parser.py`:

```python
import json
import subprocess

RUST_OFFICE_PARSER_BIN = (
    Path(__file__).resolve().parents[1]
    / "rust/office_parser/target/release/ai-daily-office-parser"
)


def _run_rust_office_parser(sample: Path, file_type: str) -> dict:
    request = {
        "file_path": str(sample),
        "file_type": file_type,
        "limits": {"document_excerpt_max_chars": 4000},
        "parser_backend": "rust_office_oxide_v1",
    }
    completed = subprocess.run(
        [str(RUST_OFFICE_PARSER_BIN)],
        input=json.dumps(request, ensure_ascii=False),
        text=True,
        capture_output=True,
        timeout=20,
        check=False,
    )
    assert completed.returncode == 0, completed.stderr
    return json.loads(completed.stdout)


@pytest.mark.skipif(
    not RUST_OFFICE_PARSER_BIN.exists(),
    reason="Rust Office parser release binary has not been built",
)
def test_rust_office_parser_extracts_generated_docx_xlsx_and_pptx(tmp_path: Path):
    docx_sample = tmp_path / "report.docx"
    doc = Document()
    doc.add_paragraph("Rust DOCX 中文")
    doc.save(docx_sample)

    xlsx_sample = tmp_path / "workbook.xlsx"
    workbook = Workbook()
    sheet = workbook.active
    sheet.append(["项目", "状态"])
    sheet.append(["Rust XLSX 中文", "完成"])
    workbook.save(xlsx_sample)

    pptx_sample = tmp_path / "deck.pptx"
    presentation = Presentation()
    slide = presentation.slides.add_slide(presentation.slide_layouts[1])
    slide.shapes.title.text = "Rust PPTX 中文"
    slide.placeholders[1].text = "完成 parser spike"
    presentation.save(pptx_sample)

    docx_context = _run_rust_office_parser(docx_sample, ".docx")
    xlsx_context = _run_rust_office_parser(xlsx_sample, ".xlsx")
    pptx_context = _run_rust_office_parser(pptx_sample, ".pptx")

    assert docx_context["error"] is None
    assert "Rust DOCX 中文" in docx_context["content"]
    assert xlsx_context["error"] is None
    assert "Rust XLSX 中文" in xlsx_context["content"]
    assert pptx_context["error"] is None
    assert "Rust PPTX 中文" in pptx_context["content"]
```

Add `import pytest` to `tests/test_document_parser.py` if it is not already imported.

- [ ] **Step 2: Build Rust CLI and run the contract test**

Run:

```bash
cd rust/office_parser && cargo build --release
conda run -n test python -m pytest tests/test_document_parser.py::test_rust_office_parser_extracts_generated_docx_xlsx_and_pptx -q
```

Expected: PASS when the release binary exists; SKIP only if the binary has not been built.

- [ ] **Step 3: Commit**

```bash
git add tests/test_document_parser.py
git commit -m "Add Rust Office parser contract test"
```

## Task 8: README and Benchmark Instructions

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add Rust Office parser docs**

Add after the Rust discovery section in `README.md`:

````markdown
### Rust Office parser backend

Office 文件默认优先使用 Rust `office_oxide` backend，并在 Rust 失败时回退到 Python backend：

```yaml
scanner:
  office_parser_backend: "rust_office_oxide_v1"
  rust_office_parser_bin: "rust/office_parser/target/release/ai-daily-office-parser"
  office_parser_fallback_enabled: true
  office_parser_fallback_order:
    - "python_office_v1"
    - "python_sharepoint_text_v1"
  office_fallback_after_timeout: false
  office_external_fallback: "disabled"
  office_legacy_extensions_enabled: false
```

构建 Rust Office parser：

```bash
cd rust/office_parser
cargo test
cargo build --release
```

默认扫描范围不自动加入 `.doc/.ppt`。如果需要 legacy Office 文件，先确认本机样本和 fallback 行为，再显式把扩展名加入 `scanner.allowed_extensions`。

benchmark 报告中的 `parser_backend`、`attempted_backend`、`fallback_backend` 和 `fallback_reason` 字段用于确认 Rust 是否成功，或是否回退到了 Python。
````

- [ ] **Step 2: Run README grep checks**

Run:

```bash
rg -n "rust_office_oxide_v1|rust/office_parser|fallback_backend|office_legacy_extensions_enabled" README.md
```

Expected: all four terms are found.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "Document Rust Office parser backend"
```

## Task 9: End-to-End Verification and Benchmarks

**Files:**
- No required source edits.
- Outputs: `/tmp/scanner-office-rust-no-cache.json`, `/tmp/scanner-office-rust-no-cache.md`

- [ ] **Step 1: Run focused tests**

Run:

```bash
cd rust/office_parser && cargo test
cd rust/office_parser && cargo build --release
conda run -n test python -m pytest \
  tests/test_office_parser.py \
  tests/test_document_parser.py \
  tests/test_file_scanner.py \
  tests/test_benchmark_scanner.py \
  tests/test_config.py \
  tests/test_scan_planner.py -q
```

Expected: PASS.

- [ ] **Step 2: Run full Python test suite**

Run:

```bash
conda run -n test python -m pytest tests -q
```

Expected: PASS.

- [ ] **Step 3: Run compile check**

Run:

```bash
conda run -n test python -m compileall main.py src tests
```

Expected: PASS.

- [ ] **Step 4: Run whitespace check**

Run:

```bash
git diff --check
```

Expected: no output.

- [ ] **Step 5: Run no-cache benchmark against an isolated DB**

Run:

```bash
DAILY_REPORT_SCANNER__INDEX_DB_PATH=/tmp/ai-daily-report-scan-index-office-rust.sqlite3 \
conda run -n test python scripts/benchmark_scanner.py \
  --start-date 2026-05-24 \
  --end-date 2026-05-25 \
  --json-out /tmp/scanner-office-rust-no-cache.json \
  --markdown-out /tmp/scanner-office-rust-no-cache.md
```

Expected:

- JSON exists at `/tmp/scanner-office-rust-no-cache.json`.
- Markdown exists at `/tmp/scanner-office-rust-no-cache.md`.
- Office rows show `attempted_backend = rust_office_oxide_v1`.
- Successful Rust rows show `parser_backend = rust_office_oxide_v1`.
- Fallback rows show `fallback_backend` and `fallback_reason`.
- `.pdf` rows still show `pdf_text_v1`.

- [ ] **Step 6: Inspect benchmark evidence**

Run:

```bash
jq '.parser_backend_summary, .extension_metrics, [.reparse_details[] | select(.extension == ".xlsx" or .extension == ".docx" or .extension == ".pptx" or .extension == ".xls" or .extension == ".doc" or .extension == ".ppt") | {path, extension, parser_backend, attempted_backend, fallback_backend, fallback_reason, parse_duration_ms, rust_duration_ms, fallback_duration_ms}]' /tmp/scanner-office-rust-no-cache.json
```

Expected: output clearly separates Rust successes from Python fallback rows.

- [ ] **Step 7: Commit any final test-only corrections**

If previous steps required test expectation corrections:

```bash
git add rust/office_parser src tests scripts README.md requirements.txt config/settings.example.yaml
git commit -m "Stabilize Rust Office parser verification"
```

If no corrections were needed, do not create an empty commit.

## Self-Review

- Spec coverage: Rust primary path, Python fallback, DOC/PPT dependency research, timeout policy, cache isolation, benchmark fallback fields, default legacy scan-range behavior, and verification commands are all mapped to tasks.
- Placeholder scan: The plan contains no unresolved placeholder markers or generic "add tests" instructions.
- Type consistency: `OfficeParseAudit`, `OfficeParseOutcome`, `RustOfficeParserRunner`, `parse_office_with_fallback`, and new `ReparseDetail` fields are introduced before downstream tasks reference them.
