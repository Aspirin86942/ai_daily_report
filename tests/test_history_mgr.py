"""Tests for HistoryManager using SQLite backend."""

from datetime import date, datetime

from src.models.schemas import (
    CategorySummary,
    DailyReportData,
    MonthlyReportData,
    RiskItem,
    WeeklyReportData,
    WorkItem,
)
from src.services.history_mgr import HistoryManager


def _make_daily(report_date: str, summary: str = "daily summary") -> DailyReportData:
    return DailyReportData(
        date=report_date,
        summary=summary,
        achievements=[
            WorkItem(
                category="testing",
                content="write tests",
                status="done",
                quantitative="1",
            )
        ],
        risks=[RiskItem(severity="low", description="minor")],
        plans=["next task"],
    )


def test_history_manager_init(tmp_path):
    db_dir = tmp_path / "db"
    mgr = HistoryManager(db_dir=db_dir)

    assert mgr.db_dir == db_dir
    assert mgr.db_dir.exists()
    assert mgr.db_path == db_dir / "reports.sqlite3"
    assert mgr.db_path.exists()


def test_save_and_load_report(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    report = _make_daily("2026-01-28", "saved summary")

    saved_path = mgr.save_report(report)
    loaded = mgr.get_report("2026-01-28")

    assert saved_path == mgr.db_path
    assert loaded is not None
    assert loaded.date == "2026-01-28"
    assert loaded.summary == "saved summary"


def test_get_month_reports(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    mgr.save_report(_make_daily("2026-01-27"))
    mgr.save_report(_make_daily("2026-01-28"))
    mgr.save_report(_make_daily("2026-02-01"))

    reports = mgr.get_month_reports("2026-01")
    assert [r.date for r in reports] == ["2026-01-27", "2026-01-28"]


def test_get_reports_in_range(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    mgr.save_report(_make_daily("2026-01-27"))
    mgr.save_report(_make_daily("2026-01-29"))

    reports, missing = mgr.get_reports_in_range(date(2026, 1, 27), date(2026, 1, 30))

    assert [r.date for r in reports] == ["2026-01-27", "2026-01-29"]
    assert missing == ["2026-01-28", "2026-01-30"]


def test_get_reports_in_range_skips_weekends(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")

    reports, missing = mgr.get_reports_in_range(date(2026, 1, 31), date(2026, 2, 1))

    assert reports == []
    assert missing == []


def test_get_week_reports(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    mgr.save_report(_make_daily("2026-01-26"))
    mgr.save_report(_make_daily("2026-01-28"))

    reports, missing = mgr.get_week_reports(2026, 5)

    assert [r.date for r in reports] == ["2026-01-26", "2026-01-28"]
    assert "2026-01-27" in missing
    assert "2026-01-29" in missing
    assert "2026-01-30" in missing


def test_get_yesterday_plan(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    mgr.save_report(_make_daily("2026-02-10"))

    plans = mgr.get_yesterday_plan(datetime(2026, 2, 11, 9, 30, 0))

    assert plans == ["next task"]


def test_list_all_reports(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    mgr.save_report(_make_daily("2026-02-03"))
    mgr.save_report(_make_daily("2026-02-01"))

    assert mgr.list_all_reports() == ["2026-02-01", "2026-02-03"]


def test_save_weekly_report(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    report = WeeklyReportData(
        week_label="2026-W05",
        date_range="2026-01-26 ~ 2026-02-01",
        summary="weekly summary",
        category_summaries=[
            CategorySummary(category="testing", items=["task"], total_count=1)
        ],
        risks=[],
        key_achievements=["done"],
        next_week_plans=["next"],
        missing_days=[],
        data_source="db",
    )

    saved_path = mgr.save_weekly_report(report)
    loaded = mgr.get_weekly_report("2026-W05")

    assert saved_path == mgr.db_path
    assert loaded is not None
    assert loaded.summary == "weekly summary"


def test_save_monthly_report(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    report = MonthlyReportData(
        year_month="2026-01",
        summary="monthly summary",
        category_summaries=[],
        risks=[],
        statistics={"total": "5"},
        key_achievements=["done"],
        next_month_plans=["next"],
        missing_days=[],
        data_source="db",
    )

    saved_path = mgr.save_monthly_report(report)
    loaded = mgr.get_monthly_report("2026-01")

    assert saved_path == mgr.db_path
    assert loaded is not None
    assert loaded.statistics["total"] == "5"
