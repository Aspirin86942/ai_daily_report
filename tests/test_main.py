"""Smoke tests for CLI entrypoints in main.py."""

from argparse import Namespace
from datetime import date as real_date
from pathlib import Path
import os
import subprocess
import sys

import pytest

import main
from src.core.healthcheck import HealthCheckResult
from src.models.scanner_contract import ContextSummary, Diagnostic
from src.models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from src.services.context_scheduler import ContextBuildResult


class DummyProgress:
    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def add_task(self, description: str, total=None) -> str:
        return description

    def update(self, task, completed: bool) -> None:
        return None


def _patch_console(monkeypatch) -> list[str]:
    printed: list[str] = []
    monkeypatch.setattr(
        main.console,
        "print",
        lambda *args, **kwargs: printed.append(args[0] if args else ""),
    )
    return printed


def _patch_progress(monkeypatch) -> None:
    monkeypatch.setattr(main, "Progress", lambda *args, **kwargs: DummyProgress())


def _schedule_result(
    file_context: str,
    *,
    status: str = "ok",
    message: str = "synthetic scanner diagnostic",
) -> ContextBuildResult:
    diagnostic = Diagnostic(
        error_code=("RUST_CORE_CRASHED" if status == "error" else "PARSER_FAILED"),
        message=message,
        retryable=False,
        stage=("process" if status == "error" else "parse"),
        file_path=None,
        backend=None,
    )
    return ContextBuildResult(
        file_context="" if status == "error" else file_context,
        status=status,
        summary=ContextSummary(
            source_file_count=1,
            success_count=1 if status != "error" else 0,
            timeout_count=0,
            included_file_count=1 if status != "error" else 0,
            omitted_file_count=0,
            error_file_count=0,
            input_chars=len(file_context) if status != "error" else 0,
            output_chars=len(file_context) if status != "error" else 0,
            total_duration_ms=1,
            discovery_duration_ms=0,
            parse_duration_ms=0,
            compression_duration_ms=0,
        ),
        scan_run_id=1 if status != "error" else None,
        context_run_id=1 if status != "error" else None,
        warnings=[diagnostic] if status == "partial" else [],
        error=diagnostic if status == "error" else None,
    )


def test_generate_daily_report_uses_context_scheduler(monkeypatch):
    calls: list[tuple[str, object]] = []

    class FixedDate(real_date):
        @classmethod
        def today(cls) -> real_date:
            return real_date(2026, 5, 25)

    class StubContextScheduler:
        def build_context(self, request) -> ContextBuildResult:
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
            calls.append(("render_markdown", report.date))
            return "daily markdown"

        def save_markdown(self, markdown: str, report_date: str) -> None:
            calls.append(("save_markdown", report_date))

    class StubLLMClient:
        def generate_report(
            self, user_input: str, file_context: str, yesterday_plan: str
        ) -> DailyReportData:
            calls.append(("generate_report", (file_context, yesterday_plan)))
            return DailyReportData(
                date="2026-02-03",
                completed_work="完成日报",
                work_summary="日报摘要",
                next_plan="后续计划",
            )

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "date", FixedDate)
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_daily_report(
        Namespace(input="今天工作", no_save=False, date=None)
    )

    assert success is True
    assert [name for name, _ in calls] == [
        "init",
        "build_context",
        "get_yesterday_plan",
        "generate_report",
        "render_markdown",
        "save_report",
        "save_markdown",
    ]
    assert ("build_context", ("daily", "scan", "2026-05-24", "2026-05-25")) in calls
    assert ("generate_report", ("scheduler daily context", "昨日计划")) in calls
    assert any("日报预览" in text for text in printed)


