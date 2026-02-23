"""Tests for SQLite-based history storage."""

import sqlite3
from datetime import date, datetime

from src.models.schemas import (
    CategorySummary,
    DailyReportData,
    MonthlyReportData,
    RiskItem,
    WeeklyReportData,
    WorkItem,
)
from src.services.sqlite_store import SQLiteStore


def _make_daily_report(
    report_date: str, summary: str = "daily summary"
) -> DailyReportData:
    return DailyReportData(
        date=report_date,
        summary=summary,
        achievements=[
            WorkItem(
                category="testing",
                content="write unit tests",
                status="completed",
                quantitative="3",
            )
        ],
        risks=[RiskItem(severity="low", description="minor issue")],
        plans=["next step"],
    )


def test_sqlite_store_init_creates_tables(tmp_path):
    db_path = tmp_path / "reports.sqlite3"
    store = SQLiteStore(db_path=db_path)

    assert store.db_path.exists()

    with sqlite3.connect(store.db_path) as conn:
        rows = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table'"
        ).fetchall()
    table_names = {row[0] for row in rows}
    assert "daily_reports" in table_names
    assert "weekly_reports" in table_names
    assert "monthly_reports" in table_names


def test_save_and_get_daily_report(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    report = _make_daily_report("2026-02-03", "saved summary")

    saved_path = store.save_report(report)
    loaded = store.get_report("2026-02-03")

    assert saved_path == store.db_path
    assert loaded is not None
    assert loaded.date == "2026-02-03"
    assert loaded.summary == "saved summary"
    assert loaded.plans == ["next step"]


def test_get_reports_in_range_and_missing_days(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-01-26", "monday"))
    store.save_report(_make_daily_report("2026-01-28", "wednesday"))

    reports, missing = store.get_reports_in_range(
        date(2026, 1, 26),
        date(2026, 1, 30),
    )

    assert [r.date for r in reports] == ["2026-01-26", "2026-01-28"]
    assert missing == ["2026-01-27", "2026-01-29", "2026-01-30"]


def test_get_yesterday_plan(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-02-10"))

    plans = store.get_yesterday_plan(datetime(2026, 2, 11, 9, 30, 0))

    assert plans == ["next step"]


def test_save_and_get_weekly_monthly_reports(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")

    weekly = WeeklyReportData(
        week_label="2026-W06",
        date_range="2026-02-02 ~ 2026-02-08",
        summary="weekly summary",
        category_summaries=[
            CategorySummary(category="testing", items=["task"], total_count=1)
        ],
        risks=[],
        key_achievements=["weekly achievement"],
        next_week_plans=["weekly plan"],
        missing_days=[],
        data_source="db",
    )
    monthly = MonthlyReportData(
        year_month="2026-02",
        summary="monthly summary",
        category_summaries=[],
        risks=[],
        statistics={"done": "10"},
        key_achievements=["monthly achievement"],
        next_month_plans=["monthly plan"],
        missing_days=[],
        data_source="db",
    )

    store.save_weekly_report(weekly)
    store.save_monthly_report(monthly)

    loaded_weekly = store.get_weekly_report("2026-W06")
    loaded_monthly = store.get_monthly_report("2026-02")

    assert loaded_weekly is not None
    assert loaded_weekly.summary == "weekly summary"
    assert loaded_monthly is not None
    assert loaded_monthly.statistics["done"] == "10"
