# Context Scheduler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a deterministic `context_scheduler.py` + `context_compressor.py` layer that turns scanner output into auditable, budgeted LLM file context for daily, weekly, and monthly CLI scan flows.

**Architecture:** Keep `FileScanner` responsible for discovery/cache/parse and add a separate context orchestration layer above it. `ContextScheduler` creates context profiles and file decisions, `ContextCompressor` renders Markdown-like evidence context, and `ScanIndexStore` records context runs and decisions for benchmark and audit.

**Tech Stack:** Python 3.10+, dataclasses, Pydantic `ScanResult` / `FileContext`, SQLite via `sqlite3`, pytest, current scanner/parser services, CLI scripts under `scripts/`.

---

## Scope And File Structure

Create:

- `src/services/context_compressor.py`
  - Owns `ContextProfile`, `ContextDecision`, `CompressedContext`, deterministic action constants, and `ContextCompressor`.
  - Does not call LLM.
  - Does not read or parse files.

- `src/services/context_scheduler.py`
  - Owns `ContextScheduleRequest`, `ContextScheduleResult`, `ContextScheduler`.
  - Calls `FileScanner`, builds decisions, calls `ContextCompressor`, writes context audit rows through `ScanIndexStore`.

- `tests/test_context_compressor.py`
  - Unit tests for deterministic compression, budget handling, metadata-only blocks, omitted summary, and stable output.

- `tests/test_context_scheduler.py`
  - Unit tests for scheduler orchestration using stub scanner/store/compressor behavior.

- `scripts/benchmark_context_scheduler.py`
  - Real benchmark entry for context scheduling and compression metrics.

Modify:

- `src/services/scan_index_store.py`
  - Add `context_runs` and `context_decisions` schema.
  - Add save/read methods for context run and decisions.

- `tests/test_scan_index_store.py`
  - Add storage tests for context run and decision persistence.

- `main.py`
  - Use `ContextScheduler` for `daily`, `weekly --source scan`, and `monthly --source scan`.
  - Keep `build_file_context()` as legacy fallback.

- `tests/test_main.py`
  - Add or update scan-source tests to assert `ContextScheduler` is used.

- `tests/test_benchmark_scanner.py`
  - No production change expected. Add benchmark script tests in a new `tests/test_benchmark_context_scheduler.py`.

No production parser behavior should change in this plan.

---

## Task 1: Context Compressor Models And Deterministic Compression

**Files:**
- Create: `src/services/context_compressor.py`
- Create: `tests/test_context_compressor.py`

- [ ] **Step 1: Write failing tests for compressor output shape and keep/compress/metadata/omit actions**

Create `tests/test_context_compressor.py` with:

```python
"""测试 context compressor 的确定性上下文输出。"""

from pathlib import Path

from src.models.schemas import FileContext, ScanResult
from src.services.context_compressor import (
    ACTION_COMPRESS,
    ACTION_KEEP,
    ACTION_METADATA_ONLY,
    ACTION_OMIT,
    ContextCompressor,
    ContextDecision,
    ContextProfile,
)


def _decision(
    path: str,
    action: str,
    reason: str,
    *,
    priority: int = 10,
    parser_backend: str = "light_text_v1",
    input_chars: int = 0,
) -> ContextDecision:
    return ContextDecision(
        file_path=path,
        extension=Path(path).suffix.lower(),
        size_bytes=123,
        parser_backend=parser_backend,
        worker_lane="direct",
        cache_status="fresh",
        action=action,
        reason=reason,
        priority=priority,
        input_chars=input_chars,
        output_chars=0,
        truncated=False,
        error=None,
    )


def test_compress_keeps_small_file_and_renders_audit_header() -> None:
    profile = ContextProfile.for_report_mode("daily")
    compressor = ContextCompressor()
    context = FileContext(
        file_path="D:/work/report.md",
        file_type=".md",
        content="# 今日工作\n完成 scanner 验证。",
        parser_backend="light_text_v1",
    )
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[context],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision(
                "D:/work/report.md",
                ACTION_KEEP,
                "small_file_keep",
                input_chars=len(context.content),
            )
        ],
        profile=profile,
    )

    assert "# 文件证据上下文" in compressed.content
    assert "## 本轮摘要" in compressed.content
    assert "## 文件证据" in compressed.content
    assert "D:/work/report.md" in compressed.content
    assert "# 今日工作" in compressed.content
    assert compressed.source_file_count == 1
    assert compressed.included_file_count == 1
    assert compressed.omitted_file_count == 0
    assert compressed.output_chars == len(compressed.content)


def test_compress_limits_single_large_file_by_per_file_budget() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    profile = profile.with_budget(global_context_max_chars=5000, per_file_max_chars=80)
    compressor = ContextCompressor()
    content = "A" * 300
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path="D:/work/large.md",
                file_type=".md",
                content=content,
                parser_backend="light_text_v1",
            )
        ],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision(
                "D:/work/large.md",
                ACTION_COMPRESS,
                "medium_text_compress",
                input_chars=len(content),
            )
        ],
        profile=profile,
    )

    assert "内容已按单文件预算截断" in compressed.content
    assert "A" * 80 in compressed.content
    assert "A" * 120 not in compressed.content
    assert compressed.compressed_file_count == 1


def test_compress_renders_metadata_only_without_body_content() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    compressor = ContextCompressor()
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path="D:/work/huge.xlsx",
                file_type=".xlsx",
                content="secret body should not enter prompt",
                parser_backend="office_v1",
                truncated=True,
            )
        ],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision(
                "D:/work/huge.xlsx",
                ACTION_METADATA_ONLY,
                "file_size_policy",
                parser_backend="office_v1",
                input_chars=35,
            )
        ],
        profile=profile,
    )

    assert "huge.xlsx" in compressed.content
    assert "metadata_only" in compressed.content
    assert "file_size_policy" in compressed.content
    assert "secret body should not enter prompt" not in compressed.content
    assert compressed.metadata_only_count == 1


def test_compress_moves_over_budget_files_to_omitted_summary() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    profile = profile.with_budget(global_context_max_chars=1300, per_file_max_chars=500)
    compressor = ContextCompressor()
    contexts = [
        FileContext(
            file_path="D:/work/a.md",
            file_type=".md",
            content="A" * 500,
            parser_backend="light_text_v1",
        ),
        FileContext(
            file_path="D:/work/b.md",
            file_type=".md",
            content="B" * 500,
            parser_backend="light_text_v1",
        ),
    ]
    scan_result = ScanResult(total_files=2, success_count=2, error_count=0, contexts=contexts)

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision("D:/work/a.md", ACTION_KEEP, "small_file_keep", input_chars=500),
            _decision("D:/work/b.md", ACTION_KEEP, "small_file_keep", input_chars=500),
        ],
        profile=profile,
    )

    assert "D:/work/a.md" in compressed.content
    assert "## 省略文件摘要" in compressed.content
    assert "D:/work/b.md" in compressed.content
    assert compressed.included_file_count == 1
    assert compressed.omitted_file_count == 1
    assert compressed.decisions[1].action == ACTION_OMIT
    assert compressed.decisions[1].reason == "global_budget_exceeded"


def test_compress_empty_scan_returns_auditable_empty_context() -> None:
    profile = ContextProfile.for_report_mode("monthly")
    compressor = ContextCompressor()
    scan_result = ScanResult(total_files=0, success_count=0, error_count=0, contexts=[])

    compressed = compressor.compress(scan_result=scan_result, decisions=[], profile=profile)

    assert "无文件证据" in compressed.content
    assert compressed.source_file_count == 0
    assert compressed.included_file_count == 0
    assert compressed.omitted_file_count == 0
```

- [ ] **Step 2: Run compressor tests and verify import failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_context_compressor.py -q
```

Expected: FAIL because `src.services.context_compressor` does not exist.

- [ ] **Step 3: Implement `src/services/context_compressor.py`**

Create `src/services/context_compressor.py` with:

```python
"""确定性文件上下文压缩服务。"""

from __future__ import annotations

from dataclasses import dataclass, replace
from pathlib import Path

from src.models.schemas import FileContext, ScanResult

ACTION_KEEP = "keep"
ACTION_COMPRESS = "compress"
ACTION_METADATA_ONLY = "metadata_only"
ACTION_OMIT = "omit"
ACTION_ERROR = "error"