def test_generate_daily_report_warns_and_calls_llm_for_partial_context(monkeypatch):
    calls: list[tuple[str, object]] = []

    class StubLogger:
        def warning(self, message: str, *args) -> None:
            calls.append(("logger_warning", message % args))

    class StubContextScheduler:
        def build_context(self, request) -> ContextBuildResult:
            calls.append(("build_context", request.report_mode))
            return _schedule_result(
                "partial context",
                status="partial",
                message="one synthetic file failed",
            )

    class StubSQLiteStore:
        def __init__(self) -> None:
            calls.append(("init", None))

        def get_yesterday_plan(self) -> str:
            calls.append(("get_yesterday_plan", None))
            return ""

        def save_report(self, report: DailyReportData) -> None:
            calls.append(("save_report", report.date))

    class StubReportGenerator:
        def render_markdown(self, report: DailyReportData) -> str:
            calls.append(("render_markdown", report.date))
            return "daily markdown"

        def save_markdown(self, markdown: str, report_date: str) -> None:
            calls.append(("save_markdown", report_date))

    class StubLLMClient:
        def generate_report(
            self, user_input: str, file_context: str, yesterday_plan: str
        ) -> DailyReportData:
            calls.append(("generate_report", file_context))
            return DailyReportData(
                date="2026-02-04",
                completed_work="partial 后继续生成",
                work_summary="partial 摘要",
                next_plan="partial 后续",
            )

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "logger", StubLogger())
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_daily_report(
        Namespace(input="今天工作", no_save=False, date=None)
    )

    assert success is True
    assert any("文件上下文不完整" in text for text in printed)
    assert any("one synthetic file failed" in text for text in printed)
    assert (
        "logger_warning",
        "文件上下文不完整: one synthetic file failed",
    ) in calls
    assert ("generate_report", "partial context") in calls


def test_generate_daily_report_stops_before_constructing_llm_on_context_error(
    monkeypatch,
):
    calls: list[str] = []

    class StubContextScheduler:
        def build_context(self, request) -> ContextBuildResult:
            calls.append("build_context")
            return _schedule_result(
                "must not reach LLM",
                status="error",
                message="Rust scanner process failed",
            )

    class StubSQLiteStore:
        pass

    class StubReportGenerator:
        pass

    class ForbiddenLLMClient:
        def __init__(self) -> None:
            calls.append("llm_init")

    class StubLogger:
        def error(self, message: str, *args) -> None:
            calls.append("logger_error")

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", ForbiddenLLMClient)
    monkeypatch.setattr(main, "logger", StubLogger())

    success = main.generate_daily_report(
        Namespace(input="今天工作", no_save=True, date=None)
    )

    assert success is False
    assert calls == ["build_context", "logger_error"]
    assert any("文件上下文构建失败" in text for text in printed)
    assert any("Rust scanner process failed" in text for text in printed)


def test_generate_weekly_report_db_uses_sqlite_store(monkeypatch):
    calls: list[tuple[str, object]] = []

    reports = [
        DailyReportData(
            date="2026-01-26",
            completed_work="完成周报输入",
            work_summary="周报摘要",
            next_plan="继续推进",
        )
    ]

    class StubSQLiteStore:
        def __init__(self) -> None:
            calls.append(("init", None))

        def get_week_reports(self, year: int, week: int):
            calls.append(("get_week_reports", (year, week)))
            return reports, ["2026-01-27"]

        def save_weekly_report(self, report: WeeklyReportData) -> None:
            calls.append(("save_weekly_report", report.week_label))

    class StubReportGenerator:
        def render_weekly_markdown(self, report: WeeklyReportData) -> str:
            calls.append(("render_weekly_markdown", report.week_label))
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
            calls.append(("generate_weekly_report", (year, week, data_source)))
            return WeeklyReportData(
                week_label=f"{year}-W{week:02d}",
                date_range="2026-01-26 ~ 2026-02-01",
                completed_work="完成周报",
                self_growth="自我成长",
                improvement_actions="改善措施",
                work_summary="周报总结",
                next_plan="下周计划",
                support_needed="需要支持",
                other_notes="其他说明",
            )

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_weekly_report_cmd(
        Namespace(week="2026-W05", source="db", input=None, no_save=False)
    )

    assert success is True
    assert [name for name, _ in calls] == [
        "init",
        "get_week_reports",
        "generate_weekly_report",
        "render_weekly_markdown",
        "save_weekly_report",
        "save_weekly_markdown",
    ]
    assert any("周报预览" in text for text in printed)


def test_generate_weekly_report_scan_uses_context_scheduler(monkeypatch):
    calls: list[tuple[str, object]] = []

    class StubContextScheduler:
        def build_context(self, request) -> ContextBuildResult:
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
        def __init__(self) -> None:
            calls.append(("init", None))

        def save_weekly_report(self, report: WeeklyReportData) -> None:
            calls.append(("save_weekly_report", report.week_label))

    class StubReportGenerator:
        def render_weekly_markdown(self, report: WeeklyReportData) -> str:
            calls.append(("render_weekly_markdown", report.week_label))
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
                self_growth="自我成长",
                improvement_actions="改善措施",
                work_summary="周报总结",
                next_plan="下周计划",
                support_needed="需要支持",
                other_notes="其他说明",
            )

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_weekly_report_cmd(
        Namespace(
            week="2026-W20",
            source="scan",
            input="用户补充内容",
            no_save=False,
        )
    )

    assert success is True
    assert (
        "build_context",
        ("weekly", "scan", "2026-05-11", "2026-05-17"),
    ) in calls
    assert (
        "generate_weekly_report",
        "scheduler weekly context\n\n---\n\n用户补充: 用户补充内容",
    ) in calls
    assert any("周报预览" in text for text in printed)


