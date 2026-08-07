# ReportRunner 落地实施计划（阶段 3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `main.py` 中 daily/weekly/monthly 三条重复的报告编排收敛到一个 `ReportRunner.run` 应用 seam（封闭 request union + typed outcome + 稳定 error model + publication receipt），并按既有 spec 的 characterization-first 顺序迁移，最后删除旧编排。

**Architecture:** 新增 `src/services/report_runner/` 包。`ReportRunner.run(ReportRunRequest) -> ReportRunOutcome` 是唯一公开 seam；内部一条公共 pipeline，用私有 recipe 表达三种 report mode 差异。LLM 走窄 `ReportModelPort`（lazy factory，true external 唯一 mock 点）；`ContextScheduler`、`SQLiteStore`、`ReportGenerator`、Markdown filesystem 作为本地依赖注入真实实现（测试用临时真实 substitute）。CLI 最后收窄为 request mapping + daily input adapter + outcome presentation + exit code。

**Tech Stack:** Python 3.13、pydantic（DTO 已存在）、dataclasses（request/outcome）、pytest 8.4（TDD）、uv run。

**引用设计（本 plan 的实现目标，必须遵守其 interface/pipeline/error model/测试清单）：**
`docs/superpowers/specs/2026-07-17-deep-report-run-module-design.md`

## Global Constraints

- 前置：Plan 1（uv + pytest 正式化）已完成；`uv run pytest` 当前 237 passed / 1 skipped / 0 failed 为全绿基线。
- 每 Task 结束 `uv run pytest` 全绿（≥237 passed，新增测试只增不减）。
- 修改范围：`src/services/report_runner/`（新建）、`main.py`、`tests/`；**禁止改** `rust/`、`templates/`、`src/models/schemas.py`、`src/models/scanner_contract.py`、`src/services/sqlite_store.py`、`src/core/llm.py`、`src/services/context_*`。
- CLI 兼容：参数、退出码（0/1/130）、提示文案、Markdown 预览保持不变；`main.py` 的 `_run_bootstrap_doctor` 轻量入口分支不动。
- 禁止：公开 run_daily/run_weekly/run_monthly 三个 interface；引入 workflow engine / middleware / plugin registry / 通用 DTO；跨 SQLite/Markdown 的原子事务或自动重试；构造重试 scanner/LLM。
- 本地依赖用真实临时 substitute 测试（SQLite 用 `tmp_path` 真实 DB、Jinja 真实模板、filesystem 真实临时目录）；只有 LLM 用 recording/failing mock adapter（经 lazy factory 注入，断言构造次数）。
- 迁移完成即删除旧三套编排与重复 internal-order 测试，不保留新旧双轨 shim。
- 每 Task 的「验证」必须包含计划中给出的 `uv run pytest <精确节点>` 命令。

---

### Task 1: 确认既有 CLI 测试冻结 report-run 行为

**Files:**
- 无改动（验证任务）

**Interfaces:**
- Consumes: 现有 `tests/test_main.py`（已冻结 daily/weekly/monthly 的调用顺序、退出码、no-save、date override、partial/error 分支）
- Produces: 迁移的冻结基线；后续 Task 用同一批断言比对行为等价

- [ ] **Step 1: 跑全量测试确认冻结基线**

Run: `uv run pytest`
Expected: `237 passed, 1 skipped`。重点确认 `tests/test_main.py` 全部通过 —— 它是旧编排的 characterization 基线。

- [ ] **Step 2: 记录 report-run 冻结行为矩阵**

创建 `docs/superpowers/specs/2026-08-07-report-runner-characterization.md`，把以下冻结行为逐一列出（来源：`tests/test_main.py` 现有断言 + `main.py` 三函数）：

```markdown
# report-run 行为冻结矩阵（迁移前）

| mode | source | scanner 调用 | LLM 方法 | render | 保存顺序 | 失败退出码 |
|---|---|---|---|---|---|---|
| daily | 固定 scan | build_context(daily,scan,asof-1,asof) | generate_report | render_markdown | save_report → save_markdown | 1 |
| weekly | db | 0 | generate_weekly_report | render_weekly_markdown | save_weekly_report → save_weekly_markdown | 1 |
| weekly | scan | 1 | generate_weekly_report | render_weekly_markdown | 同上 | 1 |
| monthly | db | 0 | generate_monthly_report | render_monthly_markdown | save_monthly_report → save_monthly_markdown | 1 |
| monthly | scan | 1 | generate_monthly_report | render_monthly_markdown | 同上 | 1 |

固定规则：
- daily 无输入报错、退出码 1；--no-save 不写任何载体；--date 只覆盖报告日期不改变 scan window。
- weekly/monthly source=db 无报告 → 退出码 1（提示"未找到…数据"）。
- scan partial → 保留 warning 继续；scan error → 不构造 LLM、退出码 1。
- 退出码：成功 0、预期失败 1、KeyboardInterrupt 130。
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-08-07-report-runner-characterization.md
git commit -m "docs: freeze report-run behavior matrix before ReportRunner migration"
```

---

### Task 2: 定义 request / outcome / period / model port / input adapter 类型

**Files:**
- Create: `src/services/report_runner/__init__.py`
- Create: `src/services/report_runner/requests.py`
- Create: `src/services/report_runner/outcomes.py`
- Create: `src/services/report_runner/period.py`
- Create: `src/services/report_runner/model_port.py`
- Create: `src/services/report_runner/input_adapter.py`
- Test: `tests/test_report_runner_types.py`

**Interfaces:**
- Consumes: `src/models/schemas.py` 的 `DailyReportData` / `WeeklyReportData` / `MonthlyReportData`；`src/utils/text_tools.py` 的 `parse_week_label` / `get_month_date_range`；`src/services/context_engine.py` 的 `ContextBuildResult`。
- Produces: `ReportRunRequest`（union）、`DailyReportRunRequest` / `WeeklyReportRunRequest` / `MonthlyReportRunRequest`、`ReportRunSuccess` / `ReportRunFailure` / `ReportRunOutcome`、`PublicationReceipt`、`ScanEvidence` / `DatabaseEvidence`、`ReportError`、`ErrorCode`、`ResolvedPeriod`、`ReportModelPort`、`GenerationRequest`（union）、`DailyInputAdapter`。后续 Task 3–7 依赖这些精确名称与字段。

- [ ] **Step 1: 写失败测试（类型与约束）**