@dataclass(frozen=True, slots=True)
class ContextProfile:
    """一次 context 构建使用的预算和策略版本。"""

    report_mode: str
    compression_profile: str
    global_context_max_chars: int
    per_file_max_chars: int
    small_file_max_bytes: int = 64 * 1024
    medium_file_max_bytes: int = 1024 * 1024
    large_file_max_bytes: int = 10 * 1024 * 1024
    version: str = "context_scheduler_v1"
    priority_policy: str = "default_v1"
    compression_policy: str = "markdown_context_v1"

    @classmethod
    def for_report_mode(cls, report_mode: str) -> "ContextProfile":
        """按报告模式选择默认预算，避免周报/月报被单个文件占满。"""
        normalized = report_mode.lower()
        if normalized == "daily":
            return cls(
                report_mode=normalized,
                compression_profile="daily_balanced_v1",
                global_context_max_chars=50000,
                per_file_max_chars=8000,
            )
        if normalized == "monthly":
            return cls(
                report_mode=normalized,
                compression_profile="monthly_balanced_v1",
                global_context_max_chars=60000,
                per_file_max_chars=4000,
            )
        return cls(
            report_mode=normalized,
            compression_profile="weekly_balanced_v1",
            global_context_max_chars=50000,
            per_file_max_chars=5000,
        )

    def with_budget(
        self,
        *,
        global_context_max_chars: int,
        per_file_max_chars: int,
    ) -> "ContextProfile":
        """测试和配置覆盖预算时复用不可变 profile。"""
        return replace(
            self,
            global_context_max_chars=max(1, int(global_context_max_chars)),
            per_file_max_chars=max(1, int(per_file_max_chars)),
        )

    def to_profile_dict(self) -> dict[str, int | str]:
        """返回稳定可序列化 profile，用于 context_profile_key。"""
        return {
            "version": self.version,
            "report_mode": self.report_mode,
            "compression_profile": self.compression_profile,
            "global_context_max_chars": self.global_context_max_chars,
            "per_file_max_chars": self.per_file_max_chars,
            "small_file_max_bytes": self.small_file_max_bytes,
            "medium_file_max_bytes": self.medium_file_max_bytes,
            "large_file_max_bytes": self.large_file_max_bytes,
            "priority_policy": self.priority_policy,
            "compression_policy": self.compression_policy,
        }


@dataclass(slots=True)
class ContextDecision:
    """单个文件在一次 context run 中的策略选择。"""

    file_path: str
    extension: str
    size_bytes: int | None
    parser_backend: str | None
    worker_lane: str | None
    cache_status: str
    action: str
    reason: str
    priority: int
    input_chars: int
    output_chars: int
    truncated: bool
    error: str | None


@dataclass(slots=True)
class CompressedContext:
    """压缩后的 LLM 上下文及审计统计。"""

    content: str
    source_file_count: int
    included_file_count: int
    omitted_file_count: int
    metadata_only_count: int
    compressed_file_count: int
    error_file_count: int
    truncated_file_count: int
    input_chars: int
    output_chars: int
    warnings: list[str]
    decisions: list[ContextDecision]

    @classmethod
    def empty(cls, error: str | None = None) -> "CompressedContext":
        """构建可审计空结果，避免异常路径返回 None。"""
        content = "无文件证据" if not error else f"文件上下文构建失败：{error}"
        return cls(
            content=content,
            source_file_count=0,
            included_file_count=0,
            omitted_file_count=0,
            metadata_only_count=0,
            compressed_file_count=0,
            error_file_count=1 if error else 0,
            truncated_file_count=0,
            input_chars=0,
            output_chars=len(content),
            warnings=[],
            decisions=[],
        )

    def to_summary(self) -> dict[str, int | float | str]:
        """输出 benchmark/store 共享的统计摘要。"""
        ratio = 0.0 if self.input_chars == 0 else self.output_chars / self.input_chars
        return {
            "source_file_count": self.source_file_count,
            "included_file_count": self.included_file_count,
            "omitted_file_count": self.omitted_file_count,
            "metadata_only_count": self.metadata_only_count,
            "compressed_file_count": self.compressed_file_count,
            "error_file_count": self.error_file_count,
            "truncated_file_count": self.truncated_file_count,
            "input_chars": self.input_chars,
            "output_chars": self.output_chars,
            "compression_ratio": round(ratio, 6),
        }


class ContextCompressor:
    """把 ScanResult 压缩成 LLM 可读且可审计的 Markdown-like context。"""

    def compress(
        self,
        *,
        scan_result: ScanResult,
        decisions: list[ContextDecision],
        profile: ContextProfile,
    ) -> CompressedContext:
        contexts_by_path = {ctx.file_path: ctx for ctx in scan_result.contexts}
        ordered_decisions = list(decisions)
        output_parts: list[str] = []
        included: list[ContextDecision] = []
        omitted: list[ContextDecision] = []
        warnings: list[str] = []

        header = self._render_header(scan_result, ordered_decisions, profile)
        output_parts.append(header)
        output_chars = len(header)

        if not ordered_decisions:
            empty_note = "## 文件证据\n\n无文件证据"
            output_parts.append(empty_note)
            output_chars += len(empty_note)

        for decision in ordered_decisions:
            context = contexts_by_path.get(decision.file_path)
            if decision.action == ACTION_OMIT:
                omitted.append(decision)
                continue

            block = self._render_block(decision, context, profile)
            if output_chars + len(block) > profile.global_context_max_chars:
                decision.action = ACTION_OMIT
                decision.reason = "global_budget_exceeded"
                decision.output_chars = 0
                omitted.append(decision)
                continue

            decision.output_chars = len(block)
            output_parts.append(block)
            output_chars += len(block)
            included.append(decision)

        omitted_summary = self._render_omitted_summary(omitted)
        if omitted_summary:
            if output_chars + len(omitted_summary) <= profile.global_context_max_chars:
                output_parts.append(omitted_summary)
                output_chars += len(omitted_summary)
            else:
                warnings.append("省略摘要因全局预算不足被缩短")

        problem_summary = self._render_problem_summary(scan_result)
        if output_chars + len(problem_summary) <= profile.global_context_max_chars:
            output_parts.append(problem_summary)
            output_chars += len(problem_summary)
        else:
            warnings.append("解析问题摘要因全局预算不足被省略")

        content = "\n\n".join(part for part in output_parts if part).strip()
        return CompressedContext(
            content=content or "无文件证据",
            source_file_count=scan_result.total_files,
            included_file_count=len(included),
            omitted_file_count=len(omitted),
            metadata_only_count=sum(
                1 for item in included if item.action == ACTION_METADATA_ONLY
            ),
            compressed_file_count=sum(
                1 for item in included if item.action == ACTION_COMPRESS
            ),
            error_file_count=scan_result.error_count,
            truncated_file_count=sum(1 for ctx in scan_result.contexts if ctx.truncated),
            input_chars=sum(len(ctx.content) for ctx in scan_result.contexts),
            output_chars=len(content or "无文件证据"),
            warnings=warnings,
            decisions=ordered_decisions,
        )

    def _render_header(
        self,
        scan_result: ScanResult,
        decisions: list[ContextDecision],
        profile: ContextProfile,
    ) -> str:
        """先说明上下文口径，避免 LLM 把压缩结果误判为全集。"""
        metadata_count = sum(1 for item in decisions if item.action == ACTION_METADATA_ONLY)
        omitted_count = sum(1 for item in decisions if item.action == ACTION_OMIT)
        return "\n".join(
            [
                "# 文件证据上下文",
                "",
                "## 本轮摘要",
                f"- 报告模式：{profile.report_mode}",
                f"- compression_profile：{profile.compression_profile}",
                f"- 扫描文件数：{scan_result.total_files}",
                f"- 初始仅保留元数据：{metadata_count}",
                f"- 初始省略文件：{omitted_count}",
                f"- 解析错误：{scan_result.error_count}",
                "",
                "## 重要提示",
                "- 部分文件可能因全局上下文预算被省略。",
                "- Office/PDF 仅使用已解析文本或结构化预览，不做 OCR。",
                "- Excel 仅保留有限 sheet / row / column 预览。",
                "",
                "## 文件证据",
            ]
        )

    def _render_block(
        self,
        decision: ContextDecision,
        context: FileContext | None,
        profile: ContextProfile,
    ) -> str:
        if context is None:
            return self._render_missing_context_block(decision)
        if decision.action == ACTION_METADATA_ONLY:
            return self._render_metadata_only_block(decision)
        if context.error or decision.action == ACTION_ERROR:
            return self._render_error_block(decision, context)
        return self._render_content_block(decision, context, profile)

    def _render_missing_context_block(self, decision: ContextDecision) -> str:
        return "\n".join(
            [
                f"### {decision.file_path}",
                f"- 类型：{decision.extension}",
                "- 策略：error",
                "- 原因：missing_context",
                "- 错误：scanner result 中缺少该文件上下文",
            ]
        )

    def _render_metadata_only_block(self, decision: ContextDecision) -> str:
        return "\n".join(
            [
                f"### {decision.file_path}",
                f"- 类型：{decision.extension}",
                f"- parser_backend：{decision.parser_backend or ''}",
                "- 策略：metadata_only",
                f"- 原因：{decision.reason}",
                f"- 文件大小：{decision.size_bytes if decision.size_bytes is not None else '未知'}",
            ]
        )

    def _render_error_block(
        self,
        decision: ContextDecision,
        context: FileContext,
    ) -> str:
        return "\n".join(
            [
                f"### {decision.file_path}",
                f"- 类型：{decision.extension}",
                f"- parser_backend：{decision.parser_backend or context.parser_backend or ''}",
                "- 策略：error",
                f"- 原因：{decision.reason}",
                f"- 错误：{context.error or decision.error or 'unknown error'}",
            ]
        )

    def _render_content_block(
        self,
        decision: ContextDecision,
        context: FileContext,
        profile: ContextProfile,
    ) -> str:
        body = context.content
        truncated_by_compressor = len(body) > profile.per_file_max_chars
        if truncated_by_compressor:
            body = body[: profile.per_file_max_chars]
        lines = [
            f"### {decision.file_path}",
            f"- 类型：{decision.extension}",
            f"- parser_backend：{decision.parser_backend or context.parser_backend or ''}",
            f"- 策略：{decision.action}",
            f"- 原因：{decision.reason}",
            f"- parser 截断：{'是' if context.truncated else '否'}",
        ]
        if truncated_by_compressor:
            lines.append("- 压缩说明：内容已按单文件预算截断")
        lines.extend(["", "```text", body, "```"])
        return "\n".join(lines)

    def _render_omitted_summary(self, omitted: list[ContextDecision]) -> str:
        if not omitted:
            return ""
        extension_counts: dict[str, int] = {}
        for decision in omitted:
            extension_counts[decision.extension] = extension_counts.get(decision.extension, 0) + 1
        extension_text = ", ".join(
            f"{extension} {count}" for extension, count in sorted(extension_counts.items())
        )
        examples = [f"- {decision.file_path}" for decision in omitted[:5]]
        return "\n".join(
            [
                "## 省略文件摘要",
                f"- 省略文件数：{len(omitted)}",
                f"- 主要类型：{extension_text}",
                "- 示例：",
                *examples,
            ]
        )

    def _render_problem_summary(self, scan_result: ScanResult) -> str:
        errors = [ctx for ctx in scan_result.contexts if ctx.error]
        if not errors:
            return "## 解析问题\n\n- 无"
        lines = ["## 解析问题"]
        for context in errors[:20]:
            lines.append(f"- {context.file_path}: {context.error}")
        return "\n".join(lines)
