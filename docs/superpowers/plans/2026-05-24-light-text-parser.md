# Light Text Parser Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a bounded Python light parser for `.md`, `.txt`, `.json`, `.log`, and `.csv` so large text-like files avoid Windows subprocess parse overhead.

**Architecture:** Keep `FileScanner` as the router and move text-like parsing into a new `src/services/light_text_parser.py` module. Add optional metadata to `FileContext` and `ReparseDetail` so benchmark output can prove which parser backend ran and whether content was truncated, while preserving existing content/error behavior for daily and weekly report consumers.

**Tech Stack:** Python 3.10+, pytest, Pydantic, standard-library `json` / `csv`, existing scanner cache, `ScanPlanner`, `ScanMetricsCollector`, and `scripts/benchmark_scanner.py`.

---

## Scope Check

This plan implements only the approved方案 A:

- Cover `.md`, `.txt`, `.json`, `.log`, `.csv`.
- Do not add Rust.
- Do not integrate MarkItDown.
- Do not change PDF / Office / image / audio parsing.
- Do not change discovery pruning; directory pruning remains a separate optimization.

## File Structure

- Modify `src/models/schemas.py`
  - Add optional `parser_backend` and `truncated` fields to `FileContext`.
  - Keep existing required fields unchanged: `file_path`, `file_type`, `content`, `error`.
- Modify `src/services/scan_metrics.py`
  - Add `parser_backend` and `truncated` to `ReparseDetail`.
  - Keep existing fields and defaults so old tests can construct `ReparseDetail` with minimal arguments.
- Modify `tests/test_scan_metrics.py`
  - Assert the new fields serialize into benchmark payloads.
- Modify `src/core/config.py`
  - Pass through light parser config keys when present.
  - Keep legacy `direct_text_max_bytes` for compatibility.
- Modify `tests/test_config.py`
  - Cover pass-through for `direct_text_read_bytes`, `log_tail_read_bytes`, and `text_excerpt_max_chars`.
- Modify `src/services/scan_planner.py`
  - Include light parser backend version and effective read budgets in parser profile.
  - Preserve stable JSON serialization.
- Modify `tests/test_scan_planner.py`
  - Cover parser profile defaults and legacy `direct_text_max_bytes` fallback.
- Create `src/services/light_text_parser.py`
  - Implement bounded head/tail reads and format-specific previews.
- Create `tests/test_light_text_parser.py`
  - Cover Markdown/text head excerpts, log tail excerpts, JSON preview/fallback, CSV preview, and decode failures.
- Modify `src/services/file_scanner.py`
  - Route text-like files to `parse_text_like_file()` when `worker_lane_mode="direct"`.
  - Keep non-text-like files on `_extract_content_with_timeout()`.
  - Record parser backend and truncation in reparse details.
- Modify `tests/test_file_scanner.py`
  - Update the old large text fallback test to expect light parser usage.
  - Keep the PDF subprocess fallback test.
  - Add reparse detail assertions for backend and truncation.
- Modify `scripts/benchmark_scanner.py`
  - Add parser backend summary to JSON and Markdown output.
- Modify `tests/test_benchmark_scanner.py`
  - Cover backend summary and new reparse detail fields.

## Task 1: Add Parser Metadata To Models And Metrics

**Files:**
- Modify: `src/models/schemas.py`
- Modify: `src/services/scan_metrics.py`
- Modify: `tests/test_scan_metrics.py`

- [ ] **Step 1: Update failing metric serialization test**

In `tests/test_scan_metrics.py`, update `test_reparse_detail_serializes_stable_payload()` so the constructed detail includes backend metadata:

```python
detail = ReparseDetail(
    path="D:\\work\\report.md",
    extension=".md",
    file_identity="bootstrap:d:\\work\\report.md",
    source_version="mtime=2:size=10",
    cache_status="miss",
    cache_miss_reason="source_version_changed",
    previous_source_version="mtime=1:size=10",
    parse_duration_ms=12,
    parse_status="success",
    parse_error="",
    parser_backend="light_text_v1",
    truncated=True,
)
```

Update the expected dict in the same test:

```python
assert detail.to_dict() == {
    "path": "D:\\work\\report.md",
    "extension": ".md",
    "file_identity": "bootstrap:d:\\work\\report.md",
    "source_version": "mtime=2:size=10",
    "cache_status": "miss",
    "cache_miss_reason": "source_version_changed",
    "previous_source_version": "mtime=1:size=10",
    "parse_duration_ms": 12,
    "parse_status": "success",
    "parse_error": "",
    "parser_backend": "light_text_v1",
    "truncated": True,
}
```

Also add these assertions to `test_reparse_detail_normalizes_duration_and_none_version()`:

```python
assert payload["parser_backend"] == ""
assert payload["truncated"] is False
```