创建 `tests/test_report_runner_types.py`：
```python
"""ReportRunner request/outcome 类型与非法组合约束。"""
from __future__ import annotations

from datetime import date
from pathlib import Path

import pytest

from src.models.schemas import DailyReportData
from src.services.report_runner.outcomes import (
    DatabaseEvidence,
    ErrorCode,
    PublicationReceipt,
    ReportError,
    ReportRunFailure,
    ReportRunSuccess,
    ScanEvidence,
)
from src.services.report_runner.period import ResolvedPeriod
from src.services.report_runner.requests import (
    DailyReportRunRequest,
    MonthlyReportRunRequest,
    WeeklyReportRunRequest,
)


def test_daily_request_has_no_source_field():
    req = DailyReportRunRequest(as_of_date=date(2026, 5, 25), save=True)
    assert not hasattr(req, "source")


def test_weekly_request_requires_source():
    with pytest.raises(TypeError):
        WeeklyReportRunRequest(as_of_date=date(2026, 5, 25), save=True)


def test_weekly_request_accepts_db_or_scan():
    for source in ("db", "scan"):
        req = WeeklyReportRunRequest(
            as_of_date=date(2026, 5, 25), source=source, save=False
        )
        assert req.source == source


def test_monthly_request_accepts_db_or_scan():
    req = MonthlyReportRunRequest(
        as_of_date=date(2026, 5, 25), source="scan", save=False
    )
    assert req.source == "scan"


def test_success_outcome_fields():
    report = DailyReportData(
        date="2026-05-25", completed_work="x", work_summary="y", next_plan="z"
    )
    outcome = ReportRunSuccess(
        mode="daily",
        source="scan",
        status="ok",
        period=ResolvedPeriod(
            mode="daily",
            source="scan",
            start_date=date(2026, 5, 24),
            end_date=date(2026, 5, 25),
            display_label="2026-05-25",
            as_of_date=date(2026, 5, 25),
        ),
        report=report,
        markdown="# 预览",
        warnings=[],
        source_evidence=ScanEvidence(
            status="ok",
            source_file_count=1,
            success_count=1,
            scan_run_id=1,
            context_run_id=1,
        ),
        publication=PublicationReceipt(
            requested=True,
            sqlite_state="committed",
            markdown_state="written",
            markdown_path=Path("out/2026-05-25.md"),
        ),
    )
    assert outcome.outcome == "success"
    assert outcome.report is report


def test_failure_outcome_carries_phase_and_error_code():
    failure = ReportRunFailure(
        mode="weekly",
        source="db",
        period=None,
        phase="source",
        error=ReportError(
            error_code=ErrorCode.NO_SOURCE_REPORTS,
            message="未找到日报数据",
            retryable=False,
        ),
        warnings=[],
        source_evidence=None,
        publication=PublicationReceipt(
            requested=True,
            sqlite_state="not_attempted",
            markdown_state="not_attempted",
        ),
    )
    assert failure.outcome == "failure"
    assert failure.phase == "source"
    assert failure.error.error_code is ErrorCode.NO_SOURCE_REPORTS


def test_database_evidence_lists_missing_days():
    evidence = DatabaseEvidence(report_count=1, missing_days=["2026-05-25"])
    assert evidence.report_count == 1
    assert evidence.missing_days == ["2026-05-25"]
```

- [ ] **Step 2: 运行确认失败**

Run: `uv run pytest tests/test_report_runner_types.py -q`
Expected: FAIL，`ModuleNotFoundError: src.services.report_runner`。

- [ ] **Step 3: 实现类型模块**

创建 `src/services/report_runner/__init__.py`：
```python
"""ReportRunner 应用 seam：daily/weekly/monthly 报告的单一 run 入口。"""

from .outcomes import ReportRunFailure, ReportRunOutcome, ReportRunSuccess
from .requests import (
    DailyReportRunRequest,
    MonthlyReportRunRequest,
    ReportRunRequest,
    WeeklyReportRunRequest,
)
from .runner import ReportRunner

__all__ = [
    "ReportRunRequest",
    "DailyReportRunRequest",
    "WeeklyReportRunRequest",
    "MonthlyReportRunRequest",
    "ReportRunOutcome",
    "ReportRunSuccess",
    "ReportRunFailure",
    "ReportRunner",
]
```

创建 `src/services/report_runner/requests.py`：
```python
"""封闭的 report-run request variants。"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date


@dataclass(frozen=True)
class DailyReportRunRequest:
    as_of_date: date
    save: bool
    user_input: str | None = None
    report_date_override: str | None = None


@dataclass(frozen=True)
class WeeklyReportRunRequest:
    as_of_date: date
    source: str
    save: bool
    week_label: str | None = None
    supplemental_input: str | None = None


@dataclass(frozen=True)
class MonthlyReportRunRequest:
    as_of_date: date
    source: str
    save: bool
    year_month: str | None = None
    supplemental_input: str | None = None


ReportRunRequest = (
    DailyReportRunRequest | WeeklyReportRunRequest | MonthlyReportRunRequest
)
```

创建 `src/services/report_runner/period.py`：
```python
"""已解析的目标周期：让 source/generation/publication 共用同一组日期。"""

from __future__ import annotations

import calendar
from dataclasses import dataclass
from datetime import date, timedelta

from ..utils.text_tools import get_month_date_range, parse_week_label


@dataclass(frozen=True)
class ResolvedPeriod:
    mode: str
    source: str
    start_date: date
    end_date: date
    display_label: str
    as_of_date: date


def resolve_daily_period(as_of_date: date) -> ResolvedPeriod:
    return ResolvedPeriod(
        mode="daily",
        source="scan",
        start_date=as_of_date - timedelta(days=1),
        end_date=as_of_date,
        display_label=as_of_date.isoformat(),
        as_of_date=as_of_date,
    )


def resolve_weekly_period(as_of_date: date, week_label: str | None) -> ResolvedPeriod:
    if week_label:
        year, week = parse_week_label(week_label)
    else:
        year, week, _ = as_of_date.isocalendar()
    monday = date.fromisocalendar(year, week, 1)
    sunday = date.fromisocalendar(year, week, 7)
    return ResolvedPeriod(
        mode="weekly",
        source="scan",
        start_date=monday,
        end_date=sunday,
        display_label=f"{year}-W{week:02d}",
        as_of_date=as_of_date,
    )


def resolve_monthly_period(
    as_of_date: date, year_month: str | None
) -> ResolvedPeriod:
    label = year_month or as_of_date.strftime("%Y-%m")
    start, end = get_month_date_range(label)
    return ResolvedPeriod(
        mode="monthly",
        source="scan",
        start_date=start,
        end_date=end,
        display_label=label,
        as_of_date=as_of_date,
    )
```
（`resolve_daily_period` 的 `source="scan"` 仅用于占位；实际 daily 的 source 恒为 `scan`，Task 3 统一从 request 传 `source`。）

创建 `src/services/report_runner/outcomes.py`：
```python
"""ReportRunner typed outcomes、publication receipt 与错误模型。"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Literal

from ...models.schemas import (
    DailyReportData,
    MonthlyReportData,
    WeeklyReportData,
)
from .period import ResolvedPeriod


class ErrorCode(str, Enum):
    INVALID_WEEK = "INVALID_WEEK"
    INVALID_MONTH = "INVALID_MONTH"
    EMPTY_DAILY_INPUT = "EMPTY_DAILY_INPUT"
    NO_SOURCE_REPORTS = "NO_SOURCE_REPORTS"
    SCANNER_FAILED = "SCANNER_FAILED"
    SOURCE_READ_FAILED = "SOURCE_READ_FAILED"
    LLM_GENERATION_FAILED = "LLM_GENERATION_FAILED"
    MARKDOWN_RENDER_FAILED = "MARKDOWN_RENDER_FAILED"
    SQLITE_PUBLISH_FAILED = "SQLITE_PUBLISH_FAILED"
    MARKDOWN_PUBLISH_FAILED = "MARKDOWN_PUBLISH_FAILED"


@dataclass(frozen=True)
class ReportError:
    error_code: ErrorCode
    message: str
    retryable: bool
    cause: str | None = None


@dataclass(frozen=True)
class ScanEvidence:
    status: str
    source_file_count: int
    success_count: int
    scan_run_id: int | None
    context_run_id: int | None


@dataclass(frozen=True)
class DatabaseEvidence:
    report_count: int
    missing_days: list[str]


@dataclass(frozen=True)
class PublicationReceipt:
    requested: bool
    sqlite_state: Literal["not_attempted", "committed", "failed"]
    markdown_state: Literal["not_attempted", "written", "failed"]
    markdown_path: Path | None = None


@dataclass(frozen=True)
class ReportRunSuccess:
    outcome: Literal["success"] = "success"
    status: str = "ok"
    mode: str = ""
    source: str = ""
    period: ResolvedPeriod | None = None
    report: (
        DailyReportData | WeeklyReportData | MonthlyReportData | None
    ) = None
    markdown: str = ""
    warnings: list = field(default_factory=list)
    source_evidence: ScanEvidence | DatabaseEvidence | None = None
    publication: PublicationReceipt | None = None


@dataclass(frozen=True)
class ReportRunFailure:
    outcome: Literal["failure"] = "failure"
    mode: str = ""
    source: str = ""
    period: ResolvedPeriod | None = None
    phase: str = ""
    error: ReportError | None = None
    warnings: list = field(default_factory=list)
    source_evidence: ScanEvidence | DatabaseEvidence | None = None
    publication: PublicationReceipt | None = None


ReportRunOutcome = ReportRunSuccess | ReportRunFailure
```