```

- [ ] **Step 4: Run compressor tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_context_compressor.py -q
```

Expected: PASS.

- [ ] **Step 5: Commit Task 1**

Run:

```powershell
git add src/services/context_compressor.py tests/test_context_compressor.py
git commit -m "Add context compressor service"
```

---

## Task 2: Store Context Runs And Decisions In ScanIndexStore

**Files:**
- Modify: `src/services/scan_index_store.py`
- Modify: `tests/test_scan_index_store.py`

- [ ] **Step 1: Write failing store tests for context schema and round trip**

Append to `tests/test_scan_index_store.py`:

```python
from src.services.context_compressor import ContextDecision
```

Add these tests:

```python
def test_index_store_creates_context_run_and_decision_tables(tmp_path: Path):
    """初始化索引库时应创建 context 调度审计表。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    table_names = store.list_tables()

    assert "context_runs" in table_names
    assert "context_decisions" in table_names


def test_context_run_and_decisions_round_trip(tmp_path: Path):
    """context run 和文件级 decision 应能落库并按 run_id 读回。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    scan_run_id = store.save_scan_run_metrics(
        discovered_count=2,
        reused_count=1,
        reparsed_count=1,
    )
    run_id = store.save_context_run(
        report_mode="weekly",
        start_date=date(2026, 5, 10),
        end_date=date(2026, 5, 24),
        compression_profile="weekly_balanced_v1",
        context_profile_key='{"version":"context_scheduler_v1"}',
        scan_run_id=scan_run_id,
        source_file_count=2,
        included_file_count=1,
        omitted_file_count=1,
        metadata_only_count=0,
        compressed_file_count=1,
        error_file_count=0,
        truncated_file_count=1,
        input_chars=1200,
        output_chars=500,
        duration_ms=33,
        status="success",
        error="",
    )
    decisions = [
        ContextDecision(
            file_path="D:/work/a.md",
            extension=".md",
            size_bytes=100,
            parser_backend="light_text_v1",
            worker_lane="direct",
            cache_status="fresh",
            action="keep",
            reason="small_file_keep",
            priority=10,
            input_chars=100,
            output_chars=120,
            truncated=False,
            error=None,
        ),
        ContextDecision(
            file_path="D:/work/b.xlsx",
            extension=".xlsx",
            size_bytes=5000000,
            parser_backend="office_v1",
            worker_lane="subprocess",
            cache_status="miss",
            action="compress",
            reason="large_document_summary",
            priority=30,
            input_chars=1100,
            output_chars=380,
            truncated=True,
            error=None,
        ),
    ]

    store.save_context_decisions(run_id, decisions)

    assert store.latest_context_run() == {
        "context_run_id": run_id,
        "report_mode": "weekly",
        "start_date": "2026-05-10",
        "end_date": "2026-05-24",
        "compression_profile": "weekly_balanced_v1",
        "context_profile_key": '{"version":"context_scheduler_v1"}',
        "scan_run_id": scan_run_id,
        "source_file_count": 2,
        "included_file_count": 1,
        "omitted_file_count": 1,
        "metadata_only_count": 0,
        "compressed_file_count": 1,
        "error_file_count": 0,
        "truncated_file_count": 1,
        "input_chars": 1200,
        "output_chars": 500,
        "duration_ms": 33,
        "status": "success",
        "error": "",
    }
    rows = store.list_context_decisions(run_id)
    assert rows == [
        {
            "context_run_id": run_id,
            "file_identity": "",
            "path": "D:/work/a.md",
            "extension": ".md",
            "size_bytes": 100,
            "parser_backend": "light_text_v1",
            "worker_lane": "direct",
            "cache_status": "fresh",
            "action": "keep",
            "reason": "small_file_keep",
            "priority": 10,
            "input_chars": 100,
            "output_chars": 120,
            "truncated": False,
            "error": "",
        },
        {
            "context_run_id": run_id,
            "file_identity": "",
            "path": "D:/work/b.xlsx",
            "extension": ".xlsx",
            "size_bytes": 5000000,
            "parser_backend": "office_v1",
            "worker_lane": "subprocess",
            "cache_status": "miss",
            "action": "compress",
            "reason": "large_document_summary",
            "priority": 30,
            "input_chars": 1100,
            "output_chars": 380,
            "truncated": True,
            "error": "",
        },
    ]
```

- [ ] **Step 2: Run store tests and verify missing methods/tables**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_index_store.py::test_index_store_creates_context_run_and_decision_tables tests/test_scan_index_store.py::test_context_run_and_decisions_round_trip -q
```

Expected: FAIL because context tables and store methods are not implemented.

- [ ] **Step 3: Add context schema to `ScanIndexStore._init_schema()`**

In `src/services/scan_index_store.py`, add these tables inside the existing `conn.executescript(...)` block after `scan_extension_metrics`:

```python
                CREATE TABLE IF NOT EXISTS context_runs (
                    context_run_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    report_mode TEXT NOT NULL,
                    start_date TEXT NOT NULL,
                    end_date TEXT NOT NULL,
                    compression_profile TEXT NOT NULL,
                    context_profile_key TEXT NOT NULL,
                    scan_run_id INTEGER,
                    source_file_count INTEGER NOT NULL DEFAULT 0,
                    included_file_count INTEGER NOT NULL DEFAULT 0,
                    omitted_file_count INTEGER NOT NULL DEFAULT 0,
                    metadata_only_count INTEGER NOT NULL DEFAULT 0,
                    compressed_file_count INTEGER NOT NULL DEFAULT 0,
                    error_file_count INTEGER NOT NULL DEFAULT 0,
                    truncated_file_count INTEGER NOT NULL DEFAULT 0,
                    input_chars INTEGER NOT NULL DEFAULT 0,
                    output_chars INTEGER NOT NULL DEFAULT 0,
                    duration_ms INTEGER NOT NULL DEFAULT 0,
                    status TEXT NOT NULL DEFAULT 'success',
                    error TEXT NOT NULL DEFAULT '',
                    FOREIGN KEY (scan_run_id) REFERENCES scan_runs(run_id)
                );

                CREATE TABLE IF NOT EXISTS context_decisions (
                    context_decision_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    context_run_id INTEGER NOT NULL,
                    file_identity TEXT NOT NULL DEFAULT '',
                    path TEXT NOT NULL,
                    extension TEXT NOT NULL,
                    size_bytes INTEGER,
                    parser_backend TEXT NOT NULL DEFAULT '',
                    worker_lane TEXT NOT NULL DEFAULT '',
                    cache_status TEXT NOT NULL DEFAULT '',
                    action TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    priority INTEGER NOT NULL DEFAULT 0,
                    input_chars INTEGER NOT NULL DEFAULT 0,
                    output_chars INTEGER NOT NULL DEFAULT 0,
                    truncated INTEGER NOT NULL DEFAULT 0,
                    error TEXT NOT NULL DEFAULT '',
                    FOREIGN KEY (context_run_id) REFERENCES context_runs(context_run_id)
                );