def test_generate_monthly_report_db_uses_sqlite_store(monkeypatch):
    calls: list[tuple[str, object]] = []

    reports = [
        DailyReportData(
            date="2026-01-05",
            completed_work="完成月报输入",
            work_summary="月报摘要",
            next_plan="继续推进",
        )
    ]

    class StubSQLiteStore:
        def __init__(self) -> None:
            calls.append(("init", None))

        def get_reports_in_range(self, start_date, end_date):
            calls.append(("get_reports_in_range", (start_date.isoformat(), end_date.isoformat())))
            return reports, ["2026-01-06"]

        def save_monthly_report(self, report: MonthlyReportData) -> None:
            calls.append(("save_monthly_report", report.year_month))

    class StubReportGenerator:
        def render_monthly_markdown(self, report: MonthlyReportData) -> str:
            calls.append(("render_monthly_markdown", report.year_month))
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
            calls.append(("generate_monthly_report", (year_month, data_source)))
            return MonthlyReportData(
                year_month=year_month,
                overview="月报概览",
                completed_work="完成月报",
                work_summary="月报总结",
                next_plan="下月计划",
            )

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_monthly_report_cmd(
        Namespace(month="2026-01", source="db", input=None, no_save=False)
    )

    assert success is True
    assert [name for name, _ in calls] == [
        "init",
        "get_reports_in_range",
        "generate_monthly_report",
        "render_monthly_markdown",
        "save_monthly_report",
        "save_monthly_markdown",
    ]
    assert any("月报预览" in text for text in printed)


def test_generate_monthly_report_scan_uses_context_scheduler(monkeypatch):
    calls: list[tuple[str, object]] = []

    class StubContextScheduler:
        def build_context(self, request) -> ContextBuildResult:
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
        def __init__(self) -> None:
            calls.append(("init", None))

        def save_monthly_report(self, report: MonthlyReportData) -> None:
            calls.append(("save_monthly_report", report.year_month))

    class StubReportGenerator:
        def render_monthly_markdown(self, report: MonthlyReportData) -> str:
            calls.append(("render_monthly_markdown", report.year_month))
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
                overview="月报概览",
                completed_work="完成月报",
                work_summary="月报总结",
                next_plan="下月计划",
            )

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_monthly_report_cmd(
        Namespace(
            month="2026-05",
            source="scan",
            input="月报补充内容",
            no_save=False,
        )
    )

    assert success is True
    assert (
        "build_context",
        ("monthly", "scan", "2026-05-01", "2026-05-31"),
    ) in calls
    assert (
        "generate_monthly_report",
        "scheduler monthly context\n\n---\n\n用户补充: 月报补充内容",
    ) in calls
    assert any("月报预览" in text for text in printed)


def test_list_reports_uses_sqlite_store(monkeypatch):
    calls: dict[str, int] = {"init": 0, "list_all_reports": 0}

    class StubSQLiteStore:
        def __init__(self) -> None:
            calls["init"] += 1

        def list_all_reports(self) -> list[str]:
            calls["list_all_reports"] += 1
            return []

    printed = _patch_console(monkeypatch)

    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)

    main.list_reports()

    assert calls == {"init": 1, "list_all_reports": 1}
    assert any("已有日报列表" in text for text in printed)
    assert any("暂无日报数据" in text for text in printed)


def test_build_parser_accepts_doctor_and_check_config_alias():
    parser = main.build_parser()

    doctor_args = parser.parse_args(["doctor"])
    strict_args = parser.parse_args(["doctor", "--strict"])
    alias_args = parser.parse_args(["check-config"])

    assert doctor_args.subcommand == "doctor"
    assert doctor_args.strict is False
    assert strict_args.subcommand == "doctor"
    assert strict_args.strict is True
    assert alias_args.subcommand == "doctor"
    assert alias_args.strict is False