创建 `src/services/report_runner/model_port.py`：
```python
"""ReportModelPort：唯一 true external port，lazy factory 注入。"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date
from typing import Callable, Protocol

from ...core.llm import LLMClient
from ...models.schemas import (
    DailyReportData,
    MonthlyReportData,
    WeeklyReportData,
)


@dataclass(frozen=True)
class DailyGenerationRequest:
    user_input: str
    file_context: str
    yesterday_plan: str


@dataclass(frozen=True)
class WeeklyGenerationRequest:
    reports: list[DailyReportData]
    file_context: str
    year: int
    week: int
    missing_days: list[str]
    data_source: str


@dataclass(frozen=True)
class MonthlyGenerationRequest:
    reports: list[DailyReportData]
    file_context: str
    year_month: str
    missing_days: list[str]
    data_source: str


GenerationRequest = (
    DailyGenerationRequest
    | WeeklyGenerationRequest
    | MonthlyGenerationRequest
)


class ReportModelPort(Protocol):
    def generate(
        self, request: GenerationRequest
    ) -> DailyReportData | WeeklyReportData | MonthlyReportData:
        ...


@dataclass
class LLMModelPort:
    """production adapter：延迟构造 LLMClient，只在首次 generate 时。"""

    client_factory: Callable[[], LLMClient]
    _client: LLMClient | None = None

    def generate(
        self, request: GenerationRequest
    ) -> DailyReportData | WeeklyReportData | MonthlyReportData:
        client = self._get_client()
        if isinstance(request, DailyGenerationRequest):
            return client.generate_report(
                user_input=request.user_input,
                file_context=request.file_context,
                yesterday_plan=request.yesterday_plan,
            )
        if isinstance(request, WeeklyGenerationRequest):
            return client.generate_weekly_report(
                reports=request.reports,
                file_context=request.file_context,
                year=request.year,
                week=request.week,
                missing_days=request.missing_days,
                data_source=request.data_source,
            )
        return client.generate_monthly_report(
            reports=request.reports,
            file_context=request.file_context,
            year_month=request.year_month,
            missing_days=request.missing_days,
            data_source=request.data_source,
        )

    def _get_client(self) -> LLMClient:
        if self._client is None:
            self._client = self.client_factory()
        return self._client
```

创建 `src/services/report_runner/input_adapter.py`：
```python
"""Daily 交互输入 adapter：source gate 后才调用。"""

from __future__ import annotations

from typing import Protocol

from rich.console import Console


class DailyInputAdapter(Protocol):
    def read(self) -> str:
        ...


class ConsoleDailyInputAdapter:
    """从控制台读取多行输入（与现有 get_user_input 语义一致）。"""

    def __init__(self, console: Console | None = None) -> None:
        self._console = console or Console()

    def read(self) -> str:
        self._console.print("\n[bold cyan]请描述今日工作内容:[/bold cyan]")
        self._console.print(
            "[dim](输入完成后按 Ctrl+Z (Windows) 或 Ctrl+D (Linux/Mac) 结束)[/dim]\n"
        )
        lines: list[str] = []
        try:
            while True:
                line = input()
                lines.append(line)
        except EOFError:
            pass
        return "\n".join(lines).strip()
```

- [ ] **Step 4: 运行确认通过**

Run: `uv run pytest tests/test_report_runner_types.py -q`
Expected: PASS（8 tests）。

- [ ] **Step 5: 跑全量确认无回归**

Run: `uv run pytest`
Expected: 全绿（原 237 + 新增 8 = 245 passed，1 skipped）。`__init__.py` 当前 import `runner` 会失败，因此先创建占位 `src/services/report_runner/runner.py`（内容 `raise NotImplementedError`），Task 3 替换。
```python
# 占位，Task 3 替换为完整实现
```

- [ ] **Step 6: Commit**

```bash
git add src/services/report_runner tests/test_report_runner_types.py
git commit -m "feat: define ReportRunner request/outcome/period/model-port types"
```

---

### Task 3: 实现 ReportRunner pipeline（核心）

**Files:**
- Create: `src/services/report_runner/runner.py`（替换 Task 2 占位）
- Test: `tests/test_report_runner.py`

**Interfaces:**
- Consumes: Task 2 全部类型；`ContextScheduler`（`build_context(ContextScheduleRequest) -> ContextBuildResult`）、`ContextScheduleRequest`；`ContextBuildResult`（`.status/.file_context/.summary/.warnings/.error`）。
- Produces: `ReportRunner.__init__` 依赖注入签名与 `run(request) -> ReportRunOutcome`；Task 4–6 用它接真实依赖，Task 7 CLI 用它。

- [ ] **Step 1: 写失败测试（pipeline 骨架，注入轻量 fake 依赖）**