- [ ] **Step 2: Run metric tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_metrics.py::test_reparse_detail_serializes_stable_payload tests/test_scan_metrics.py::test_reparse_detail_normalizes_duration_and_none_version -q
```

Expected before implementation: failure because `ReparseDetail.__init__()` does not accept `parser_backend` and `truncated`, or `to_dict()` does not include those keys.

- [ ] **Step 3: Add optional parser fields to `FileContext`**

In `src/models/schemas.py`, replace the current `FileContext` class with:

```python
class FileContext(BaseModel):
    file_path: str = Field(description="文件路径")
    file_type: str = Field(description="文件类型")
    content: str = Field(description="抽取文本")
    error: Optional[str] = Field(default=None, description="发现的问题摘要")
    parser_backend: Optional[str] = Field(
        default=None,
        description="解析后端标识，用于 scanner benchmark 和审计",
    )
    truncated: bool = Field(default=False, description="抽取内容是否被读取预算截断")
```

- [ ] **Step 4: Add backend fields to `ReparseDetail`**

In `src/services/scan_metrics.py`, update the `ReparseDetail` dataclass:

```python
@dataclass(slots=True)
class ReparseDetail:
    """单个重解析文件的 cache miss 与解析结果明细。"""

    path: str
    extension: str
    file_identity: str
    source_version: str
    cache_status: str
    cache_miss_reason: str
    previous_source_version: str | None = None
    parse_duration_ms: int = 0
    parse_status: str = "success"
    parse_error: str = ""
    parser_backend: str = ""
    truncated: bool = False

    def to_dict(self) -> dict[str, int | str | bool | None]:
        """转成 benchmark JSON / Markdown 共用的稳定结构。"""
        return {
            "path": self.path,
            "extension": self.extension,
            "file_identity": self.file_identity,
            "source_version": self.source_version,
            "cache_status": self.cache_status,
            "cache_miss_reason": self.cache_miss_reason,
            "previous_source_version": self.previous_source_version,
            "parse_duration_ms": max(0, int(self.parse_duration_ms)),
            "parse_status": self.parse_status,
            "parse_error": self.parse_error,
            "parser_backend": self.parser_backend,
            "truncated": bool(self.truncated),
        }
```

- [ ] **Step 5: Run metric tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_metrics.py -q
```

Expected: all `tests/test_scan_metrics.py` tests pass.

- [ ] **Step 6: Commit Task 1**

Run:

```powershell
git add src/models/schemas.py src/services/scan_metrics.py tests/test_scan_metrics.py
git commit -m "feat: track scanner parser backend metadata"
```

## Task 2: Pass Light Parser Config Into Scanner And Parser Profile

**Files:**
- Modify: `src/core/config.py`
- Modify: `tests/test_config.py`
- Modify: `src/services/scan_planner.py`
- Modify: `tests/test_scan_planner.py`

- [ ] **Step 1: Write failing config pass-through test**

Append this test to `tests/test_config.py`:

```python
def test_scanner_config_passes_light_text_parser_options_when_present():
    """轻量文本解析预算应从 settings 透传到 scanner 配置。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".md"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
            direct_text_read_bytes=131072,
            log_tail_read_bytes=65536,
            text_excerpt_max_chars=3000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["direct_text_read_bytes"] == 131072
    assert scanner_config["log_tail_read_bytes"] == 65536
    assert scanner_config["text_excerpt_max_chars"] == 3000
```

- [ ] **Step 2: Run config test and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_config.py::test_scanner_config_passes_light_text_parser_options_when_present -q
```

Expected before implementation: failure with `KeyError: 'direct_text_read_bytes'`.

- [ ] **Step 3: Implement config pass-through**

In `src/core/config.py`, extend the optional scanner keys tuple:

```python
for key in (
    "summary_excel_max_rows",
    "summary_pdf_max_pages",
    "summary_text_max_chars",
    "total_max_chars",
    "max_file_size_mb",
    "file_timeout_seconds",
    "file_timeout_by_extension",
    "direct_text_max_bytes",
    "direct_text_read_bytes",
    "log_tail_read_bytes",
    "text_excerpt_max_chars",
):
    if hasattr(scanner, key):
        cfg[key] = self._to_builtin_value(getattr(scanner, key))
```

- [ ] **Step 4: Run config tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_config.py -q
```

Expected: all config tests pass.

- [ ] **Step 5: Write failing parser profile tests**

Append these tests to `tests/test_scan_planner.py`:

```python
def test_build_parser_profile_includes_light_text_parser_defaults():
    """parser profile 必须包含轻量文本解析参数，避免 cache 错误复用。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "total_max_chars": 50000,
            "parser_profile_version": "v8",
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["text_parser_backend"] == "light_text_v1"
    assert profile["direct_text_read_bytes"] == 262144
    assert profile["log_tail_read_bytes"] == 262144
    assert profile["text_excerpt_max_chars"] == 6000


def test_build_parser_profile_uses_legacy_direct_text_max_bytes_as_read_budget():
    """旧配置只设置 direct_text_max_bytes 时，应作为读取预算兼容。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "direct_text_max_bytes": 8192,
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["direct_text_read_bytes"] == 8192
```

