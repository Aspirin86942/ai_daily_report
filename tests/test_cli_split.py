"""src/cli 拆分后的行为等价性测试。"""

from __future__ import annotations

from argparse import Namespace
from datetime import date

import pytest

from src.cli.daily import generate_daily_report
from src.cli.common import present_report_outcome
from src.cli.parser import build_parser
from src.cli.doctor import run_doctor_cmd
from src.cli.list_reports import list_reports
from src.cli.monthly import generate_monthly_report_cmd
from src.cli.weekly import generate_weekly_report_cmd
from src.models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
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


@pytest.mark.parametrize(
    ("argv", "subcommand"),
    [
        (["daily", "-i", "x"], "daily"),
        (["daily", "--no-save"], "daily"),
        (["weekly", "2026-W05", "--source", "scan"], "weekly"),
        (["monthly", "2026-01", "--source", "db"], "monthly"),
        (["list"], "list"),
        (["doctor"], "doctor"),
        (["check-config"], "doctor"),
    ],
)
def test_parser_accepts_equivalent_commands(
    argv: list[str], subcommand: str
) -> None:
    assert build_parser().parse_args(argv).subcommand == subcommand


def test_parser_weekly_requires_source() -> None:
    with pytest.raises(SystemExit):
        build_parser().parse_args(["weekly"])


def test_parser_preserves_daily_flags() -> None:
    args = build_parser().parse_args(
        ["daily", "--no-save", "--date", "2026-02-05"]
    )

    assert args.no_save is True
    assert args.date == "2026-02-05"


def test_list_reports_shows_empty_hint() -> None:
    printed: list[str] = []
    console = type(
        "Console",
        (),
        {"print": lambda self, *args, **kwargs: printed.append(args[0])},
    )()
    store = type("Store", (), {"list_all_reports": lambda self: []})()

    list_reports(console, store=store)

    assert any("暂无日报数据" in text for text in printed)


def test_run_doctor_cmd_passes_strict() -> None:
    printed: list[str] = []
    console = type(
        "Console",
        (),
        {"print": lambda self, *args, **kwargs: printed.append(args[0])},
    )()
    seen: list[bool] = []

    def collect(*, strict: bool = False):
        seen.append(strict)
        return type(
            "Result",
            (),
            {
                "info": {"LLM Provider": "deepseek"},
                "warnings": ["w"],
                "errors": [],
            },
        )()

    ok = run_doctor_cmd(console=console, collect=collect, strict=True)

    assert ok is True
    assert seen == [True]
    assert any("所有检查通过" in text for text in printed)


def _success_outcome(mode: str = "daily", source: str = "scan") -> ReportRunSuccess:
    if mode == "daily":
        report = DailyReportData(
            date="2026-05-25",
            completed_work="日报",
            work_summary="摘要",
            next_plan="计划",
        )
        start_date = date(2026, 5, 24)
        end_date = date(2026, 5, 25)
        display_label = "2026-05-25"
    elif mode == "weekly":
        report = WeeklyReportData(
            week_label="2026-W05",
            date_range="2026-01-26 ~ 2026-02-01",
            completed_work="周报",
            self_growth="",
            improvement_actions="",
            work_summary="",
            next_plan="",
            support_needed="",
            other_notes="",
        )
        start_date = date(2026, 1, 26)
        end_date = date(2026, 2, 1)
        display_label = "2026-W05"
    else:
        report = MonthlyReportData(
            year_month="2026-05",
            overview="概览",
            completed_work="月报",
            work_summary="",
            next_plan="",
        )
        start_date = date(2026, 5, 1)
        end_date = date(2026, 5, 31)
        display_label = "2026-05"

    evidence = (
        ScanEvidence(
            status="ok",
            source_file_count=1,
            success_count=1,
            scan_run_id=1,
            context_run_id=1,
        )
        if source == "scan"
        else DatabaseEvidence(report_count=1, missing_days=[])
    )
    return ReportRunSuccess(
        mode=mode,
        source=source,
        status="ok",
        period=ResolvedPeriod(
            mode=mode,
            source=source,
            start_date=start_date,
            end_date=end_date,
            display_label=display_label,
            as_of_date=date(2026, 5, 25),
        ),
        report=report,
        markdown=f"# {mode}",
        warnings=[],
        source_evidence=evidence,
        publication=PublicationReceipt(
            requested=False,
            sqlite_state="not_attempted",
            markdown_state="not_attempted",
        ),
    )


def test_daily_handler_maps_namespace_to_request() -> None:
    captured: list[object] = []

    class StubRunner:
        def run(self, request):
            captured.append(request)
            return _success_outcome()

    printed: list[str] = []
    console = type(
        "Console",
        (),
        {"print": lambda self, *args, **kwargs: printed.append(args[0])},
    )()

    ok = generate_daily_report(
        Namespace(input="今天工作", no_save=True, date="2026-05-20"),
        console=console,
        runner_factory=lambda: StubRunner(),
        markdown=lambda text: text,
    )

    assert ok is True
    request = captured[0]
    assert request.user_input == "今天工作"
    assert request.save is False
    assert request.report_date_override == "2026-05-20"
    assert any("日报预览" in text for text in printed)


def test_weekly_handler_maps_source_and_week() -> None:
    captured: list[object] = []

    class StubRunner:
        def run(self, request):
            captured.append(request)
            return _success_outcome(mode="weekly", source="db")

    console = type("Console", (), {"print": lambda self, *a, **k: None})()
    ok = generate_weekly_report_cmd(
        Namespace(week="2026-W05", source="db", input=None, no_save=False),
        console=console,
        runner_factory=lambda: StubRunner(),
        markdown=lambda text: text,
    )

    assert ok is True
    assert captured[0].source == "db"
    assert captured[0].week_label == "2026-W05"
    assert captured[0].save is True


def test_monthly_handler_maps_year_month() -> None:
    captured: list[object] = []

    class StubRunner:
        def run(self, request):
            captured.append(request)
            return _success_outcome(mode="monthly")

    console = type("Console", (), {"print": lambda self, *a, **k: None})()
    ok = generate_monthly_report_cmd(
        Namespace(month="2026-05", source="scan", input="补充", no_save=True),
        console=console,
        runner_factory=lambda: StubRunner(),
        markdown=lambda text: text,
    )

    assert ok is True
    assert captured[0].source == "scan"
    assert captured[0].year_month == "2026-05"
    assert captured[0].save is False
    assert captured[0].supplemental_input == "补充"


def test_common_preserves_generation_failure_message() -> None:
    success = _success_outcome()
    failure = ReportRunFailure(
        mode="daily",
        source="scan",
        period=success.period,
        phase="generation",
        error=ReportError(
            error_code=ErrorCode.LLM_GENERATION_FAILED,
            message="LLM unavailable",
            retryable=False,
        ),
        publication=PublicationReceipt(
            requested=False,
            sqlite_state="not_attempted",
            markdown_state="not_attempted",
        ),
        warnings=[],
        source_evidence=success.source_evidence,
    )
    printed: list[str] = []
    console = type(
        "Console",
        (),
        {"print": lambda self, *args, **kwargs: printed.append(args[0])},
    )()

    ok = present_report_outcome(
        failure,
        label="日报",
        console=console,
        markdown=lambda text: text,
    )

    assert ok is False
    assert any("生成失败: LLM unavailable" in text for text in printed)