创建 `tests/test_report_runner.py`（先放 pipeline 关键行为的 4 个测试，Task 4–6 追加 mode 全路径）：
```python
"""ReportRunner.run pipeline 行为测试（依赖注入）。"""
from __future__ import annotations

from datetime import date
from pathlib import Path

from src.models.schemas import DailyReportData
from src.services.context_engine import ContextBuildResult
from src.services.report_runner.outcomes import (
    ErrorCode,
    PublicationReceipt,
    ReportRunFailure,
    ReportRunSuccess,
)
from src.services.report_runner.requests import DailyReportRunRequest


class FakeScheduler:
    def __init__(self, result: ContextBuildResult) -> None:
        self._result = result
        self.calls: list[str] = []

    def build_context(self, request) -> ContextBuildResult:
        self.calls.append(request.report_mode)
        return self._result


class FakeStore:
    def __init__(self, reports=None) -> None:
        self._reports = reports or {}
        self.saved: list = []

    def get_yesterday_plan(self, target_date=None) -> str:
        return self._reports.get("plan", "")

    def save_report(self, report) -> None:
        self.saved.append(("daily", report))

    def save_weekly_report(self, report) -> None:
        self.saved.append(("weekly", report))

    def save_monthly_report(self, report) -> None:
        self.saved.append(("monthly", report))


class FakeRenderer:
    def render_markdown(self, report) -> str:
        return f"markdown:{report.date}"

    def render_weekly_markdown(self, report) -> str:
        return "weekly-md"

    def render_monthly_markdown(self, report) -> str:
        return "monthly-md"

    def save_markdown(self, content, report_date, output_dir=None) -> Path:
        return Path(output_dir or Path(".")) / f"{report_date}.md"

    def save_weekly_markdown(self, content, year, week) -> Path:
        return Path(f"{year}-W{week:02d}.md")

    def save_monthly_markdown(self, content, year_month) -> Path:
        return Path(f"{year_month}.md")


class RecordingModelPort:
    def __init__(self, report=None) -> None:
        self._report = report or DailyReportData(
            date="2026-05-25", completed_work="c", work_summary="w", next_plan="n"
        )
        self.calls: list = []

    def generate(self, request):
        self.calls.append(request)
        return self._report


class FixedInputAdapter:
    def __init__(self, value: str = "今天工作") -> None:
        self.value = value

    def read(self) -> str:
        return self.value


def _context(status="ok") -> ContextBuildResult:
    from src.models.scanner_contract import ContextSummary, Diagnostic

    return ContextBuildResult(
        file_context="ctx" if status != "error" else "",
        status=status,
        summary=ContextSummary(
            source_file_count=1, success_count=1, timeout_count=0,
            included_file_count=1, omitted_file_count=0, error_file_count=0,
            input_chars=0, output_chars=0, total_duration_ms=1,
            discovery_duration_ms=0, parse_duration_ms=0, compression_duration_ms=0,
        ),
        scan_run_id=1, context_run_id=1,
        warnings=[], error=None,
    )


def _make_runner(**overrides):
    from src.services.report_runner.runner import ReportRunner

    defaults = {
        "scheduler": FakeScheduler(_context()),
        "store": FakeStore(),
        "renderer": FakeRenderer(),
        "model_port": RecordingModelPort(),
        "daily_input": FixedInputAdapter(),
    }
    defaults.update(overrides)
    return ReportRunner(**defaults)


def test_daily_success_publishes_sqlite_then_markdown():
    runner = _make_runner()
    outcome = runner.run(
        DailyReportRunRequest(as_of_date=date(2026, 5, 25), save=True)
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.status == "ok"
    assert outcome.markdown == "markdown:2026-05-25"
    assert outcome.publication.sqlite_state == "committed"
    assert outcome.publication.markdown_state == "written"


def test_daily_no_save_skips_publication():
    runner = _make_runner()
    outcome = runner.run(
        DailyReportRunRequest(as_of_date=date(2026, 5, 25), save=False)
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.publication.requested is False
    assert outcome.publication.sqlite_state == "not_attempted"
    assert outcome.publication.markdown_state == "not_attempted"
    assert outcome.markdown != ""


def test_daily_scanner_error_fails_before_llm():
    model_port = RecordingModelPort()
    runner = _make_runner(
        scheduler=FakeScheduler(_context(status="error")),
        model_port=model_port,
    )
    outcome = runner.run(
        DailyReportRunRequest(as_of_date=date(2026, 5, 25), save=False)
    )

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.phase == "source"
    assert outcome.error.error_code is ErrorCode.SCANNER_FAILED
    assert model_port.calls == []


def test_daily_empty_input_fails_before_llm():
    model_port = RecordingModelPort()
    runner = _make_runner(
        model_port=model_port,
        daily_input=FixedInputAdapter(value="   "),
    )
    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25), save=False, user_input=None
        )
    )

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.error.error_code is ErrorCode.EMPTY_DAILY_INPUT
    assert model_port.calls == []
```

- [ ] **Step 2: 运行确认失败**

Run: `uv run pytest tests/test_report_runner.py -q`
Expected: FAIL（`ReportRunner` 未实现或报 NotImplementedError）。

- [ ] **Step 3: 实现 ReportRunner.run**