Update the expected dict in `test_build_parser_profile_uses_summary_limits_when_requested()` by adding:

```python
"text_parser_backend": "light_text_v1",
"direct_text_read_bytes": 262144,
"log_tail_read_bytes": 262144,
"text_excerpt_max_chars": 2000,
```

- [ ] **Step 6: Run planner tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_planner.py -q
```

Expected before implementation: profile assertions fail because the new parser profile fields are absent.

- [ ] **Step 7: Implement parser profile fields**

In `src/services/scan_planner.py`, add constants near the imports:

```python
LIGHT_TEXT_PARSER_BACKEND = "light_text_v1"
DEFAULT_DIRECT_TEXT_READ_BYTES = 256 * 1024
DEFAULT_LOG_TAIL_READ_BYTES = 256 * 1024
```

Add this helper inside `ScanPlanner`:

```python
    def _positive_int_config(self, key: str, default: int) -> int:
        """读取正整数配置；非法值回退默认值，避免 cache key 写入脏值。"""
        raw_value = self.scanner_cfg.get(key, default)
        try:
            value = int(raw_value)
        except (TypeError, ValueError):
            return default
        return value if value > 0 else default
```

Then, before `return profile` in `build_parser_profile()`, add:

```python
        direct_default = self._positive_int_config(
            "direct_text_max_bytes",
            DEFAULT_DIRECT_TEXT_READ_BYTES,
        )
        direct_text_read_bytes = self._positive_int_config(
            "direct_text_read_bytes",
            direct_default,
        )
        log_tail_read_bytes = self._positive_int_config(
            "log_tail_read_bytes",
            DEFAULT_LOG_TAIL_READ_BYTES,
        )
        text_excerpt_max_chars = self._positive_int_config(
            "text_excerpt_max_chars",
            int(profile["text_max_chars"]),
        )
        profile["text_parser_backend"] = LIGHT_TEXT_PARSER_BACKEND
        profile["direct_text_read_bytes"] = direct_text_read_bytes
        profile["log_tail_read_bytes"] = log_tail_read_bytes
        profile["text_excerpt_max_chars"] = text_excerpt_max_chars
```

- [ ] **Step 8: Run planner tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_planner.py -q
```

Expected: all planner tests pass.

- [ ] **Step 9: Commit Task 2**

Run:

```powershell
git add src/core/config.py tests/test_config.py src/services/scan_planner.py tests/test_scan_planner.py
git commit -m "feat: include light text parser profile"
```

## Task 3: Implement The Light Text Parser Module

**Files:**
- Create: `src/services/light_text_parser.py`
- Create: `tests/test_light_text_parser.py`

- [ ] **Step 1: Write failing light parser tests**

Create `tests/test_light_text_parser.py`:

```python
"""测试轻量 text-like 解析器。"""

from pathlib import Path

from src.services.light_text_parser import (
    LIGHT_TEXT_PARSER_BACKEND,
    LightTextParserOptions,
    parse_text_like_file,
)


def _options(**overrides) -> LightTextParserOptions:
    values = {
        "read_head_bytes": 64,
        "read_tail_bytes": 64,
        "max_output_chars": 200,
        "encoding": "utf-8",
        "parser_backend_version": LIGHT_TEXT_PARSER_BACKEND,
    }
    values.update(overrides)
    return LightTextParserOptions(**values)


def test_parse_markdown_reads_bounded_head_and_marks_truncated(tmp_path: Path):
    sample = tmp_path / "large.md"
    sample.write_text("# Title\n\nfirst paragraph\n\nsecond paragraph", encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".md",
        {"text_max_chars": 200},
        _options(read_head_bytes=18),
    )

    assert context.error is None
    assert context.parser_backend == "light_text_v1"
    assert context.truncated is True
    assert "truncated: true" in context.content
    assert "# Title" in context.content
    assert "second" not in context.content


def test_parse_log_reads_tail_excerpt(tmp_path: Path):
    sample = tmp_path / "app.log"
    sample.write_text("old line\nmiddle line\nlatest line", encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".log",
        {"text_max_chars": 200},
        _options(read_tail_bytes=16),
    )

    assert context.error is None
    assert context.truncated is True
    assert "excerpt_source: tail" in context.content
    assert "latest line" in context.content
    assert "old line" not in context.content


def test_parse_json_outputs_top_level_keys(tmp_path: Path):
    sample = tmp_path / "payload.json"
    sample.write_text('{"name": "demo", "items": [1, 2]}', encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".json",
        {"text_max_chars": 200},
        _options(read_head_bytes=256),
    )

    assert context.error is None
    assert "JSON object preview" in context.content
    assert "top_level_keys: items, name" in context.content


def test_parse_truncated_json_falls_back_to_text_excerpt(tmp_path: Path):
    sample = tmp_path / "payload.json"
    sample.write_text('{"name": "demo", "items": [1, 2]}', encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".json",
        {"text_max_chars": 200},
        _options(read_head_bytes=10),
    )

    assert context.error is None
    assert context.truncated is True
    assert "warning: JSON_PREVIEW_FALLBACK" in context.content
    assert '{"name"' in context.content


def test_parse_csv_outputs_header_and_preview_rows(tmp_path: Path):
    sample = tmp_path / "table.csv"
    sample.write_text("name,amount\nalpha,10\nbeta,20\n", encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".csv",
        {"text_max_chars": 200},
        _options(read_head_bytes=256),
    )

    assert context.error is None
    assert "CSV preview" in context.content
    assert "header: name | amount" in context.content
    assert "row 1: alpha | 10" in context.content


def test_parse_decode_failure_returns_auditable_error(tmp_path: Path):
    sample = tmp_path / "bad.txt"
    sample.write_bytes(b"\xff\xfe\xfa")

    context = parse_text_like_file(
        sample,
        ".txt",
        {"text_max_chars": 200},
        _options(read_head_bytes=256),
    )

    assert context.content == ""
    assert context.parser_backend == "light_text_v1"
    assert context.error is not None
    assert context.error.startswith("TEXT_DECODE_FAILED:")
```