```

- [ ] **Step 4: Add context store methods**

In `src/services/scan_index_store.py`, add imports:

```python
from .context_compressor import ContextDecision
```

Add methods near `save_scan_run_metrics()`:

```python
    def save_context_run(
        self,
        *,
        report_mode: str,
        start_date: date,
        end_date: date,
        compression_profile: str,
        context_profile_key: str,
        scan_run_id: int | None,
        source_file_count: int,
        included_file_count: int,
        omitted_file_count: int,
        metadata_only_count: int,
        compressed_file_count: int,
        error_file_count: int,
        truncated_file_count: int,
        input_chars: int,
        output_chars: int,
        duration_ms: int,
        status: str,
        error: str,
    ) -> int:
        """保存一次 context 调度结果，空扫描也必须落库便于审计。"""
        with self._connect() as conn:
            cursor = conn.execute(
                """
                INSERT INTO context_runs (
                    report_mode,
                    start_date,
                    end_date,
                    compression_profile,
                    context_profile_key,
                    scan_run_id,
                    source_file_count,
                    included_file_count,
                    omitted_file_count,
                    metadata_only_count,
                    compressed_file_count,
                    error_file_count,
                    truncated_file_count,
                    input_chars,
                    output_chars,
                    duration_ms,
                    status,
                    error
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    report_mode,
                    start_date.isoformat(),
                    end_date.isoformat(),
                    compression_profile,
                    context_profile_key,
                    scan_run_id,
                    max(0, int(source_file_count)),
                    max(0, int(included_file_count)),
                    max(0, int(omitted_file_count)),
                    max(0, int(metadata_only_count)),
                    max(0, int(compressed_file_count)),
                    max(0, int(error_file_count)),
                    max(0, int(truncated_file_count)),
                    max(0, int(input_chars)),
                    max(0, int(output_chars)),
                    max(0, int(duration_ms)),
                    status,
                    error,
                ),
            )
            return int(cursor.lastrowid)

    def save_context_decisions(
        self,
        context_run_id: int,
        decisions: list[ContextDecision],
    ) -> None:
        """保存文件级 context decision，解释每个文件为什么进入或离开上下文。"""
        with self._connect() as conn:
            conn.executemany(
                """
                INSERT INTO context_decisions (
                    context_run_id,
                    file_identity,
                    path,
                    extension,
                    size_bytes,
                    parser_backend,
                    worker_lane,
                    cache_status,
                    action,
                    reason,
                    priority,
                    input_chars,
                    output_chars,
                    truncated,
                    error
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                [
                    (
                        context_run_id,
                        "",
                        decision.file_path,
                        decision.extension,
                        decision.size_bytes,
                        decision.parser_backend or "",
                        decision.worker_lane or "",
                        decision.cache_status,
                        decision.action,
                        decision.reason,
                        int(decision.priority),
                        max(0, int(decision.input_chars)),
                        max(0, int(decision.output_chars)),
                        int(bool(decision.truncated)),
                        decision.error or "",
                    )
                    for decision in decisions
                ],
            )

    def latest_context_run(self) -> dict[str, int | str | None] | None:
        """读取最近一次 context run；缺失时返回 None。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT
                    context_run_id,
                    report_mode,
                    start_date,
                    end_date,
                    compression_profile,
                    context_profile_key,
                    scan_run_id,
                    source_file_count,
                    included_file_count,
                    omitted_file_count,
                    metadata_only_count,
                    compressed_file_count,
                    error_file_count,
                    truncated_file_count,
                    input_chars,
                    output_chars,
                    duration_ms,
                    status,
                    error
                FROM context_runs
                ORDER BY context_run_id DESC
                LIMIT 1
                """
            ).fetchone()
        if row is None:
            return None
        return {
            "context_run_id": int(row["context_run_id"]),
            "report_mode": str(row["report_mode"]),
            "start_date": str(row["start_date"]),
            "end_date": str(row["end_date"]),
            "compression_profile": str(row["compression_profile"]),
            "context_profile_key": str(row["context_profile_key"]),
            "scan_run_id": None if row["scan_run_id"] is None else int(row["scan_run_id"]),
            "source_file_count": int(row["source_file_count"]),
            "included_file_count": int(row["included_file_count"]),
            "omitted_file_count": int(row["omitted_file_count"]),
            "metadata_only_count": int(row["metadata_only_count"]),
            "compressed_file_count": int(row["compressed_file_count"]),
            "error_file_count": int(row["error_file_count"]),
            "truncated_file_count": int(row["truncated_file_count"]),
            "input_chars": int(row["input_chars"]),
            "output_chars": int(row["output_chars"]),
            "duration_ms": int(row["duration_ms"]),
            "status": str(row["status"]),
            "error": str(row["error"]),
        }

    def list_context_decisions(
        self,
        context_run_id: int,
    ) -> list[dict[str, int | str | bool | None]]:
        """按 context_run_id 读取文件级决策。"""
        with self._connect() as conn:
            rows = conn.execute(
                """
                SELECT
                    context_run_id,
                    file_identity,
                    path,
                    extension,
                    size_bytes,
                    parser_backend,
                    worker_lane,
                    cache_status,
                    action,
                    reason,
                    priority,
                    input_chars,
                    output_chars,
                    truncated,
                    error
                FROM context_decisions
                WHERE context_run_id = ?
                ORDER BY context_decision_id
                """,
                (context_run_id,),
            ).fetchall()
        return [
            {
                "context_run_id": int(row["context_run_id"]),
                "file_identity": str(row["file_identity"]),
                "path": str(row["path"]),
                "extension": str(row["extension"]),
                "size_bytes": None if row["size_bytes"] is None else int(row["size_bytes"]),
                "parser_backend": str(row["parser_backend"]),
                "worker_lane": str(row["worker_lane"]),
                "cache_status": str(row["cache_status"]),
                "action": str(row["action"]),
                "reason": str(row["reason"]),
                "priority": int(row["priority"]),
                "input_chars": int(row["input_chars"]),
                "output_chars": int(row["output_chars"]),
                "truncated": bool(int(row["truncated"])),
                "error": str(row["error"]),
            }
            for row in rows
        ]
```

- [ ] **Step 5: Run store tests and full index-store tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_index_store.py -q
```

Expected: PASS.

- [ ] **Step 6: Commit Task 2**

Run:

```powershell
git add src/services/scan_index_store.py tests/test_scan_index_store.py
git commit -m "Persist context scheduler audit data"
```

---

## Task 3: Context Scheduler Decisions And Store Orchestration

**Files:**
- Create: `src/services/context_scheduler.py`
- Create: `tests/test_context_scheduler.py`

- [ ] **Step 1: Write failing scheduler tests**

Create `tests/test_context_scheduler.py` with:

```python
"""测试 context scheduler 的策略调度与审计落库。"""

from datetime import date
from pathlib import Path

from src.models.schemas import FileContext, ScanResult
from src.services.context_compressor import (
    ACTION_COMPRESS,
    ACTION_KEEP,
    ACTION_METADATA_ONLY,
)
from src.services.context_scheduler import ContextScheduleRequest, ContextScheduler


class StubScanner:
    def __init__(self, scan_result: ScanResult) -> None:
        self.scan_result = scan_result
        self.calls: list[tuple[date, date, bool]] = []
        self.scan_index_store = StubStore()

    def scan_files(
        self,
        start_date: date,
        end_date: date,
        summary_mode: bool = False,
    ) -> ScanResult:
        self.calls.append((start_date, end_date, summary_mode))
        return self.scan_result


class StubStore:
    def __init__(self) -> None:
        self.context_runs: list[dict[str, object]] = []
        self.context_decisions: list[object] = []

    def latest_scan_run_detail(self) -> dict[str, int]:
        return {"run_id": 77}

    def save_context_run(self, **kwargs) -> int:
        self.context_runs.append(kwargs)
        return 123

    def save_context_decisions(self, context_run_id: int, decisions) -> None:
        self.context_decisions.append((context_run_id, list(decisions)))


def test_scheduler_builds_weekly_context_and_records_audit(tmp_path: Path) -> None:
    small = tmp_path / "report.md"
    small.write_text("完成周报设计", encoding="utf-8")
    large = tmp_path / "large.log"
    large.write_text("x" * 2000000, encoding="utf-8")
    scan_result = ScanResult(
        total_files=2,
        success_count=2,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(small),
                file_type=".md",
                content="完成周报设计",
                parser_backend="light_text_v1",
            ),
            FileContext(
                file_path=str(large),
                file_type=".log",
                content="x" * 5000,
                parser_backend="light_text_v1",
                truncated=True,
            ),
        ],
    )
    scanner = StubScanner(scan_result)
    scheduler = ContextScheduler(scanner_factory=lambda: scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="weekly",
            source="scan",
            start_date=date(2026, 5, 10),
            end_date=date(2026, 5, 24),
        )
    )

    assert scanner.calls == [(date(2026, 5, 10), date(2026, 5, 24), True)]
    assert result.context_run_id == 123
    assert "完成周报设计" in result.file_context
    assert [decision.action for decision in result.decisions] == [
        ACTION_KEEP,
        ACTION_COMPRESS,
    ]
    assert scanner.scan_index_store.context_runs[0]["report_mode"] == "weekly"
    assert scanner.scan_index_store.context_runs[0]["scan_run_id"] == 77
    assert scanner.scan_index_store.context_decisions[0][0] == 123


def test_scheduler_marks_oversized_file_as_metadata_only(tmp_path: Path) -> None:
    huge = tmp_path / "huge.xlsx"
    huge.write_bytes(b"x" * (11 * 1024 * 1024))
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(huge),
                file_type=".xlsx",
                content="sheet preview",
                parser_backend="office_v1",
                truncated=True,
            )
        ],
    )
    scanner = StubScanner(scan_result)
    scheduler = ContextScheduler(scanner_factory=lambda: scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="monthly",
            source="scan",
            start_date=date(2026, 5, 1),
            end_date=date(2026, 5, 31),
        )
    )

    assert result.decisions[0].action == ACTION_METADATA_ONLY
    assert result.decisions[0].reason == "file_size_policy"
    assert "sheet preview" not in result.file_context


def test_scheduler_records_error_run_when_compressor_fails(tmp_path: Path) -> None:
    sample = tmp_path / "report.md"
    sample.write_text("content", encoding="utf-8")
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(sample),
                file_type=".md",
                content="content",
                parser_backend="light_text_v1",
            )
        ],
    )
    scanner = StubScanner(scan_result)

    class FailingCompressor:
        def compress(self, *, scan_result, decisions, profile):
            raise RuntimeError("compress failed")

    scheduler = ContextScheduler(
        scanner_factory=lambda: scanner,
        compressor=FailingCompressor(),
    )

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="daily",
            source="scan",
            start_date=date(2026, 5, 24),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.error == "compress failed"
    assert "文件上下文构建失败" in result.file_context
    assert scanner.scan_index_store.context_runs[0]["status"] == "error"
```

- [ ] **Step 2: Run scheduler tests and verify import failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_context_scheduler.py -q
```

Expected: FAIL because `context_scheduler.py` does not exist.

- [ ] **Step 3: Implement `src/services/context_scheduler.py`**

Create `src/services/context_scheduler.py` with:

```python
"""CLI 生命周期内的文件上下文调度服务。"""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from time import perf_counter
from typing import Callable

from src.models.schemas import FileContext, ScanResult
from src.services.context_compressor import (
    ACTION_COMPRESS,
    ACTION_ERROR,
    ACTION_KEEP,
    ACTION_METADATA_ONLY,
    CompressedContext,
    ContextCompressor,
    ContextDecision,
    ContextProfile,
)
from src.services.file_scanner import FileScanner


@dataclass(frozen=True, slots=True)
class ContextScheduleRequest:
    """一次 CLI run 的上下文构建请求。"""

    report_mode: str
    source: str
    start_date: date
    end_date: date
    compression_profile: str | None = None
    user_input: str | None = None


@dataclass(slots=True)
class ContextScheduleResult:
    """上下文调度结果，供 main.py 传给 LLMClient。"""

    file_context: str
    compressed_context: CompressedContext
    scan_result: ScanResult | None
    context_run_id: int | None
    decisions: list[ContextDecision]
    error: str | None = None