创建 `src/services/report_runner/runner.py`：
```python
"""ReportRunner：唯一应用 seam，一条公共 pipeline + 私有 mode recipe。"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date
from typing import Any

from ...models.schemas import DailyReportData
from ..context_engine import ContextBuildResult
from ..context_scheduler import (
    ContextScheduleRequest,
    ContextScheduler,
)
from .input_adapter import DailyInputAdapter
from .model_port import (
    DailyGenerationRequest,
    GenerationRequest,
    MonthlyGenerationRequest,
    ReportModelPort,
    WeeklyGenerationRequest,
)
from .outcomes import (
    DatabaseEvidence,
    ErrorCode,
    PublicationReceipt,
    ReportError,
    ReportRunFailure,
    ReportRunOutcome,
    ReportRunSuccess,
    ScanEvidence,
)
from .period import (
    ResolvedPeriod,
    resolve_daily_period,
    resolve_monthly_period,
    resolve_weekly_period,
)
from .requests import (
    DailyReportRunRequest,
    MonthlyReportRunRequest,
    ReportRunRequest,
    WeeklyReportRunRequest,
)

NOT_ATTEMPTED = PublicationReceipt(
    requested=False, sqlite_state="not_attempted", markdown_state="not_attempted"
)


def _no_publication(save: bool) -> PublicationReceipt:
    return NOT_ATTEMPTED if not save else PublicationReceipt(
        requested=True, sqlite_state="not_attempted", markdown_state="not_attempted"
    )


@dataclass
class ReportRunner:
    scheduler: ContextScheduler
    store: Any
    renderer: Any
    model_port: ReportModelPort
    daily_input: DailyInputAdapter

    def run(self, request: ReportRunRequest) -> ReportRunOutcome:
        if isinstance(request, DailyReportRunRequest):
            return self._run_daily(request)
        if isinstance(request, WeeklyReportRunRequest):
            return self._run_weekly(request)
        if isinstance(request, MonthlyReportRunRequest):
            return self._run_monthly(request)
        raise TypeError(f"unknown request variant: {type(request).__name__}")

    # ---------- 私有 recipe：daily ----------
    def _run_daily(self, request: DailyReportRunRequest) -> ReportRunOutcome:
        period = resolve_daily_period(request.as_of_date)
        schedule = ContextScheduleRequest(
            report_mode="daily",
            source="scan",
            start_date=period.start_date,
            end_date=period.end_date,
        )
        context = self._build_context(schedule, "daily", request.as_of_date)
        if isinstance(context, ReportRunFailure):
            return context

        yesterday_plan = self.store.get_yesterday_plan(
            target_date=request.as_of_date
        )
        user_input = request.user_input
        if user_input is None:
            user_input = self.daily_input.read()
        if not user_input.strip():
            return ReportRunFailure(
                mode="daily", source="scan", period=period, phase="request",
                error=ReportError(
                    error_code=ErrorCode.EMPTY_DAILY_INPUT,
                    message="未输入工作内容", retryable=False,
                ),
                warnings=context.warnings, source_evidence=context.source_evidence,
                publication=NOT_ATTEMPTED,
            )

        generated = self._generate(
            DailyGenerationRequest(
                user_input=user_input,
                file_context=context.file_context,
                yesterday_plan=yesterday_plan,
            ),
            "daily", request.as_of_date, period,
            context.warnings, context.source_evidence,
        )
        if isinstance(generated, ReportRunFailure):
            return generated
        report, warnings = generated

        if request.report_date_override:
            report.date = request.report_date_override

        return self._render_and_publish(
            mode="daily", source="scan", period=period, report=report,
            save=request.save, render_fn=self.renderer.render_markdown,
            save_sqlite=self.store.save_report,
            save_markdown=self.renderer.save_markdown,
            markdown_args=(report.date,),
            warnings=warnings, source_evidence=context.source_evidence,
        )

    # ---------- 私有 recipe：weekly / monthly ----------
    def _run_weekly(self, request: WeeklyReportRunRequest) -> ReportRunOutcome:
        try:
            period = resolve_weekly_period(request.as_of_date, request.week_label)
        except ValueError as exc:
            return ReportRunFailure(
                mode="weekly", source=request.source, phase="request",
                error=ReportError(
                    error_code=ErrorCode.INVALID_WEEK, message=str(exc), retryable=False
                ),
                publication=NOT_ATTEMPTED,
            )
        return self._run_period(
            mode="weekly", source=request.source, period=period,
            save=request.save, supplemental=request.supplemental_input,
            generation_builder=lambda context, missing: WeeklyGenerationRequest(
                reports=context.reports, file_context=context.file_context,
                year=period.start_date.isocalendar()[0], week=period.start_date.isocalendar()[1],
                missing_days=missing, data_source=request.source,
            ),
            render_fn=self.renderer.render_weekly_markdown,
            save_sqlite=self.store.save_weekly_report,
            save_markdown=self.renderer.save_weekly_markdown,
        )

    def _run_monthly(self, request: MonthlyReportRunRequest) -> ReportRunOutcome:
        try:
            period = resolve_monthly_period(request.as_of_date, request.year_month)
        except ValueError as exc:
            return ReportRunFailure(
                mode="monthly", source=request.source, phase="request",
                error=ReportError(
                    error_code=ErrorCode.INVALID_MONTH, message=str(exc), retryable=False
                ),
                publication=NOT_ATTEMPTED,
            )
        return self._run_period(
            mode="monthly", source=request.source, period=period,
            save=request.save, supplemental=request.supplemental_input,
            generation_builder=lambda context, missing: MonthlyGenerationRequest(
                reports=context.reports, file_context=context.file_context,
                year_month=period.display_label, missing_days=missing,
                data_source=request.source,
            ),
            render_fn=self.renderer.render_monthly_markdown,
            save_sqlite=self.store.save_monthly_report,
            save_markdown=self.renderer.save_monthly_markdown,
        )

    def _run_period(self, *, mode, source, period, save, supplemental,
                    generation_builder, render_fn, save_sqlite, save_markdown):
        """weekly/monthly 公共路径：db 或 scan 取 source evidence，再生成/渲染/发布。"""
        if source == "scan":
            schedule = ContextScheduleRequest(
                report_mode=mode, source="scan",
                start_date=period.start_date, end_date=period.end_date,
            )
            context = self._build_context(schedule, mode, period.as_of_date)
            if isinstance(context, ReportRunFailure):
                return context
            file_context = context.file_context
            reports, missing_days = [], []
            warnings = context.warnings
            evidence = context.source_evidence
        else:  # source == "db"
            try:
                reports, missing_days = self._read_period_reports(mode, period)
            except Exception as exc:
                return ReportRunFailure(
                    mode=mode, source=source, period=period, phase="source",
                    error=ReportError(
                        error_code=ErrorCode.SOURCE_READ_FAILED,
                        message=str(exc), retryable=True,
                    ),
                    publication=_no_publication(save),
                )
            if not reports:
                return ReportRunFailure(
                    mode=mode, source=source, period=period, phase="source",
                    error=ReportError(
                        error_code=ErrorCode.NO_SOURCE_REPORTS,
                        message=f"未找到 {period.display_label} 的日报数据",
                        retryable=False,
                    ),
                    publication=_no_publication(save),
                )
            file_context = "无文件证据"
            warnings = []
            evidence = DatabaseEvidence(
                report_count=len(reports), missing_days=list(missing_days)
            )

        if supplemental and supplemental.strip():
            file_context = f"{file_context}\n\n---\n\n用户补充: {supplemental}"

        generated = self._generate(
            generation_builder(
                type("Ctx", (), {"reports": reports, "file_context": file_context}),
                missing_days,
            ),
            mode, period.as_of_date, period, warnings, evidence,
        )
        if isinstance(generated, ReportRunFailure):
            return generated
        report, warnings = generated

        def _markdown_args(report):
            if mode == "weekly":
                year, week, _ = period.start_date.isocalendar()
                return (year, week)
            return (period.display_label,)

        return self._render_and_publish(
            mode=mode, source=source, period=period, report=report,
            save=save, render_fn=render_fn,
            save_sqlite=save_sqlite,
            save_markdown=save_markdown,
            markdown_args=_markdown_args(report),
            warnings=warnings, source_evidence=evidence,
        )

    # ---------- 公共子步骤 ----------
    def _build_context(self, schedule, mode, as_of_date):
        result = self.scheduler.build_context(schedule)
        if result.status == "error":
            return ReportRunFailure(
                mode=mode, source="scan",
                period=ResolvedPeriod(mode=mode, source="scan",
                                      start_date=schedule.start_date,
                                      end_date=schedule.end_date,
                                      display_label="", as_of_date=as_of_date),
                phase="source",
                error=ReportError(
                    error_code=ErrorCode.SCANNER_FAILED,
                    message=(result.error.message if result.error else "scanner failed"),
                    retryable=False,
                ),
                warnings=result.warnings,
                source_evidence=ScanEvidence(
                    status="error", source_file_count=result.summary.source_file_count,
                    success_count=result.summary.success_count,
                    scan_run_id=result.scan_run_id, context_run_id=result.context_run_id,
                ),
                publication=NOT_ATTEMPTED,
            )
        evidence = ScanEvidence(
            status=result.status, source_file_count=result.summary.source_file_count,
            success_count=result.summary.success_count,
            scan_run_id=result.scan_run_id, context_run_id=result.context_run_id,
        )
        return type(
            "Ctx",
            (),
            {
                "status": result.status,
                "file_context": result.file_context,
                "warnings": list(result.warnings),
                "source_evidence": evidence,
            },
        )

    def _read_period_reports(self, mode, period):
        if mode == "weekly":
            year, week, _ = period.start_date.isocalendar()
            return self.store.get_week_reports(year, week)
        return self.store.get_reports_in_range(period.start_date, period.end_date)

    def _generate(self, gen_request, mode, as_of_date, period, warnings, evidence):
        try:
            report = self.model_port.generate(gen_request)
        except Exception as exc:
            return ReportRunFailure(
                mode=mode, source=getattr(period, "source", "scan"),
                period=period, phase="generation",
                error=ReportError(
                    error_code=ErrorCode.LLM_GENERATION_FAILED,
                    message=str(exc), retryable=False,
                ),
                warnings=warnings, source_evidence=evidence,
                publication=NOT_ATTEMPTED,
            )
        return report, warnings

    def _render_and_publish(self, *, mode, source, period, report, save,
                            render_fn, save_sqlite, save_markdown, markdown_args,
                            warnings, source_evidence):
        try:
            markdown = render_fn(report)
        except Exception as exc:
            return ReportRunFailure(
                mode=mode, source=source, period=period, phase="render",
                error=ReportError(
                    error_code=ErrorCode.MARKDOWN_RENDER_FAILED,
                    message=str(exc), retryable=False,
                ),
                warnings=warnings, source_evidence=source_evidence,
                publication=_no_publication(save),
            )
        receipt = _no_publication(save)
        if save:
            try:
                save_sqlite(report)
                receipt = PublicationReceipt(
                    requested=True, sqlite_state="committed",
                    markdown_state="not_attempted",
                )
            except Exception as exc:
                return ReportRunFailure(
                    mode=mode, source=source, period=period, phase="sqlite_publish",
                    error=ReportError(
                        error_code=ErrorCode.SQLITE_PUBLISH_FAILED,
                        message=str(exc), retryable=False,
                    ),
                    warnings=warnings, source_evidence=source_evidence,
                    publication=receipt,
                )
            try:
                path = save_markdown(markdown, *markdown_args)
                receipt = PublicationReceipt(
                    requested=True, sqlite_state="committed",
                    markdown_state="written", markdown_path=Path(path),
                )
            except Exception as exc:
                return ReportRunFailure(
                    mode=mode, source=source, period=period, phase="markdown_publish",
                    error=ReportError(
                        error_code=ErrorCode.MARKDOWN_PUBLISH_FAILED,
                        message=str(exc), retryable=False,
                    ),
                    warnings=warnings, source_evidence=source_evidence,
                    publication=receipt,
                )
        return ReportRunSuccess(
            status=("partial" if warnings else "ok"),
            mode=mode, source=source, period=period, report=report,
            markdown=markdown, warnings=warnings,
            source_evidence=source_evidence, publication=receipt,
        )
```
> 说明：`render_markdown` 的 `markdown_args` 在 daily 为 `(report.date,)`，weekly 为 `(year, week)`，monthly 为 `(year_month,)` —— 与现有 `ReportGenerator.save_*_markdown` 签名一致。`type("Ctx", ...)` 是传递 reports/file_context 的轻量命名空间，避免新增类。