- [ ] **Step 2: Run light parser tests and verify import failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_light_text_parser.py -q
```

Expected before implementation: import failure because `src.services.light_text_parser` does not exist.

- [ ] **Step 3: Implement `src/services/light_text_parser.py`**

Create `src/services/light_text_parser.py`:

```python
"""轻量 text-like 文件解析器。"""

from __future__ import annotations

import csv
import io
import json
from dataclasses import dataclass
from itertools import islice
from pathlib import Path
from typing import Any

from ..models.schemas import FileContext
from ..utils.text_tools import truncate_text

LIGHT_TEXT_PARSER_BACKEND = "light_text_v1"
DEFAULT_DIRECT_TEXT_READ_BYTES = 256 * 1024
DEFAULT_LOG_TAIL_READ_BYTES = 256 * 1024


@dataclass(frozen=True, slots=True)
class LightTextParserOptions:
    """轻量解析器运行参数。"""

    read_head_bytes: int = DEFAULT_DIRECT_TEXT_READ_BYTES
    read_tail_bytes: int = DEFAULT_LOG_TAIL_READ_BYTES
    max_output_chars: int = 6000
    encoding: str = "utf-8"
    parser_backend_version: str = LIGHT_TEXT_PARSER_BACKEND


def parse_text_like_file(
    file_path: Path,
    file_type: str,
    limits: dict[str, Any],
    options: LightTextParserOptions,
) -> FileContext:
    """按读取预算解析 text-like 文件，避免为大文本启动 subprocess。"""
    normalized_type = file_type.lower()
    try:
        if normalized_type == ".log":
            raw_text, truncated = _read_tail_text(
                file_path,
                max_bytes=options.read_tail_bytes,
                encoding=options.encoding,
            )
            excerpt_source = "tail"
        else:
            raw_text, truncated = _read_head_text(
                file_path,
                max_bytes=options.read_head_bytes,
                encoding=options.encoding,
            )
            excerpt_source = "head"

        max_chars = _effective_max_chars(limits, options)
        excerpt = truncate_text(_normalize_newlines(raw_text), max_chars)

        if normalized_type == ".json":
            content = _format_json_preview_or_fallback(
                excerpt,
                truncated=truncated,
                excerpt_source=excerpt_source,
                options=options,
            )
        elif normalized_type == ".csv":
            content = _format_csv_preview_or_fallback(
                excerpt,
                truncated=truncated,
                excerpt_source=excerpt_source,
                options=options,
            )
        else:
            content = _format_plain_excerpt(
                excerpt,
                truncated=truncated,
                excerpt_source=excerpt_source,
                options=options,
            )

        return FileContext(
            file_path=str(file_path),
            file_type=normalized_type,
            content=content,
            error=None,
            parser_backend=options.parser_backend_version,
            truncated=truncated,
        )
    except UnicodeDecodeError as exc:
        return _error_context(
            file_path,
            normalized_type,
            options,
            "TEXT_DECODE_FAILED",
            str(exc),
            truncated=False,
        )
    except OSError as exc:
        return _error_context(
            file_path,
            normalized_type,
            options,
            "FILE_READ_FAILED",
            str(exc),
            truncated=False,
        )
    except Exception as exc:
        return _error_context(
            file_path,
            normalized_type,
            options,
            "LIGHT_TEXT_PARSE_FAILED",
            str(exc),
            truncated=False,
        )


def _read_head_text(
    file_path: Path,
    *,
    max_bytes: int,
    encoding: str,
) -> tuple[str, bool]:
    with open(file_path, "rb") as file:
        raw = file.read(max_bytes + 1)
    truncated = len(raw) > max_bytes or file_path.stat().st_size > max_bytes
    return _decode_bounded(raw[:max_bytes], encoding, truncated=truncated), truncated