def test_run_doctor_cmd_uses_healthcheck(monkeypatch):
    printed = _patch_console(monkeypatch)
    strict_values = []

    monkeypatch.setattr(
        main,
        "collect_healthcheck",
        lambda *, strict=False: (
            strict_values.append(strict)
            or HealthCheckResult(
            info={
                "LLM Provider": "deepseek",
                "工作目录": str(Path("D:/work")),
                "LLM 模型": "deepseek-chat",
                "最大并发": "4",
                "SQLite DB": "data/db/reports.sqlite3",
                "API Key": "sk-test-12...",
            },
            warnings=["缺少敏感配置文件: config/.secrets.yaml"],
            errors=[],
            )
        ),
    )

    success = main.run_doctor_cmd(strict=True)

    assert success is True
    assert strict_values == [True]
    assert any("环境检查" in str(text) for text in printed)
    assert any("LLM Provider" in str(text) for text in printed)
    assert any("警告" in str(text) for text in printed)
    assert any("所有检查通过" in str(text) for text in printed)


@pytest.mark.parametrize(
    ("command", "args"),
    [
        (
            main.generate_daily_report,
            Namespace(input="work", no_save=True, date=None),
        ),
        (
            main.generate_weekly_report_cmd,
            Namespace(
                week="2026-W01",
                source="scan",
                input=None,
                no_save=True,
            ),
        ),
        (
            main.generate_monthly_report_cmd,
            Namespace(
                month="2026-01",
                source="scan",
                input=None,
                no_save=True,
            ),
        ),
    ],
    ids=["daily", "weekly", "monthly"],
)
def test_report_command_returns_false_when_generation_fails(
    monkeypatch,
    command,
    args: Namespace,
):
    class StubContextScheduler:
        def build_context(self, request) -> ContextBuildResult:
            return _schedule_result("report context")

    class StubSQLiteStore:
        def get_yesterday_plan(self) -> str:
            return ""

    class FailingLLMClient:
        def generate_report(self, **kwargs):
            raise RuntimeError("LLM unavailable")

        def generate_weekly_report(self, **kwargs):
            raise RuntimeError("LLM unavailable")

        def generate_monthly_report(self, **kwargs):
            raise RuntimeError("LLM unavailable")

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "ContextScheduler", StubContextScheduler)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", object)
    monkeypatch.setattr(main, "LLMClient", FailingLLMClient)

    success = command(args)

    assert success is False
    assert any("生成失败: LLM unavailable" in text for text in printed)


@pytest.mark.parametrize(
    ("handler_name", "argv"),
    [
        ("generate_daily_report", ["daily", "--input", "work"]),
        (
            "generate_weekly_report_cmd",
            ["weekly", "2026-W01", "--source", "db"],
        ),
        (
            "generate_monthly_report_cmd",
            ["monthly", "2026-01", "--source", "db"],
        ),
    ],
)
def test_main_returns_one_when_report_command_fails(
    monkeypatch,
    handler_name: str,
    argv: list[str],
):
    monkeypatch.setattr(main, handler_name, lambda args: False)
    monkeypatch.setattr(sys, "argv", ["main.py", *argv])

    assert main.main() == 1


@pytest.mark.parametrize(
    ("handler_name", "argv"),
    [
        ("generate_daily_report", ["daily", "--input", "work"]),
        (
            "generate_weekly_report_cmd",
            ["weekly", "2026-W01", "--source", "db"],
        ),
        (
            "generate_monthly_report_cmd",
            ["monthly", "2026-01", "--source", "db"],
        ),
    ],
)
def test_main_returns_zero_when_report_command_succeeds(
    monkeypatch,
    handler_name: str,
    argv: list[str],
):
    monkeypatch.setattr(main, handler_name, lambda args: True)
    monkeypatch.setattr(sys, "argv", ["main.py", *argv])

    assert main.main() == 0


def test_main_returns_doctor_status(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["main.py", "doctor"])
    monkeypatch.setattr(main, "run_doctor_cmd", lambda *, strict=False: False)
    assert main.main() == 1

    monkeypatch.setattr(main, "run_doctor_cmd", lambda *, strict=False: True)
    assert main.main() == 0


def test_main_returns_zero_without_subcommand(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["main.py"])

    assert main.main() == 0


def test_main_returns_one_for_unexpected_exception(monkeypatch):
    def fail() -> None:
        raise RuntimeError("unexpected failure")

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(sys, "argv", ["main.py", "list"])
    monkeypatch.setattr(main, "list_reports", fail)

    assert main.main() == 1
    assert any("unexpected failure" in text for text in printed)