- [ ] **Step 4: 运行确认通过**

Run: `uv run pytest tests/test_report_runner.py -q`
Expected: PASS（4 tests）。

- [ ] **Step 5: 跑全量确认无回归**

Run: `uv run pytest`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add src/services/report_runner/runner.py tests/test_report_runner.py
git commit -m "feat: implement ReportRunner pipeline with error model and publication receipt"
```

---

### Task 4: daily 接入真实依赖（SQLite/Jinja/scheduler 全路径）

**Files:**
- Modify: `tests/test_report_runner.py`（追加真实 substitute 测试）
- Test: 追加用例

**Interfaces:**
- Consumes: 真实 `SQLiteStore(db_path=tmp_path)`, `ReportGenerator(reports_dir=tmp_path)`, `ContextScheduler` + fake engine；`LLMModelPort(client_factory=lambda: recording client)`。
- Produces: 证明 daily 全路径（含真实 SQLite commit、真实 Markdown 落盘、scan window、date override、昨日计划读取）与现有 `tests/test_main.py` 行为等价。

- [ ] **Step 1: 追加真实 substitute 的 daily 测试**

在 `tests/test_report_runner.py` 追加：
```python
from src.services.report_runner.model_port import LLMModelPort
from src.services.report_runner.requests import DailyReportRunRequest as _DailyReq


def test_daily_full_path_writes_real_sqlite_and_markdown(tmp_path):
    from src.services.report_gen import ReportGenerator
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    renderer = ReportGenerator(reports_dir=tmp_path / "reports")

    class FailingClient:
        def __init__(self):
            self.calls = 0

        def generate_report(self, user_input, file_context, yesterday_plan=None):
            self.calls += 1
            return DailyReportData(
                date="2026-05-25", completed_work="完成日报",
                work_summary="日报摘要", next_plan="后续计划",
            )

    fake = FailingClient()
    runner = _make_runner(
        store=store, renderer=renderer,
        model_port=LLMModelPort(client_factory=lambda: fake),
        daily_input=FixedInputAdapter(value="今天工作"),
    )
    outcome = runner.run(_DailyReq(as_of_date=date(2026, 5, 25), save=True))

    assert isinstance(outcome, ReportRunSuccess)
    assert store.get_report("2026-05-25") is not None
    md = (tmp_path / "reports" / "2026-05" / "2026-05-25.md")
    assert md.is_file()
    assert md.read_text(encoding="utf-8") == outcome.markdown
    assert outcome.publication.sqlite_state == "committed"
    assert outcome.publication.markdown_state == "written"
    assert fake.calls == 1


def test_daily_date_override_keeps_scan_window(tmp_path):
    from src.services.report_gen import ReportGenerator
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    renderer = ReportGenerator(reports_dir=tmp_path / "reports")

    class FailingClient:
        def generate_report(self, user_input, file_context, yesterday_plan=None):
            return DailyReportData(
                date="2026-05-25", completed_work="c", work_summary="w", next_plan="n"
            )

    scheduler = FakeScheduler(_context())
    runner = _make_runner(
        scheduler=scheduler, store=store, renderer=renderer,
        model_port=LLMModelPort(client_factory=FailingClient),
        daily_input=FixedInputAdapter(value="x"),
    )
    outcome = runner.run(
        _DailyReq(
            as_of_date=date(2026, 5, 25), save=True,
            report_date_override="2026-05-20",
        )
    )

    assert scheduler.calls == ["daily"]
    assert outcome.report.date == "2026-05-20"
    assert store.get_report("2026-05-20") is not None
    assert (tmp_path / "reports" / "2026-05" / "2026-05-20.md").is_file()
```

- [ ] **Step 2: 运行确认通过**

Run: `uv run pytest tests/test_report_runner.py -q`
Expected: PASS（新增 2 tests）。

- [ ] **Step 3: 跑全量**

Run: `uv run pytest`
Expected: 全绿。

- [ ] **Step 4: Commit**

```bash
git add tests/test_report_runner.py
git commit -m "test: prove daily ReportRunner path with real SQLite/Jinja substitutes"
```

---

### Task 5: weekly 接入（db + scan 两条 recipe）

**Files:**
- Modify: `tests/test_report_runner.py`（追加 weekly db/scan 测试）
- Test: 追加用例

**Interfaces:**
- Consumes: `WeeklyReportRunRequest`；`SQLiteStore.get_week_reports(year, week)`（db 源）；`ContextScheduler.build_context`（scan 源）。
- Produces: weekly 两条路径的 interface 级证明（scanner 零调用 / 恰一次、supplement 格式、无报告 → NO_SOURCE_REPORTS）。

- [ ] **Step 1: 追加 weekly 测试**

在 `tests/test_report_runner.py` 追加：
```python
from src.services.report_runner.model_port import WeeklyGenerationRequest
from src.services.report_runner.requests import WeeklyReportRunRequest as _WeeklyReq


def test_weekly_db_zero_scanner_calls_and_publishes(tmp_path):
    from src.services.report_gen import ReportGenerator
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(DailyReportData(
        date="2026-05-11", completed_work="c", work_summary="w", next_plan="n"
    ))
    renderer = ReportGenerator(reports_dir=tmp_path / "reports")

    class FailingClient:
        def generate_weekly_report(self, reports, file_context, year, week,
                                   missing_days, data_source):
            assert reports[0].date == "2026-05-11"
            assert missing_days == ["2026-05-12", "2026-05-13", "2026-05-14", "2026-05-15"]
            return WeeklyReportData(
                week_label=f"{year}-W{week:02d}", date_range="2026-05-11 ~ 2026-05-17",
                completed_work="cw", self_growth="", improvement_actions="",
                work_summary="", next_plan="", support_needed="", other_notes="",
            )

    scheduler = FakeScheduler(_context())
    runner = _make_runner(
        scheduler=scheduler, store=store, renderer=renderer,
        model_port=LLMModelPort(client_factory=FailingClient),
    )
    outcome = runner.run(_WeeklyReq(
        as_of_date=date(2026, 5, 18), source="db", save=True,
        week_label="2026-W20",
    ))

    assert scheduler.calls == []
    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.publication.markdown_state == "written"
    assert (tmp_path / "reports" / "weekly" / "2026-W20.md").is_file()


