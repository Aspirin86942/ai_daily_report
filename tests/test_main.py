"""Smoke tests for CLI entrypoints in main.py."""

from argparse import Namespace

import main
from src.models.schemas import DailyReportData, MonthlyReportData, ScanResult, WeeklyReportData


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


def test_generate_daily_report_uses_sqlite_store(monkeypatch):
    calls: list[tuple[str, object]] = []

    class StubFileScanner:
        def scan_today_files(self) -> ScanResult:
            calls.append(("scan_today_files", None))
            return ScanResult(total_files=0, success_count=0, error_count=0, contexts=[])

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
            calls.append(("generate_report", yesterday_plan))
            return DailyReportData(
                date="2026-02-03",
                completed_work="完成日报",
                work_summary="日报摘要",
                next_plan="后续计划",
            )

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "FileScanner", StubFileScanner)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    main.generate_daily_report(
        Namespace(input="今天工作", no_save=False, date=None)
    )

    assert [name for name, _ in calls] == [
        "init",
        "scan_today_files",
        "get_yesterday_plan",
        "generate_report",
        "render_markdown",
        "save_report",
        "save_markdown",
    ]
    assert any("日报预览" in text for text in printed)


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
                overview="周报概览",
                completed_work="完成周报",
                work_summary="周报总结",
                next_plan="下周计划",
            )

    printed = _patch_console(monkeypatch)
    _patch_progress(monkeypatch)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(main, "ReportGenerator", StubReportGenerator)
    monkeypatch.setattr(main, "LLMClient", StubLLMClient)
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    main.generate_weekly_report_cmd(
        Namespace(week="2026-W05", source="db", input=None, no_save=False)
    )

    assert [name for name, _ in calls] == [
        "init",
        "get_week_reports",
        "generate_weekly_report",
        "render_weekly_markdown",
        "save_weekly_report",
        "save_weekly_markdown",
    ]
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

    main.generate_monthly_report_cmd(
        Namespace(month="2026-01", source="db", input=None, no_save=False)
    )

    assert [name for name, _ in calls] == [
        "init",
        "get_reports_in_range",
        "generate_monthly_report",
        "render_monthly_markdown",
        "save_monthly_report",
        "save_monthly_markdown",
    ]
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