def _read_tail_text(
    file_path: Path,
    *,
    max_bytes: int,
    encoding: str,
) -> tuple[str, bool]:
    file_size = file_path.stat().st_size
    read_bytes = min(file_size, max_bytes)
    with open(file_path, "rb") as file:
        file.seek(max(0, file_size - read_bytes))
        raw = file.read(read_bytes)
    truncated = file_size > max_bytes
    return _decode_bounded(raw, encoding, truncated=truncated), truncated


def _decode_bounded(raw: bytes, encoding: str, *, truncated: bool) -> str:
    """解码预算内字节；只修剪由截断造成的 UTF-8 边界残片。"""
    try:
        return raw.decode(encoding)
    except UnicodeDecodeError as exc:
        if not truncated:
            raise

        # head 读取可能截在多字节字符尾部；修掉尾部残片即可。
        if exc.end == len(raw) and exc.start > 0:
            return raw[: exc.start].decode(encoding)

        # tail 读取可能从多字节字符中间开始；最多跳过 UTF-8 单字符宽度。
        if exc.start == 0:
            for offset in range(1, min(4, len(raw)) + 1):
                try:
                    return raw[offset:].decode(encoding)
                except UnicodeDecodeError:
                    continue
        raise


def _effective_max_chars(
    limits: dict[str, Any],
    options: LightTextParserOptions,
) -> int:
    raw_value = limits.get("text_max_chars", options.max_output_chars)
    try:
        limit_value = int(raw_value)
    except (TypeError, ValueError):
        return options.max_output_chars
    if limit_value <= 0:
        return options.max_output_chars
    return min(limit_value, options.max_output_chars)


def _normalize_newlines(text: str) -> str:
    return text.replace("\r\n", "\n").replace("\r", "\n")


def _format_plain_excerpt(
    text: str,
    *,
    truncated: bool,
    excerpt_source: str,
    options: LightTextParserOptions,
    warning: str | None = None,
) -> str:
    header = [
        f"parser_backend: {options.parser_backend_version}",
        f"excerpt_source: {excerpt_source}",
        f"truncated: {str(truncated).lower()}",
    ]
    if warning:
        header.append(f"warning: {warning}")
    return "\n".join(header) + "\n\n" + text


def _format_json_preview_or_fallback(
    text: str,
    *,
    truncated: bool,
    excerpt_source: str,
    options: LightTextParserOptions,
) -> str:
    if truncated:
        return _format_plain_excerpt(
            text,
            truncated=truncated,
            excerpt_source=excerpt_source,
            options=options,
            warning="JSON_PREVIEW_FALLBACK",
        )

    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return _format_plain_excerpt(
            text,
            truncated=truncated,
            excerpt_source=excerpt_source,
            options=options,
            warning="JSON_PREVIEW_FALLBACK",
        )

    header = [
        f"parser_backend: {options.parser_backend_version}",
        f"excerpt_source: {excerpt_source}",
        "truncated: false",
    ]
    if isinstance(payload, dict):
        keys = ", ".join(sorted(str(key) for key in payload.keys()))
        return "\n".join(header + ["", "JSON object preview", f"top_level_keys: {keys}"])
    if isinstance(payload, list):
        return "\n".join(
            header
            + [
                "",
                "JSON list preview",
                f"item_count: {len(payload)}",
                f"first_item_type: {type(payload[0]).__name__ if payload else 'empty'}",
            ]
        )
    return "\n".join(header + ["", f"JSON scalar preview: {type(payload).__name__}"])


def _format_csv_preview_or_fallback(
    text: str,
    *,
    truncated: bool,
    excerpt_source: str,
    options: LightTextParserOptions,
) -> str:
    try:
        rows = list(islice(csv.reader(io.StringIO(text)), 6))
    except csv.Error:
        return _format_plain_excerpt(
            text,
            truncated=truncated,
            excerpt_source=excerpt_source,
            options=options,
            warning="CSV_PREVIEW_FALLBACK",
        )

    header = [
        f"parser_backend: {options.parser_backend_version}",
        f"excerpt_source: {excerpt_source}",
        f"truncated: {str(truncated).lower()}",
        "",
        "CSV preview",
    ]
    if not rows:
        return "\n".join(header + ["empty: true"])

    lines = header + [f"header: {' | '.join(rows[0])}"]
    for index, row in enumerate(rows[1:], start=1):
        lines.append(f"row {index}: {' | '.join(row)}")
    return "\n".join(lines)


def _error_context(
    file_path: Path,
    file_type: str,
    options: LightTextParserOptions,
    error_code: str,
    message: str,
    *,
    truncated: bool,
) -> FileContext:
    return FileContext(
        file_path=str(file_path),
        file_type=file_type,
        content="",
        error=f"{error_code}: {message}",
        parser_backend=options.parser_backend_version,
        truncated=truncated,
    )
```

- [ ] **Step 4: Run light parser tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_light_text_parser.py -q
```

Expected: all light parser tests pass.

- [ ] **Step 5: Commit Task 3**

Run:

```powershell
git add src/services/light_text_parser.py tests/test_light_text_parser.py
git commit -m "feat: add bounded light text parser"
```

## Task 4: Route Text-Like Files Through The Light Parser