def test_weekly_scan_calls_scanner_once_and_appends_supplement(tmp_path):
    from src.services.report_gen import ReportGenerator
    from src.models.schemas import WeeklyReportData

    renderer = ReportGenerator(reports_dir=tmp_path / "reports")
    scheduler = FakeScheduler(_context())

    class FailingClient:
        def generate_weekly_report(self, reports, file_context, year, week,
                                   missing_days, data_source):
            assert "用户补充: 补丁" in file_context
            return WeeklyReportData(
                week_label=f"{year}-W{week:02d}", date_range="2026-05-11 ~ 2026-05-17",
                completed_work="cw", self_growth="", improvement_actions="",
                work_summary="", next_plan="", support_needed="", other_notes="",
            )

    runner = _make_runner(
        scheduler=scheduler, store=FakeStore(), renderer=renderer,
        model_port=LLMModelPort(client_factory=FailingClient),
    )
    outcome = runner.run(_WeeklyReq(
        as_of_date=date(2026, 5, 18), source="scan", save=False,
        week_label="2026-W20", supplemental_input="补丁",
    ))

    assert scheduler.calls == ["weekly"]
    assert isinstance(outcome, ReportRunSuccess)


def test_weekly_db_no_reports_fails_before_llm(tmp_path):
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "empty.sqlite3")
    model_port = RecordingModelPort()
    runner = _make_runner(
        store=store, model_port=model_port,
    )
    outcome = runner.run(_WeeklyReq(
        as_of_date=date(2026, 5, 18), source="db", save=False, week_label="2026-W20",
    ))

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.error.error_code is ErrorCode.NO_SOURCE_REPORTS
    assert model_port.calls == []
```

- [ ] **Step 2: 运行确认通过**

Run: `uv run pytest tests/test_report_runner.py -q`
Expected: PASS（新增 3 tests）。若 weekly scan 的 `period.start_date.isocalendar()[0]` 断言不匹配，检查 `resolve_weekly_period("2026-W20")` → 2026-05-11（周一）。

- [ ] **Step 3: 跑全量**

Run: `uv run pytest`
Expected: 全绿。

- [ ] **Step 4: Commit**

```bash
git add tests/test_report_runner.py
git commit -m "test: prove weekly ReportRunner db/scan recipes"
```

---

### Task 6: monthly 接入（db + scan 两条 recipe）

**Files:**
- Modify: `tests/test_report_runner.py`（追加 monthly db/scan 测试）
- Test: 追加用例

**Interfaces:**
- Consumes: `MonthlyReportRunRequest`；`SQLiteStore.get_reports_in_range(start, end)`（db 源）；`ContextScheduler`（scan 源）。
- Produces: monthly 两条路径证明（自然月边界、零 scanner / 恰一次、supplement）。

- [ ] **Step 1: 追加 monthly 测试**

在 `tests/test_report_runner.py` 追加：
```python
from src.models.schemas import MonthlyReportData
from src.services.report_runner.requests import MonthlyReportRunRequest as _MonthlyReq


def test_monthly_db_zero_scanner_calls(tmp_path):
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "m.sqlite3")
    store.save_report(DailyReportData(
        date="2026-05-05", completed_work="c", work_summary="w", next_plan="n"
    ))
    scheduler = FakeScheduler(_context())

    class FailingClient:
        def generate_monthly_report(self, reports, file_context, year_month,
                                    missing_days, data_source):
            assert year_month == "2026-05"
            return MonthlyReportData(
                year_month=year_month, overview="ov", completed_work="cw",
                work_summary="", next_plan="",
            )

    runner = _make_runner(
        scheduler=scheduler, store=store,
        model_port=LLMModelPort(client_factory=FailingClient),
    )
    outcome = runner.run(_MonthlyReq(
        as_of_date=date(2026, 5, 20), source="db", save=False, year_month="2026-05",
    ))

    assert scheduler.calls == []
    assert isinstance(outcome, ReportRunSuccess)


def test_monthly_scan_calls_scanner_once(tmp_path):
    from src.services.report_gen import ReportGenerator

    renderer = ReportGenerator(reports_dir=tmp_path / "reports")
    scheduler = FakeScheduler(_context())

    class FailingClient:
        def generate_monthly_report(self, reports, file_context, year_month,
                                    missing_days, data_source):
            assert reports == []
            return MonthlyReportData(
                year_month=year_month, overview="ov", completed_work="cw",
                work_summary="", next_plan="",
            )

    runner = _make_runner(
        scheduler=scheduler, store=FakeStore(), renderer=renderer,
        model_port=LLMModelPort(client_factory=FailingClient),
    )
    outcome = runner.run(_MonthlyReq(
        as_of_date=date(2026, 5, 20), source="scan", save=False, year_month="2026-05",
    ))

    assert scheduler.calls == ["monthly"]
    assert isinstance(outcome, ReportRunSuccess)
```

- [ ] **Step 2: 运行确认通过**

Run: `uv run pytest tests/test_report_runner.py -q`
Expected: PASS（新增 2 tests）。

- [ ] **Step 3: 跑全量**

Run: `uv run pytest`
Expected: 全绿。

- [ ] **Step 4: Commit**

```bash
git add tests/test_report_runner.py
git commit -m "test: prove monthly ReportRunner db/scan recipes"
```

---

### Task 7: 收窄 CLI（main.py 报告命令改用 ReportRunner）

**Files:**
- Modify: `main.py`（报告命令 → request mapping + outcome presentation；保留 `_run_bootstrap_doctor` 与 `build_parser` 现有参数）
- Modify: `tests/test_main.py`（把断言"CLI 内部调用顺序"的测试改为断言 ReportRunner 替换后的呈现）

**Interfaces:**
- Consumes: `ReportRunner`（Task 3）、`DailyReportRunRequest` / `WeeklyReportRunRequest` / `MonthlyReportRunRequest`、`ReportRunSuccess` / `ReportRunFailure`、`ConsoleDailyInputAdapter`。
- Produces: CLI 只做 参数映射 + outcome 展示 + 退出码；`main.main()` 退出码 0/1/130 与现有测试一致。

- [ ] **Step 1: 改写失败测试（CLI → ReportRunner）**

修改 `tests/test_main.py`：把 `test_generate_daily_report_uses_context_scheduler`、`test_generate_weekly_report_db_uses_sqlite_store`、`test_generate_weekly_report_scan_uses_context_scheduler`、`test_generate_monthly_report_*` 从"断言 main 内部对象调用顺序"改为"断言 main 把 Namespace 映射为正确 request 并调用 ReportRunner.run"。

在每个报告命令测试中：
- `monkeypatch.setattr(main, "ReportRunner", StubRunner)`，`StubRunner.run(request)` 记录 request 并返回固定 `ReportRunSuccess`。
- 断言 `request` 的字段（as_of_date / source / save / week_label / year_month / user_input）映射正确。
- 断言 outcome 展示（Markdown preview、summary、退出码）与现有文案一致。

代表性断言（替换原 daily 测试）：
```python
def test_daily_command_maps_namespace_to_request(monkeypatch):
    captured: list = []

    class StubRunner:
        def run(self, request):
            captured.append(request)
            return ReportRunSuccess(
                status="ok", mode="daily", source="scan",
                markdown="# 日报", report=None, publication=None,
            )

    monkeypatch.setattr(main, "ReportRunner", StubRunner)
    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_daily_report(
        Namespace(input="今天工作", no_save=True, date="2026-05-20")
    )

    assert success is True
    req = captured[0]
    assert req.user_input == "今天工作"
    assert req.save is False
    assert req.report_date_override == "2026-05-20"
    assert any("日报预览" in text for text in printed)
