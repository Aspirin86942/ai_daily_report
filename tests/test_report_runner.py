"""ReportRunner.run 公共 pipeline 的行为测试。"""

from __future__ import annotations

from datetime import date
from pathlib import Path

from src.models.scanner_contract import ContextSummary, Diagnostic
from src.models.schemas import DailyReportData
from src.services.context_engine import ContextBuildResult
from src.services.report_runner.outcomes import (
    ErrorCode,
    ReportRunFailure,
    ReportRunSuccess,
)
from src.services.report_runner.requests import DailyReportRunRequest


class FakeScheduler:
    def __init__(self, result: ContextBuildResult) -> None:
        self._result = result
        self.calls: list[object] = []

    def build_context(self, request: object) -> ContextBuildResult:
        self.calls.append(request)
        return self._result


class FakeStore:
    def __init__(self, events: list[str] | None = None) -> None:
        self.events = events if events is not None else []
        self.saved: list[tuple[str, object]] = []

    def get_yesterday_plan(self, target_date=None) -> str:
        self.events.append("read_yesterday_plan")
        return "昨日计划"

    def save_report(self, report: DailyReportData) -> None:
        self.events.append("save_sqlite")
        self.saved.append(("daily", report))

    def save_weekly_report(self, report) -> None:
        self.events.append("save_sqlite")
        self.saved.append(("weekly", report))

    def save_monthly_report(self, report) -> None:
        self.events.append("save_sqlite")
        self.saved.append(("monthly", report))


class FakeRenderer:
    def __init__(self, events: list[str] | None = None) -> None:
        self.events = events if events is not None else []
        self.written: list[Path] = []

    def render_markdown(self, report: DailyReportData) -> str:
        self.events.append("render")
        return f"markdown:{report.date}"

    def render_weekly_markdown(self, report) -> str:
        self.events.append("render")
        return "weekly-md"

    def render_monthly_markdown(self, report) -> str:
        self.events.append("render")
        return "monthly-md"

    def save_markdown(self, content: str, report_date: str) -> Path:
        self.events.append("save_markdown")
        path = Path(f"{report_date}.md")
        self.written.append(path)
        return path

    def save_weekly_markdown(self, content: str, year: int, week: int) -> Path:
        self.events.append("save_markdown")
        path = Path(f"{year}-W{week:02d}.md")
        self.written.append(path)
        return path

    def save_monthly_markdown(self, content: str, year_month: str) -> Path:
        self.events.append("save_markdown")
        path = Path(f"{year_month}.md")
        self.written.append(path)
        return path


class RecordingModelPort:
    def __init__(self, report: DailyReportData | None = None) -> None:
        self._report = report or DailyReportData(
            date="2026-05-25",
            completed_work="c",
            work_summary="w",
            next_plan="n",
        )
        self.calls: list[object] = []

    def generate(self, request: object) -> DailyReportData:
        self.calls.append(request)
        return self._report


class FixedInputAdapter:
    def __init__(self, value: str = "今天工作") -> None:
        self.value = value
        self.calls = 0

    def read(self) -> str:
        self.calls += 1
        return self.value


def _context(status: str = "ok") -> ContextBuildResult:
    diagnostic = Diagnostic(
        error_code="RUST_CORE_CRASHED" if status == "error" else "PARSER_FAILED",
        message="synthetic scanner diagnostic",
        retryable=False,
        stage="process" if status == "error" else "parse",
        file_path=None,
        backend=None,
    )
    return ContextBuildResult(
        file_context="ctx" if status != "error" else "",
        status=status,
        summary=ContextSummary(
            source_file_count=1,
            success_count=0 if status == "error" else 1,
            timeout_count=0,
            included_file_count=0 if status == "error" else 1,
            omitted_file_count=0,
            error_file_count=1 if status == "error" else 0,
            input_chars=0,
            output_chars=0,
            total_duration_ms=1,
            discovery_duration_ms=0,
            parse_duration_ms=0,
            compression_duration_ms=0,
        ),
        scan_run_id=None if status == "error" else 1,
        context_run_id=None if status == "error" else 1,
        warnings=[diagnostic] if status == "partial" else [],
        error=diagnostic if status == "error" else None,
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
    events: list[str] = []
    runner = _make_runner(
        store=FakeStore(events),
        renderer=FakeRenderer(events),
    )

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=True,
            user_input="今天工作",
        )
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.status == "ok"
    assert outcome.markdown == "markdown:2026-05-25"
    assert outcome.publication.sqlite_state == "committed"
    assert outcome.publication.markdown_state == "written"
    assert events == [
        "read_yesterday_plan",
        "render",
        "save_sqlite",
        "save_markdown",
    ]


def test_daily_no_save_skips_publication():
    store = FakeStore()
    renderer = FakeRenderer()
    runner = _make_runner(store=store, renderer=renderer)

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=False,
            user_input="今天工作",
        )
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.publication.requested is False
    assert outcome.publication.sqlite_state == "not_attempted"
    assert outcome.publication.markdown_state == "not_attempted"
    assert outcome.markdown != ""
    assert store.saved == []
    assert renderer.written == []


def test_daily_scanner_error_fails_before_input_and_llm():
    model_port = RecordingModelPort()
    daily_input = FixedInputAdapter()
    runner = _make_runner(
        scheduler=FakeScheduler(_context(status="error")),
        model_port=model_port,
        daily_input=daily_input,
    )

    outcome = runner.run(
        DailyReportRunRequest(as_of_date=date(2026, 5, 25), save=False)
    )

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.phase == "source"
    assert outcome.error.error_code is ErrorCode.SCANNER_FAILED
    assert model_port.calls == []
    assert daily_input.calls == 0


def test_daily_empty_input_fails_before_llm():
    model_port = RecordingModelPort()
    runner = _make_runner(
        model_port=model_port,
        daily_input=FixedInputAdapter(value="   "),
    )

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=False,
            user_input=None,
        )
    )

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.error.error_code is ErrorCode.EMPTY_DAILY_INPUT
    assert model_port.calls == []