class ContextScheduler:
    """在一次 CLI run 内编排 scanner 和 compressor。"""

    def __init__(
        self,
        *,
        scanner_factory: Callable[[], FileScanner] | None = None,
        compressor: ContextCompressor | None = None,
    ) -> None:
        self._scanner_factory = scanner_factory or FileScanner
        self._compressor = compressor or ContextCompressor()

    def build_context(self, request: ContextScheduleRequest) -> ContextScheduleResult:
        """构建 LLM 文件上下文，并落库 context run / decisions。"""
        self._validate_request(request)
        started_at = perf_counter()
        scanner = self._scanner_factory()
        store = scanner.scan_index_store
        profile = self._build_profile(request)
        context_profile_key = self._serialize_profile(profile)
        scan_result: ScanResult | None = None
        decisions: list[ContextDecision] = []

        try:
            scan_result = scanner.scan_files(
                start_date=request.start_date,
                end_date=request.end_date,
                summary_mode=self._resolve_summary_mode(request.report_mode),
            )
            decisions = self._build_decisions(scan_result, profile)
            compressed = self._compressor.compress(
                scan_result=scan_result,
                decisions=decisions,
                profile=profile,
            )
            context_run_id = store.save_context_run(
                report_mode=request.report_mode,
                start_date=request.start_date,
                end_date=request.end_date,
                compression_profile=profile.compression_profile,
                context_profile_key=context_profile_key,
                scan_run_id=self._latest_scan_run_id(store),
                source_file_count=compressed.source_file_count,
                included_file_count=compressed.included_file_count,
                omitted_file_count=compressed.omitted_file_count,
                metadata_only_count=compressed.metadata_only_count,
                compressed_file_count=compressed.compressed_file_count,
                error_file_count=compressed.error_file_count,
                truncated_file_count=compressed.truncated_file_count,
                input_chars=compressed.input_chars,
                output_chars=compressed.output_chars,
                duration_ms=self._elapsed_ms(started_at),
                status="success",
                error="",
            )
            store.save_context_decisions(context_run_id, compressed.decisions)
            return ContextScheduleResult(
                file_context=compressed.content,
                compressed_context=compressed,
                scan_result=scan_result,
                context_run_id=context_run_id,
                decisions=compressed.decisions,
            )
        except Exception as exc:
            error_text = str(exc)
            fallback = CompressedContext.empty(error=error_text)
            context_run_id = store.save_context_run(
                report_mode=request.report_mode,
                start_date=request.start_date,
                end_date=request.end_date,
                compression_profile=profile.compression_profile,
                context_profile_key=context_profile_key,
                scan_run_id=self._latest_scan_run_id(store),
                source_file_count=scan_result.total_files if scan_result else 0,
                included_file_count=0,
                omitted_file_count=0,
                metadata_only_count=0,
                compressed_file_count=0,
                error_file_count=fallback.error_file_count,
                truncated_file_count=0,
                input_chars=0,
                output_chars=fallback.output_chars,
                duration_ms=self._elapsed_ms(started_at),
                status="error",
                error=error_text,
            )
            return ContextScheduleResult(
                file_context=fallback.content,
                compressed_context=fallback,
                scan_result=scan_result,
                context_run_id=context_run_id,
                decisions=decisions,
                error=error_text,
            )

    def _validate_request(self, request: ContextScheduleRequest) -> None:
        if request.source != "scan":
            raise ValueError("ContextScheduler only supports source='scan'")
        if request.end_date < request.start_date:
            raise ValueError("end_date must be greater than or equal to start_date")
        if request.report_mode not in {"daily", "weekly", "monthly"}:
            raise ValueError("report_mode must be daily, weekly, or monthly")

    def _build_profile(self, request: ContextScheduleRequest) -> ContextProfile:
        profile = ContextProfile.for_report_mode(request.report_mode)
        if request.compression_profile:
            return ContextProfile(
                report_mode=profile.report_mode,
                compression_profile=request.compression_profile,
                global_context_max_chars=profile.global_context_max_chars,
                per_file_max_chars=profile.per_file_max_chars,
                small_file_max_bytes=profile.small_file_max_bytes,
                medium_file_max_bytes=profile.medium_file_max_bytes,
                large_file_max_bytes=profile.large_file_max_bytes,
                version=profile.version,
                priority_policy=profile.priority_policy,
                compression_policy=profile.compression_policy,
            )
        return profile

    def _serialize_profile(self, profile: ContextProfile) -> str:
        return json.dumps(
            profile.to_profile_dict(),
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        )

    def _resolve_summary_mode(self, report_mode: str) -> bool:
        return report_mode in {"weekly", "monthly"}

    def _build_decisions(
        self,
        scan_result: ScanResult,
        profile: ContextProfile,
    ) -> list[ContextDecision]:
        decisions = [
            self._build_decision(context, profile)
            for context in scan_result.contexts
        ]
        return sorted(
            decisions,
            key=lambda item: (item.priority, item.file_path.lower()),
        )

    def _build_decision(
        self,
        context: FileContext,
        profile: ContextProfile,
    ) -> ContextDecision:
        path = Path(context.file_path)
        size_bytes = self._safe_file_size(path)
        priority = self._priority_for(path, context)
        input_chars = len(context.content)
        if context.error:
            action = ACTION_ERROR
            reason = "parse_error"
        elif size_bytes is not None and size_bytes > profile.large_file_max_bytes:
            action = ACTION_METADATA_ONLY
            reason = "file_size_policy"
        elif input_chars <= profile.per_file_max_chars and not context.truncated:
            action = ACTION_KEEP
            reason = "small_file_keep"
        else:
            action = ACTION_COMPRESS
            reason = self._compress_reason(context.file_type)
        return ContextDecision(
            file_path=context.file_path,
            extension=context.file_type,
            size_bytes=size_bytes,
            parser_backend=context.parser_backend,
            worker_lane=None,
            cache_status="unknown",
            action=action,
            reason=reason,
            priority=priority,
            input_chars=input_chars,
            output_chars=0,
            truncated=context.truncated,
            error=context.error,
        )

    def _compress_reason(self, extension: str) -> str:
        if extension == ".log":
            return "large_log_tail"
        if extension in {".docx", ".xlsx", ".pptx", ".pdf"}:
            return "large_document_summary"
        return "medium_text_compress"

    def _priority_for(self, path: Path, context: FileContext) -> int:
        path_text = str(path).lower()
        if context.error:
            return 80
        if ".pytest_cache" in path_text or "\\data\\benchmarks\\" in path_text:
            return 70
        if "\\logs\\" in path_text:
            return 60
        if context.file_type in {".docx", ".xlsx", ".pptx", ".pdf"}:
            return 20
        if context.file_type in {".md", ".txt"}:
            return 30
        return 50

    def _safe_file_size(self, path: Path) -> int | None:
        try:
            return path.stat().st_size
        except OSError:
            return None

    def _latest_scan_run_id(self, store) -> int | None:
        try:
            detail = store.latest_scan_run_detail()
        except Exception:
            return None
        run_id = detail.get("run_id") if detail else None
        return None if run_id is None else int(run_id)

    def _elapsed_ms(self, started_at: float) -> int:
        return max(0, int(round((perf_counter() - started_at) * 1000)))
```

- [ ] **Step 4: Run scheduler tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_context_scheduler.py -q
```

Expected: PASS.

- [ ] **Step 5: Commit Task 3**

Run:

```powershell
git add src/services/context_scheduler.py tests/test_context_scheduler.py
git commit -m "Add context scheduler orchestration"
```

---

## Task 4: Wire ContextScheduler Into CLI Scan Flows

**Files:**
- Modify: `main.py`
- Modify: `tests/test_main.py`

- [ ] **Step 1: Write failing CLI scan-flow tests**

In `tests/test_main.py`, add imports:

```python
from src.services.context_compressor import CompressedContext
from src.services.context_scheduler import ContextScheduleResult
```

Add this helper near the test stubs:

```python
def _schedule_result(file_context: str) -> ContextScheduleResult:
    compressed = CompressedContext.empty()
    compressed.content = file_context
    compressed.output_chars = len(file_context)
    return ContextScheduleResult(
        file_context=file_context,
        compressed_context=compressed,
        scan_result=ScanResult(total_files=1, success_count=1, error_count=0, contexts=[]),
        context_run_id=1,
        decisions=[],
    )
```

Add tests:

```python
def test_generate_daily_report_uses_context_scheduler(monkeypatch):
    calls: list[tuple[str, object]] = []

    class StubContextScheduler:
        def build_context(self, request):
            calls.append(("build_context", (request.report_mode, request.source)))
            return _schedule_result("scheduler daily context")

    class StubSQLiteStore:
        def __init__(self) -> None:
            calls.append(("init", None))

        def get_yesterday_plan(self) -> str:
            calls.append(("get_yesterday_plan", None))
            return "昨日计划"

        def save_report(self, report: DailyReportData) -> None:
            calls.append(("save_report", report.date))

    class StubReportGenerator:
        def render_markdown(self, report: DailyReportData) -> str:
            return "daily markdown"

        def save_markdown(self, markdown: str, report_date: str) -> None:
            calls.append(("save_markdown", report_date))

    class StubLLMClient:
        def generate_report(
            self,
            user_input: str,
            file_context: str,
            yesterday_plan: str,
        ) -> DailyReportData:
            calls.append(("generate_report", file_context))
            return DailyReportData(
                date="2026-02-03",
                completed_work="完成日报",
                work_summary="日报摘要",
                next_plan="后续计划",
            )

    _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    main.generate_daily_report(Namespace(input="今天工作", no_save=False, date=None))

    assert ("build_context", ("daily", "scan")) in calls
    assert ("generate_report", "scheduler daily context") in calls


def test_generate_weekly_report_scan_uses_context_scheduler(monkeypatch):
    calls: list[tuple[str, object]] = []

    class StubContextScheduler:
        def build_context(self, request):
            calls.append(
                (
                    "build_context",
                    (
                        request.report_mode,
                        request.source,
                        request.start_date.isoformat(),
                        request.end_date.isoformat(),
                    ),
                )
            )
            return _schedule_result("scheduler weekly context")

    class StubSQLiteStore:
        def save_weekly_report(self, report: WeeklyReportData) -> None:
            calls.append(("save_weekly_report", report.week_label))

    class StubReportGenerator:
        def render_weekly_markdown(self, report: WeeklyReportData) -> str:
            return "weekly markdown"

        def save_weekly_markdown(self, markdown: str, year: int, week: int) -> None:
            calls.append(("save_weekly_markdown", (year, week)))

    class StubLLMClient:
        def generate_weekly_report(
            self,
            reports,
            file_context: str,
            year: int,
            week: int,
            missing_days: list[str],
            data_source: str,
        ) -> WeeklyReportData:
            calls.append(("generate_weekly_report", file_context))
            return WeeklyReportData(
                week_label=f"{year}-W{week:02d}",
                date_range="2026-05-11 ~ 2026-05-17",
                completed_work="完成周报",
                self_growth="成长",
                improvement_actions="改善",
                work_summary="总结",
                next_plan="计划",
                support_needed="支持",
                other_notes="其他",
            )

    _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    main.generate_weekly_report_cmd(
        Namespace(week="2026-W20", source="scan", input=None, no_save=False)
    )

    assert ("generate_weekly_report", "scheduler weekly context") in calls
    assert (
        "build_context",
        ("weekly", "scan", "2026-05-11", "2026-05-17"),
    ) in calls


def test_generate_monthly_report_scan_uses_context_scheduler(monkeypatch):
    calls: list[tuple[str, object]] = []

    class StubContextScheduler:
        def build_context(self, request):
            calls.append(
                (
                    "build_context",
                    (
                        request.report_mode,
                        request.source,
                        request.start_date.isoformat(),
                        request.end_date.isoformat(),
                    ),
                )
            )
            return _schedule_result("scheduler monthly context")

    class StubSQLiteStore:
        def save_monthly_report(self, report: MonthlyReportData) -> None:
            calls.append(("save_monthly_report", report.year_month))

    class StubReportGenerator:
        def render_monthly_markdown(self, report: MonthlyReportData) -> str:
            return "monthly markdown"

        def save_monthly_markdown(self, markdown: str, year_month: str) -> None:
            calls.append(("save_monthly_markdown", year_month))

    class StubLLMClient:
        def generate_monthly_report(
            self,
            reports,
            file_context: str,
            year_month: str,
            missing_days: list[str],
            data_source: str,
        ) -> MonthlyReportData:
            calls.append(("generate_monthly_report", file_context))
            return MonthlyReportData(
                year_month=year_month,
                overview="概览",
                completed_work="完成",
                work_summary="总结",
                next_plan="计划",
            )

    _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    main.generate_monthly_report_cmd(
        Namespace(month="2026-05", source="scan", input=None, no_save=False)
    )

    assert ("generate_monthly_report", "scheduler monthly context") in calls
    assert (
        "build_context",
        ("monthly", "scan", "2026-05-01", "2026-05-31"),
    ) in calls
```