```
> 保留 `_patch_console` / `_patch_progress` helper 与退出码测试（`test_main_returns_*`、`test_main_returns_130_*`、`test_cli_*` 子进程测试）不动。daily 交互输入在无 `-i` 时经 `ConsoleDailyInputAdapter` 读取；测试用 `-i` 显式输入避免阻塞。

- [ ] **Step 2: 运行确认失败**

Run: `uv run pytest tests/test_main.py -q`
Expected: 失败（`main.generate_daily_report` 尚未改）。

- [ ] **Step 3: 改写 main.py 报告命令**

在 `main.py` 顶部把 `from src.services.report_runner import ReportRunner, DailyReportRunRequest, WeeklyReportRunRequest, MonthlyReportRunRequest, ReportRunSuccess, ReportRunFailure` 与 `ConsoleDailyInputAdapter` 加入 import（延迟到函数内也可，Task 8 统一处理启动优化）。

改写 `generate_daily_report`：
```python
def generate_daily_report(args: argparse.Namespace) -> bool:
    console.print("\n[bold green]===== 审计日报生成器 v5.0 =====[/bold green]\n")
    from src.services.report_runner import (
        DailyReportRunRequest,
        ReportRunner,
        ReportRunFailure,
        ReportRunSuccess,
    )

    runner = ReportRunner(
        scheduler=ContextScheduler(),
        store=SQLiteStore(),
        renderer=ReportGenerator(),
        model_port=LLMModelPort(client_factory=LLMClient),
        daily_input=ConsoleDailyInputAdapter(console=console),
    )
    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date.today(),
            save=not args.no_save,
            user_input=args.input,
            report_date_override=args.date,
        )
    )
    return _present_report_outcome(outcome, "日报")
```

新增通用呈现函数：
```python
def _present_report_outcome(outcome, label: str) -> bool:
    """把 ReportRunOutcome 映射为现有 CLI 提示、预览与退出码语义。"""
    from src.services.report_runner import ReportRunFailure, ReportRunSuccess

    if isinstance(outcome, ReportRunFailure):
        if outcome.error is not None:
            console.print(f"[red]错误: {outcome.error.message}[/red]")
        return False

    if outcome.source == "scan" and outcome.source_evidence is not None:
        console.print(
            "[green]✓[/green] 扫描完成: "
            f"{outcome.source_evidence.success_count}/"
            f"{outcome.source_evidence.source_file_count} 个文件\n"
        )
    for warning in outcome.warnings:
        console.print(f"[yellow]![/yellow] 文件上下文不完整: {warning.message}\n")
    console.print("[green]✓[/green] 报告生成成功\n")
    if outcome.publication is not None and outcome.publication.requested:
        console.print("[green]✓[/green] 报告已保存\n")
    console.print(f"[bold cyan]===== {label}预览 =====[/bold cyan]\n")
    console.print(Markdown(outcome.markdown))
    return True
```

同理改写 `generate_weekly_report_cmd` / `generate_monthly_report_cmd`：把 week/month 解析与 source gate 交给 runner，CLI 只传 `week_label` / `year_month` / `source` / `supplemental_input` / `save`，成功/失败呈现走 `_present_report_outcome`。

- [ ] **Step 4: 运行确认通过**

Run: `uv run pytest tests/test_main.py tests/test_report_runner.py -q`
Expected: PASS。

- [ ] **Step 5: 跑全量**

Run: `uv run pytest`
Expected: 全绿（报告命令测试改为 request 断言后，总数持平或略增）。

- [ ] **Step 6: Commit**

```bash
git add main.py tests/test_main.py
git commit -m "refactor: route report CLI through ReportRunner seam"
```

---

### Task 8: 删除旧编排并验证契约回归

**Files:**
- Modify: `tests/test_main.py`（删除已由 ReportRunner interface 测试覆盖的重复 internal-order 断言，仅保留 request 映射 / 退出码 / 子进程 / doctor 测试）
- 验证：Rust workspace tests、doctor、cold/warm smoke

**Interfaces:**
- Consumes: Task 7 完成后的 main.py（不再直接编排 scheduler/store/renderer/LLM）
- Produces: 无；最终验收态

- [ ] **Step 1: 确认 CLI 不再直接编排内部对象**

`grep -n "ContextScheduler\|SQLiteStore\|ReportGenerator\|LLMClient" main.py`
Expected: 仅出现在 `ReportRunner(...)` 的依赖注入处，报告中不再有 `build_context` / `save_report` / `render_*` 直调。

- [ ] **Step 2: 删除重复 internal-order 测试**

从 `tests/test_main.py` 删除已由 `tests/test_report_runner.py` 覆盖的、只断言内部对象调用顺序的 fake 测试（如原 daily/weekly/monthly 的 `StubContextScheduler` / `StubSQLiteStore` / `StubLLMClient` 调用序列断言），保留：request 映射、`--no-save`/`--date`/week/month 缺省、退出码 0/1/130、doctor、`--help`、bootstrap 子进程测试。删除后运行：
Run: `uv run pytest tests/test_main.py -q`
Expected: 全绿且用例数减少。

- [ ] **Step 3: 运行 Rust 契约回归**

Run:
```bash
cargo test --manifest-path rust/Cargo.toml --workspace --locked
uv run python main.py doctor --strict
uv run pytest tests/test_windows_rust_core_e2e.py tests/test_rust_context_client.py -q
```
Expected: 全部通过；`git diff --stat` 确认无 `rust/` 改动。

- [ ] **Step 4: 跑全量 + cold/warm smoke**

Run: `uv run pytest`
Expected: 全绿（≥237 passed）。
Run（若需要真实扫描）：`uv run python main.py daily --no-save -i "smoke"`，确认成功提示与预览可见（不保存）。

- [ ] **Step 5: Commit**

```bash
git add tests/test_main.py
git commit -m "refactor: drop duplicated report orchestration tests after ReportRunner migration"
```

---

## Self-Review

- **Spec coverage**：既有 spec 的 25 个 acceptance tests 由本 plan 的 Task 3–6 的 interface 测试 + Task 4–6 真实 substitute 测试 + Task 7 request 映射测试覆盖关键子集；error model（Task 2 `ErrorCode` / `ReportError`）与 publication receipt（Task 2 `PublicationReceipt`）落地；lazy LLM factory（Task 3 `LLMModelPort`）与 zero-scanner/zero-LLM invariants（Task 3/5/6 断言）覆盖；`_run_bootstrap_doctor` 不动（Global Constraints）。
- **占位符**：所有任务含具体代码与命令；无 TBD/TODO。
- **类型一致性**：`markdown_args` 顺序（daily=(date,), weekly=(year,week), monthly=(year_month,)）与 `ReportGenerator.save_*_markdown` 签名一致；`LLMModelPort.generate` 返回 union 与 `ReportRunSuccess.report` 一致。