**Files:**
- Modify: `src/services/file_scanner.py`
- Modify: `tests/test_file_scanner.py`

- [ ] **Step 1: Update file scanner helper defaults**

In `tests/test_file_scanner.py`, update `_make_scanner()` default `scanner_cfg` with these keys after `"worker_lane_mode"` or near parser profile settings:

```python
"direct_text_read_bytes": 262144,
"log_tail_read_bytes": 262144,
"text_excerpt_max_chars": 6000,
```

If `"worker_lane_mode"` is absent from the helper, add:

```python
"worker_lane_mode": "direct",
```

- [ ] **Step 2: Replace the old large text subprocess test**

Replace `test_direct_text_lane_falls_back_to_subprocess_for_large_text_file()` with:

```python
def test_direct_text_lane_uses_light_parser_for_large_text_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """大 text-like 文件也应走 light parser，只按读取预算截断。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {
            "allowed_extensions": [".md"],
            "worker_lane_mode": "direct",
            "direct_text_read_bytes": 8,
            "text_excerpt_max_chars": 100,
        },
    )
    sample = scanner.work_dir / "large.md"
    sample.write_text("large direct content", encoding="utf-8")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=20")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )

    def fail_subprocess(file_path: Path, limits: dict):
        raise AssertionError("large text-like file should not use subprocess")

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", fail_subprocess)

    result = scanner.scan_files(date.today(), date.today())

    assert result.success_count == 1
    assert result.contexts[0].parser_backend == "light_text_v1"
    assert result.contexts[0].truncated is True
    assert "large di" in result.contexts[0].content
    assert scanner.last_reparse_details[0].parser_backend == "light_text_v1"
    assert scanner.last_reparse_details[0].truncated is True
```

- [ ] **Step 3: Add direct routing test with monkeypatched parser**

Append this test to `tests/test_file_scanner.py`:

```python
def test_scan_files_passes_light_parser_options(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """FileScanner 应把配置解析成 LightTextParserOptions 后传给 light parser。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {
            "allowed_extensions": [".log"],
            "worker_lane_mode": "direct",
            "direct_text_read_bytes": 123,
            "log_tail_read_bytes": 45,
            "text_excerpt_max_chars": 67,
        },
    )
    sample = scanner.work_dir / "app.log"
    sample.write_text("old\nnew", encoding="utf-8")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=7")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )
    captured = {}

    def fake_parse(file_path, file_type, limits, options):
        captured["file_path"] = file_path
        captured["file_type"] = file_type
        captured["limits"] = limits
        captured["options"] = options
        return file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="light",
            error=None,
            parser_backend=options.parser_backend_version,
            truncated=False,
        )

    monkeypatch.setattr(file_scanner_module, "parse_text_like_file", fake_parse)

    result = scanner.scan_files(date.today(), date.today())

    assert result.success_count == 1
    assert captured["file_path"] == sample
    assert captured["file_type"] == ".log"
    assert captured["limits"]["text_max_chars"] == 6000
    assert captured["options"].read_head_bytes == 123
    assert captured["options"].read_tail_bytes == 45
    assert captured["options"].max_output_chars == 67
```

- [ ] **Step 4: Run targeted file scanner tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py::test_direct_text_lane_uses_light_parser_for_large_text_file tests/test_file_scanner.py::test_scan_files_passes_light_parser_options tests/test_file_scanner.py::test_scan_files_keeps_subprocess_path_for_pdf_in_direct_mode -q
```

Expected before implementation: failure because `FileScanner` still uses the old size-gated direct lane or does not expose light parser options.

- [ ] **Step 5: Import light parser in `file_scanner.py`**

In `src/services/file_scanner.py`, add this import near the other service imports:

```python
from .light_text_parser import (
    DEFAULT_DIRECT_TEXT_READ_BYTES,
    DEFAULT_LOG_TAIL_READ_BYTES,
    LIGHT_TEXT_PARSER_BACKEND,
    LightTextParserOptions,
    parse_text_like_file,
)
```

- [ ] **Step 6: Replace text-like routing logic**

In `src/services/file_scanner.py`, replace `_extract_uncached_content()`, `_should_parse_direct()`, and `_direct_text_max_bytes()` with:

```python
    def _extract_uncached_content(
        self,
        file_path: Path,
        file_type: str,
        limits: Optional[dict] = None,
    ) -> FileContext:
        """根据文件类型选择 light text parser 或 subprocess timeout lane。"""
        effective_limits = limits or {}
        if self._should_parse_direct(file_type):
            return parse_text_like_file(
                file_path=file_path,
                file_type=file_type,
                limits=effective_limits,
                options=self._build_light_text_options(effective_limits),
            )
        return self._extract_content_with_timeout(file_path, effective_limits)

    def _should_parse_direct(self, file_type: str) -> bool:
        """text-like 文件使用 bounded direct parser，避免 Windows spawn 固定开销。"""
        if str(self.scanner_cfg.get("worker_lane_mode", "direct")).lower() != "direct":
            return False
        return file_type.lower() in TEXT_FILE_TYPES

    def _build_light_text_options(self, limits: dict) -> LightTextParserOptions:
        """从 scanner 配置构造轻量文本解析参数。"""
        direct_default = self._positive_int_config(
            "direct_text_max_bytes",
            DEFAULT_DIRECT_TEXT_READ_BYTES,
        )
        read_head_bytes = self._positive_int_config(
            "direct_text_read_bytes",
            direct_default,
        )
        read_tail_bytes = self._positive_int_config(
            "log_tail_read_bytes",
            DEFAULT_LOG_TAIL_READ_BYTES,
        )
        max_output_chars = self._positive_int_config(
            "text_excerpt_max_chars",
            int(limits.get("text_max_chars", self.scanner_cfg["text_max_chars"])),
        )
        return LightTextParserOptions(
            read_head_bytes=read_head_bytes,
            read_tail_bytes=read_tail_bytes,
            max_output_chars=max_output_chars,
            encoding="utf-8",
            parser_backend_version=LIGHT_TEXT_PARSER_BACKEND,
        )

    def _positive_int_config(self, key: str, default: int) -> int:
        """读取正整数配置；非法配置回退默认值并保留扫描连续性。"""
        raw_value = self.scanner_cfg.get(key, default)
        try:
            value = int(raw_value)
        except (TypeError, ValueError):
            logger.warning("%s 配置无效，使用默认值 %s: %r", key, default, raw_value)
            return default
        if value <= 0:
            logger.warning("%s 配置无效，使用默认值 %s: %r", key, default, raw_value)
            return default
        return value