def test_main_returns_130_for_keyboard_interrupt(monkeypatch):
    def interrupt() -> None:
        raise KeyboardInterrupt

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(sys, "argv", ["main.py", "list"])
    monkeypatch.setattr(main, "list_reports", interrupt)

    assert main.main() == 130
    assert any("操作已取消" in text for text in printed)


def test_cli_help_exits_zero():
    completed = subprocess.run(
        [sys.executable, str(Path(main.__file__).resolve()), "--help"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )

    assert completed.returncode == 0
    assert "usage:" in completed.stdout


def _run_main_with_sitecustomize(
    tmp_path: Path,
    source: str,
    *arguments: str,
) -> subprocess.CompletedProcess[str]:
    """在隔离启动钩子下运行真实 CLI 进程。"""
    (tmp_path / "sitecustomize.py").write_text(source, encoding="utf-8")
    env = os.environ.copy()
    env["PYTHONPATH"] = os.pathsep.join(
        filter(None, [str(tmp_path), env.get("PYTHONPATH", "")])
    )
    return subprocess.run(
        [sys.executable, str(Path(main.__file__).resolve()), *arguments],
        capture_output=True,
        text=True,
        encoding="utf-8",
        env=env,
        check=False,
    )


def test_cli_doctor_bootstraps_without_rich_or_file_logger(tmp_path):
    """doctor 必须在完整 UI/业务依赖导入前进入轻量诊断路径。"""
    completed = _run_main_with_sitecustomize(
        tmp_path,
        "\n".join(
            [
                "import builtins",
                "import sys",
                "import types",
                "real_import = builtins.__import__",
                "def guarded_import(name, *args, **kwargs):",
                "    if name == 'rich' or name.startswith('rich.') or name == 'src.core.logger':",
                "        raise RuntimeError(f'forbidden eager import: {name}')",
                "    return real_import(name, *args, **kwargs)",
                "builtins.__import__ = guarded_import",
                "healthcheck = types.ModuleType('src.core.healthcheck')",
                "class Result:",
                "    info = {'轻量入口': '可用'}",
                "    warnings = []",
                "    errors = []",
                "healthcheck.collect_healthcheck = lambda: Result()",
                "sys.modules['src.core.healthcheck'] = healthcheck",
            ]
        ),
        "doctor",
    )

    assert completed.returncode == 0
    assert "轻量入口: 可用" in completed.stdout
    assert "forbidden eager import" not in completed.stdout + completed.stderr


def test_cli_strict_doctor_uses_the_lightweight_entrypoint(tmp_path):
    completed = _run_main_with_sitecustomize(
        tmp_path,
        "\n".join(
            [
                "import sys",
                "import types",
                "healthcheck = types.ModuleType('src.core.healthcheck')",
                "class Result:",
                "    warnings = []",
                "    errors = []",
                "def collect_healthcheck(*, strict=False):",
                "    result = Result()",
                "    result.info = {'严格模式': str(strict)}",
                "    return result",
                "healthcheck.collect_healthcheck = collect_healthcheck",
                "sys.modules['src.core.healthcheck'] = healthcheck",
            ]
        ),
        "doctor",
        "--strict",
    )

    assert completed.returncode == 0
    assert "严格模式: True" in completed.stdout


def test_cli_doctor_redacts_bootstrap_exception_text(tmp_path):
    """启动期异常可能含 YAML 原文，doctor 不得直接回显。"""
    fake_secret = "dummy-secret-must-not-leak"
    completed = _run_main_with_sitecustomize(
        tmp_path,
        "\n".join(
            [
                "import sys",
                "import types",
                "healthcheck = types.ModuleType('src.core.healthcheck')",
                "def fail():",
                f"    raise ValueError('{fake_secret}')",
                "healthcheck.collect_healthcheck = fail",
                "sys.modules['src.core.healthcheck'] = healthcheck",
            ]
        ),
        "doctor",
    )

    combined_output = completed.stdout + completed.stderr
    assert completed.returncode == 1
    assert "doctor 无法启动 (ValueError)" in combined_output
    assert fake_secret not in combined_output


def test_cli_invalid_week_exits_nonzero():
    completed = subprocess.run(
        [
            sys.executable,
            str(Path(main.__file__).resolve()),
            "weekly",
            "invalid-week",
            "--source",
            "db",
        ],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )

    assert completed.returncode == 1
    assert "错误:" in completed.stdout