- [ ] **Step 2: Run CLI tests and verify missing import/use**

Run:

```powershell
conda run -n test python -m pytest tests/test_main.py -q
```

Expected: FAIL because `main.ContextScheduler` is not imported or used yet.

- [ ] **Step 3: Import scheduler types in `main.py`**

In `main.py`, add:

```python
from src.services.context_scheduler import ContextScheduleRequest, ContextScheduler
```

- [ ] **Step 4: Replace daily scan path with ContextScheduler**

In `generate_daily_report`, replace direct scanner usage with:

```python
    scheduler = ContextScheduler()
    store = SQLiteStore()
    report_gen = ReportGenerator()
    llm_client = LLMClient()

    with Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        console=console,
    ) as progress:
        task = progress.add_task("构建今日文件上下文...", total=None)
        today = date.today()
        context_result = scheduler.build_context(
            ContextScheduleRequest(
                report_mode="daily",
                source="scan",
                start_date=today - timedelta(days=1),
                end_date=today,
            )
        )
        progress.update(task, completed=True)

    scan_result = context_result.scan_result
    if scan_result is not None:
        console.print(
            f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
        )
    file_context = context_result.file_context
```

Keep the yesterday plan and LLM sections unchanged except they should use `file_context`.

- [ ] **Step 5: Replace weekly scan source path**

In `generate_weekly_report_cmd`, replace the `case "scan"` block with:

```python
        case "scan":
            scheduler = ContextScheduler()
            with Progress(
                SpinnerColumn(),
                TextColumn("[progress.description]{task.description}"),
                console=console,
            ) as progress:
                task = progress.add_task(
                    f"构建 {monday} ~ {sunday} 文件上下文...", total=None
                )
                context_result = scheduler.build_context(
                    ContextScheduleRequest(
                        report_mode="weekly",
                        source="scan",
                        start_date=monday,
                        end_date=sunday,
                    )
                )
                progress.update(task, completed=True)

            scan_result = context_result.scan_result
            if scan_result is not None:
                console.print(
                    f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
                )
            file_context = context_result.file_context
```

- [ ] **Step 6: Replace monthly scan source path**

In `generate_monthly_report_cmd`, replace the `case "scan"` block with:

```python
        case "scan":
            scheduler = ContextScheduler()
            with Progress(
                SpinnerColumn(),
                TextColumn("[progress.description]{task.description}"),
                console=console,
            ) as progress:
                task = progress.add_task(
                    f"构建 {start_date} ~ {end_date} 文件上下文...", total=None
                )
                context_result = scheduler.build_context(
                    ContextScheduleRequest(
                        report_mode="monthly",
                        source="scan",
                        start_date=start_date,
                        end_date=end_date,
                    )
                )
                progress.update(task, completed=True)

            scan_result = context_result.scan_result
            if scan_result is not None:
                console.print(
                    f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
                )
            file_context = context_result.file_context
```

- [ ] **Step 7: Run main tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_main.py -q
```

Expected: PASS.

- [ ] **Step 8: Commit Task 4**

Run:

```powershell
git add main.py tests/test_main.py
git commit -m "Use context scheduler for scan report flows"
```

---

## Task 5: Context Scheduler Benchmark Script

**Files:**
- Create: `scripts/benchmark_context_scheduler.py`
- Create: `tests/test_benchmark_context_scheduler.py`

- [ ] **Step 1: Write failing benchmark tests**

Create `tests/test_benchmark_context_scheduler.py`:

```python
"""测试 context scheduler benchmark 输出。"""

from datetime import date

from src.services.context_compressor import CompressedContext
from scripts.benchmark_context_scheduler import (
    build_context_scheduler_summary,
    build_benchmark_payload,
    render_markdown_report,
)


def test_build_context_scheduler_summary_counts_actions_and_backends():
    compressed = CompressedContext.empty()
    compressed.source_file_count = 3
    compressed.included_file_count = 2
    compressed.omitted_file_count = 1
    compressed.metadata_only_count = 1
    compressed.compressed_file_count = 1
    compressed.error_file_count = 0
    compressed.truncated_file_count = 1
    compressed.input_chars = 1000
    compressed.output_chars = 250
    compressed.decisions = []

    summary = build_context_scheduler_summary(compressed)

    assert summary["source_file_count"] == 3
    assert summary["compression_ratio"] == 0.25


def test_build_benchmark_payload_contains_context_summary():
    compressed = CompressedContext.empty()
    compressed.source_file_count = 1
    compressed.included_file_count = 1
    compressed.input_chars = 100
    compressed.output_chars = 50

    payload = build_benchmark_payload(
        compressed_context=compressed,
        context_run={"context_run_id": 7, "status": "success"},
        parameters={
            "start_date": "2026-05-10",
            "end_date": "2026-05-24",
            "report_mode": "weekly",
            "compression_profile": "weekly_balanced_v1",
        },
        scan_run={"run_id": 3, "discovered_count": 1, "reused_count": 1, "reparsed_count": 0},
    )

    assert payload["context_run"]["context_run_id"] == 7
    assert payload["context_scheduler_summary"]["source_file_count"] == 1
    assert payload["parameters"]["report_mode"] == "weekly"


def test_render_markdown_report_mentions_context_summary():
    payload = {
        "parameters": {
            "start_date": "2026-05-10",
            "end_date": "2026-05-24",
            "report_mode": "weekly",
            "compression_profile": "weekly_balanced_v1",
        },
        "scan_run": {"run_id": 3, "discovered_count": 1, "reused_count": 1, "reparsed_count": 0},
        "context_run": {"context_run_id": 7, "status": "success"},
        "context_scheduler_summary": {
            "source_file_count": 1,
            "included_file_count": 1,
            "omitted_file_count": 0,
            "metadata_only_count": 0,
            "compressed_file_count": 0,
            "error_file_count": 0,
            "truncated_file_count": 0,
            "input_chars": 100,
            "output_chars": 50,
            "compression_ratio": 0.5,
        },
    }

    markdown = render_markdown_report(payload)

    assert "# Context Scheduler Benchmark Report" in markdown
    assert "## Context Scheduler Summary" in markdown
    assert "compression_ratio" in markdown
```

- [ ] **Step 2: Run benchmark tests and verify script missing**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_context_scheduler.py -q
```

Expected: FAIL because `scripts/benchmark_context_scheduler.py` does not exist.

- [ ] **Step 3: Implement benchmark script**

Create `scripts/benchmark_context_scheduler.py`:

```python
"""运行真实 ContextScheduler 链路并输出压缩证据。"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Sequence

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from src.services.context_compressor import CompressedContext  # noqa: E402
from src.services.context_scheduler import (  # noqa: E402
    ContextScheduleRequest,
    ContextScheduler,
)
from src.services.file_scanner import FileScanner  # noqa: E402


def _parse_date(value: str) -> date:
    return date.fromisoformat(value)


def build_context_scheduler_summary(
    compressed_context: CompressedContext,
) -> dict[str, int | float]:
    """汇总 context scheduler 压缩结果，供 JSON 和 Markdown 共用。"""
    return compressed_context.to_summary()


def build_benchmark_payload(
    *,
    compressed_context: CompressedContext,
    context_run: dict[str, Any] | None,
    parameters: dict[str, Any],
    scan_run: dict[str, Any] | None,
) -> dict[str, Any]:
    """组合 benchmark 输出结构。"""
    return {
        "parameters": parameters,
        "scan_run": scan_run or {},
        "context_run": context_run or {},
        "context_scheduler_summary": build_context_scheduler_summary(compressed_context),
    }


def render_markdown_report(payload: dict[str, Any]) -> str:
    """把 context benchmark payload 渲染成 Markdown。"""
    parameters = payload["parameters"]
    summary = payload["context_scheduler_summary"]
    scan_run = payload.get("scan_run", {})
    context_run = payload.get("context_run", {})
    lines = [
        "# Context Scheduler Benchmark Report",
        "",
        "## Parameters",
        "",
        f"- start_date: `{parameters['start_date']}`",
        f"- end_date: `{parameters['end_date']}`",
        f"- report_mode: `{parameters['report_mode']}`",
        f"- compression_profile: `{parameters['compression_profile']}`",
        "",
        "## Scan Run",
        "",
        f"- run_id: `{scan_run.get('run_id', '')}`",
        f"- discovered_count: `{scan_run.get('discovered_count', 0)}`",
        f"- reused_count: `{scan_run.get('reused_count', 0)}`",
        f"- reparsed_count: `{scan_run.get('reparsed_count', 0)}`",
        "",
        "## Context Run",
        "",
        f"- context_run_id: `{context_run.get('context_run_id', '')}`",
        f"- status: `{context_run.get('status', '')}`",
        "",
        "## Context Scheduler Summary",
        "",
        "| metric | value |",
        "|---|---:|",
    ]
    for key in sorted(summary):
        lines.append(f"| {key} | {summary[key]} |")
    return "\n".join(lines)


def write_report_files(
    payload: dict[str, Any],
    *,
    json_out: Path | None,
    markdown_out: Path | None,
) -> None:
    """按需写出 JSON / Markdown benchmark 产物。"""
    if json_out:
        json_out.parent.mkdir(parents=True, exist_ok=True)
        json_out.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
    if markdown_out:
        markdown_out.parent.mkdir(parents=True, exist_ok=True)
        markdown_out.write_text(render_markdown_report(payload), encoding="utf-8")


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Benchmark ai_daily_report context scheduler")
    default_end_date = date.today()
    default_start_date = default_end_date - timedelta(days=1)
    parser.add_argument("--start-date", type=_parse_date, default=default_start_date)
    parser.add_argument("--end-date", type=_parse_date, default=default_end_date)
    parser.add_argument(
        "--report-mode",
        choices=["daily", "weekly", "monthly"],
        default="weekly",
    )
    parser.add_argument("--compression-profile", default=None)
    parser.add_argument("--json-out", type=Path, default=None)
    parser.add_argument("--markdown-out", type=Path, default=None)
    return parser


def run_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    """运行真实 ContextScheduler，并读取本轮落库指标生成 payload。"""
    scheduler = ContextScheduler()
    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode=args.report_mode,
            source="scan",
            start_date=args.start_date,
            end_date=args.end_date,
            compression_profile=args.compression_profile,
        )
    )
    # benchmark 只需要读取刚刚落库的 latest run，不重新触发 scan。
    store = FileScanner().scan_index_store
    context_run = store.latest_context_run()
    scan_run = store.latest_scan_run_detail()
    compression_profile = (
        str(context_run.get("compression_profile"))
        if context_run
        else args.compression_profile or f"{args.report_mode}_balanced_v1"
    )
    return build_benchmark_payload(
        compressed_context=result.compressed_context,
        context_run=context_run,
        scan_run=scan_run,
        parameters={
            "start_date": args.start_date.isoformat(),
            "end_date": args.end_date.isoformat(),
            "report_mode": args.report_mode,
            "compression_profile": compression_profile,
        },
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    payload = run_benchmark(args)
    write_report_files(
        payload,
        json_out=args.json_out,
        markdown_out=args.markdown_out,
    )
    print(json.dumps(payload, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 4: Run benchmark tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_context_scheduler.py -q
```

Expected: PASS.

- [ ] **Step 5: Run a real context benchmark smoke test**

Run:

```powershell
conda run -n test python scripts\benchmark_context_scheduler.py --start-date 2026-05-09 --end-date 2026-05-24 --report-mode weekly --json-out data\benchmarks\context_scheduler_2026-05-24.json --markdown-out data\benchmarks\context_scheduler_2026-05-24.md
```

Expected:

- Command exits `0`.
- JSON includes `context_scheduler_summary`.
- Markdown includes `# Context Scheduler Benchmark Report`.

- [ ] **Step 6: Commit Task 5**

Run:

```powershell
git add scripts/benchmark_context_scheduler.py tests/test_benchmark_context_scheduler.py
git commit -m "Add context scheduler benchmark"
```

---

## Task 6: Documentation And Spec Alignment

**Files:**
- Modify: `docs/superpowers/specs/2026-05-24-context-scheduler-design.md`
- Modify: `AGENTS.md` only if implementation reveals naming conventions need one more precise line

- [ ] **Step 1: Review implementation against spec**

Run:

```powershell
rg -n "context_scheduler|context_compressor|context_runs|context_decisions|context_profile" docs\superpowers\specs\2026-05-24-context-scheduler-design.md src tests scripts AGENTS.md
```

Expected:

- Spec mentions the designed concepts.
- `src/services/context_scheduler.py` and `src/services/context_compressor.py` exist.
- Store and benchmark tests mention `context_runs` / `context_decisions`.

- [ ] **Step 2: Add implementation note to spec if names differ from design**

If implementation uses the names in this plan, no edit is required. If a name differs, update the spec with a short section:

```markdown
## Implementation Notes

- 第一版实现使用 `ContextProfile` 表示 context profile。
- 第一版实现把 `ContextDecision` 放在 `context_compressor.py`，因为 compressor 和 store 都需要同一个决策模型。
- 第一版暂不实现 `context_cache`，只落库 `context_runs` 与 `context_decisions`。
```

- [ ] **Step 3: Run doc diff check**

Run:

```powershell
git diff --check -- docs\superpowers\specs\2026-05-24-context-scheduler-design.md AGENTS.md
```

Expected: no whitespace errors.

- [ ] **Step 4: Commit doc alignment if there are doc changes**

If Step 2 changed docs, run:

```powershell
git add docs\superpowers\specs\2026-05-24-context-scheduler-design.md AGENTS.md
git commit -m "Document context scheduler implementation notes"
```

If Step 2 did not change docs, skip this commit.

---

## Task 7: Final Verification And Benchmark Evidence

**Files:**
- No code edits expected.

- [ ] **Step 1: Run focused tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_context_compressor.py tests/test_context_scheduler.py tests/test_scan_index_store.py tests/test_main.py tests/test_benchmark_context_scheduler.py -q
```

Expected: PASS.

- [ ] **Step 2: Run full test suite**

Run:

```powershell
conda run -n test python -m pytest tests -q
```

Expected: PASS.

- [ ] **Step 3: Run compileall**

Run:

```powershell
conda run -n test python -m compileall main.py src tests scripts
```

Expected: exits `0`, no syntax errors.

- [ ] **Step 4: Re-run scanner benchmark to confirm parser summary still works**

Run:

```powershell
conda run -n test python scripts\benchmark_scanner.py --start-date 2026-05-09 --end-date 2026-05-24 --json-out data\benchmarks\scanner_benchmark_2026-05-24_after_context_scheduler.json --markdown-out data\benchmarks\scanner_benchmark_2026-05-24_after_context_scheduler.md
```

Expected:

- JSON includes `parser_backend_summary`.
- Existing `office_v1` / `light_text_v1` behavior is still visible when matching files exist.
- `timeout_count` is not unexpectedly increased.

- [ ] **Step 5: Run context scheduler benchmark**

Run:

```powershell
conda run -n test python scripts\benchmark_context_scheduler.py --start-date 2026-05-09 --end-date 2026-05-24 --report-mode weekly --json-out data\benchmarks\context_scheduler_2026-05-24_after_context_scheduler.json --markdown-out data\benchmarks\context_scheduler_2026-05-24_after_context_scheduler.md
```

Expected:

- JSON includes `context_scheduler_summary`.
- Markdown includes `## Context Scheduler Summary`.
- `output_chars` is less than or equal to `global_context_max_chars` plus fixed header/summary overhead.
- `context_run.status` is `success`.

- [ ] **Step 6: Check git status**

Run:

```powershell
git status --short
```

Expected:

- Source/test/script changes are committed.
- `data/benchmarks/context_scheduler_*.json` and matching Markdown files may be untracked or ignored depending on repository ignore rules.
- User-owned `config/settings.toml` may remain modified and must not be reverted.

- [ ] **Step 7: Final report to user**

Report:

- Commits created.
- Tests and benchmark commands run.
- Scanner benchmark parser summary result.
- Context scheduler benchmark summary result.
- Any files intentionally left uncommitted, especially `config/settings.toml` if still modified.