```

- [ ] **Step 7: Record backend metadata in reparse details**

In `_record_reparse_detail()`, add the new fields to `ReparseDetail(...)`:

```python
                parser_backend=context.parser_backend or "subprocess",
                truncated=context.truncated,
```

In `_record_reparse_exception()`, add:

```python
                parser_backend="",
                truncated=False,
```

- [ ] **Step 8: Run targeted file scanner tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py::test_direct_text_lane_uses_light_parser_for_large_text_file tests/test_file_scanner.py::test_scan_files_passes_light_parser_options tests/test_file_scanner.py::test_scan_files_keeps_subprocess_path_for_pdf_in_direct_mode -q
```

Expected: targeted tests pass.

- [ ] **Step 9: Run full file scanner tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py -q
```

Expected: all file scanner tests pass. If tests that explicitly expect subprocess behavior fail, set their scanner overrides to `"worker_lane_mode": "subprocess"` because this plan intentionally changes direct-mode text-like routing.

- [ ] **Step 10: Commit Task 4**

Run:

```powershell
git add src/services/file_scanner.py tests/test_file_scanner.py
git commit -m "feat: route text files through light parser"
```

## Task 5: Add Parser Backend Summary To Benchmark Output

**Files:**
- Modify: `scripts/benchmark_scanner.py`
- Modify: `tests/test_benchmark_scanner.py`

- [ ] **Step 1: Update benchmark payload test**

In `tests/test_benchmark_scanner.py`, update the `ReparseDetail(...)` in `test_build_benchmark_payload_uses_scan_result_and_metrics()` with:

```python
            parser_backend="light_text_v1",
            truncated=True,
```

Update the expected `payload["reparse_details"]` dict with:

```python
            "parser_backend": "light_text_v1",
            "truncated": True,
```

Add this assertion after the reparse details assertion:

```python
assert payload["parser_backend_summary"] == {
    "direct_count": 1,
    "subprocess_count": 0,
    "truncated_count": 1,
    "by_extension": {
        ".md": {
            "light_text_v1": 1,
            "subprocess": 0,
            "truncated": 1,
        }
    },
}
```

- [ ] **Step 2: Update Markdown benchmark test**

In `test_render_markdown_report_contains_stage_and_extension_metrics()`, add this field to the payload:

```python
"parser_backend_summary": {
    "direct_count": 1,
    "subprocess_count": 0,
    "truncated_count": 1,
    "by_extension": {
        ".md": {
            "light_text_v1": 1,
            "subprocess": 0,
            "truncated": 1,
        }
    },
},
```

Also add `parser_backend` and `truncated` to the reparse detail dict:

```python
"parser_backend": "light_text_v1",
"truncated": True,
```

Add assertions:

```python
assert "## Parser Backend Summary" in markdown
assert "- direct_count: `1`" in markdown
assert "- subprocess_count: `0`" in markdown
assert "- truncated_count: `1`" in markdown
assert "| .md | light_text_v1 | 1 | 0 | 1 |" in markdown
```

- [ ] **Step 3: Run benchmark tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected before implementation: failure because `parser_backend_summary` is absent and Markdown lacks the new section.

- [ ] **Step 4: Add backend summary builder**

In `scripts/benchmark_scanner.py`, add this helper above `build_benchmark_payload()`:

```python
def build_parser_backend_summary(
    reparse_details: list[ReparseDetail],
) -> dict[str, Any]:
    """按解析后端聚合本轮重解析文件数量。"""
    summary: dict[str, Any] = {
        "direct_count": 0,
        "subprocess_count": 0,
        "truncated_count": 0,
        "by_extension": {},
    }
    for detail in reparse_details:
        backend = detail.parser_backend or "subprocess"
        extension = detail.extension
        by_extension = summary["by_extension"].setdefault(
            extension,
            {
                "light_text_v1": 0,
                "subprocess": 0,
                "truncated": 0,
            },
        )
        if backend == "light_text_v1":
            summary["direct_count"] += 1
            by_extension["light_text_v1"] += 1
        else:
            summary["subprocess_count"] += 1
            by_extension["subprocess"] += 1
        if detail.truncated:
            summary["truncated_count"] += 1
            by_extension["truncated"] += 1
    return summary
```

Update `build_benchmark_payload()` return dict:

```python
        "reparse_details": [item.to_dict() for item in reparse_details],
        "parser_backend_summary": build_parser_backend_summary(reparse_details),
```

- [ ] **Step 5: Render backend summary in Markdown**

In `render_markdown_report()`, read the summary after `reparse_details`:

```python
    parser_backend_summary = payload.get(
        "parser_backend_summary",
        {
            "direct_count": 0,
            "subprocess_count": 0,
            "truncated_count": 0,
            "by_extension": {},
        },
    )
```

After the Extension Metrics section and before Reparse Details, append:

```python
    lines.extend(
        [
            "",
            "## Parser Backend Summary",
            "",
            f"- direct_count: `{parser_backend_summary['direct_count']}`",
            f"- subprocess_count: `{parser_backend_summary['subprocess_count']}`",
            f"- truncated_count: `{parser_backend_summary['truncated_count']}`",
            "",
            "| extension | light_text_v1 | subprocess | truncated |",
            "|---|---:|---:|---:|",
        ]
    )
    by_extension = parser_backend_summary.get("by_extension", {})
    if by_extension:
        for extension in sorted(by_extension):
            item = by_extension[extension]
            lines.append(
                "| {extension} | {light_text_v1} | {subprocess} | {truncated} |".format(
                    extension=extension,
                    light_text_v1=item.get("light_text_v1", 0),
                    subprocess=item.get("subprocess", 0),
                    truncated=item.get("truncated", 0),
                )
            )
    else:
        lines.append("| (none) | 0 | 0 | 0 |")
```

- [ ] **Step 6: Run benchmark tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected: all benchmark tests pass.

- [ ] **Step 7: Commit Task 5**

Run:

```powershell
git add scripts/benchmark_scanner.py tests/test_benchmark_scanner.py
git commit -m "feat: report scanner parser backends"
```

## Task 6: Full Regression And Benchmark Smoke

**Files:**
- No planned source edits.
- Use current tracked changes from Tasks 1-5 only.

- [ ] **Step 1: Run focused scanner test group**

Run:

```powershell
conda run -n test python -m pytest tests/test_light_text_parser.py tests/test_scan_planner.py tests/test_file_scanner.py tests/test_benchmark_scanner.py -q
```

Expected: all focused tests pass.

- [ ] **Step 2: Run full test suite**

Run:

```powershell
conda run -n test python -m pytest tests/ -q
```

Expected: full test suite passes.

- [ ] **Step 3: Run a small benchmark smoke**

Use a temp output directory so benchmark artifacts do not pollute the scan sample:

```powershell
$out = Join-Path $env:TEMP "ai_daily_report_benchmarks"
New-Item -ItemType Directory -Force -Path $out | Out-Null
conda run -n test python scripts/benchmark_scanner.py `
  --start-date 2026-05-23 `
  --end-date 2026-05-24 `
  --json-out (Join-Path $out "scanner_benchmark_light_text_smoke.json") `
  --markdown-out (Join-Path $out "scanner_benchmark_light_text_smoke.md")
```

Expected:

- Command exits with code 0.
- JSON contains `parser_backend_summary`.
- Markdown contains `## Parser Backend Summary`.

- [ ] **Step 4: Inspect benchmark summary**

Run:

```powershell
Get-Content (Join-Path $env:TEMP "ai_daily_report_benchmarks\scanner_benchmark_light_text_smoke.json") |
  Select-String -Pattern '"parser_backend_summary"|"direct_count"|"subprocess_count"|"truncated_count"'
```

Expected: output includes the four keys. Counts depend on the local scan sample, so do not assert a fixed number here.

- [ ] **Step 5: Check git status before handoff**

Run:

```powershell
git status --short --branch
```

Expected:

- Implementation commits from Tasks 1-5 are present.
- Existing unrelated local changes such as `config/settings.toml` or `.codegraph/codex-refresh-head.txt` remain untouched unless the user explicitly asked to include them.

- [ ] **Step 6: Final implementation handoff**

Report:

- commits created;
- tests run and pass/fail status;
- benchmark smoke output paths;
- whether any unrelated pre-existing dirty files remain.
