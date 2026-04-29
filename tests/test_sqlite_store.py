"""Tests for SQLite-based history storage."""

import sqlite3
from datetime import date, datetime

from src.models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from src.services import SQLiteStore


def _make_daily_report(
    report_date: str, work_summary: str = "daily summary"
) -> DailyReportData:
    return DailyReportData(
        date=report_date,
        completed_work="今天完成了 SQLite 文本字段改造。",
        work_summary=work_summary,
        next_plan="明天继续处理周报和月报存储。",
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

    with sqlite3.connect(store.db_path) as conn:
        daily_columns = {
            row[1] for row in conn.execute("PRAGMA table_info(daily_reports)").fetchall()
        }
        weekly_columns = {
            row[1]
            for row in conn.execute("PRAGMA table_info(weekly_reports)").fetchall()
        }
        monthly_columns = {
            row[1]
            for row in conn.execute("PRAGMA table_info(monthly_reports)").fetchall()
        }

    assert daily_columns == {
        "date",
        "completed_work",
        "work_summary",
        "next_plan",
        "raw_json",
        "created_at",
        "updated_at",
    }
    assert weekly_columns == {
        "week_label",
        "date_range",
        "overview",
        "completed_work",
        "work_summary",
        "next_plan",
        "raw_json",
        "created_at",
        "updated_at",
    }
    assert monthly_columns == {
        "year_month",
        "overview",
        "completed_work",
        "work_summary",
        "next_plan",
        "raw_json",
        "created_at",
        "updated_at",
    }


def test_services_exports_sqlite_store():
    assert SQLiteStore.__name__ == "SQLiteStore"


def test_save_and_get_daily_report(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    report = _make_daily_report("2026-02-03", "saved summary")

    saved_path = store.save_report(report)
    loaded = store.get_report("2026-02-03")

    assert saved_path == store.db_path
    assert loaded is not None
    assert loaded.date == "2026-02-03"
    assert loaded.completed_work == "今天完成了 SQLite 文本字段改造。"
    assert loaded.work_summary == "saved summary"
    assert loaded.next_plan == "明天继续处理周报和月报存储。"


def test_get_month_reports(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-01-27"))
    store.save_report(_make_daily_report("2026-01-28"))
    store.save_report(_make_daily_report("2026-02-01"))

    reports = store.get_month_reports("2026-01")

    assert [r.date for r in reports] == ["2026-01-27", "2026-01-28"]


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


def test_get_reports_in_range_skips_weekends(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")

    reports, missing = store.get_reports_in_range(
        date(2026, 1, 31),
        date(2026, 2, 1),
    )

    assert reports == []
    assert missing == []


def test_get_week_reports(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-01-26"))
    store.save_report(_make_daily_report("2026-01-28"))

    reports, missing = store.get_week_reports(2026, 5)

    assert [r.date for r in reports] == ["2026-01-26", "2026-01-28"]
    assert "2026-01-27" in missing
    assert "2026-01-29" in missing
    assert "2026-01-30" in missing


def test_get_yesterday_plan(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-02-10"))

    plan_text = store.get_yesterday_plan(datetime(2026, 2, 11, 9, 30, 0))

    assert plan_text == "明天继续处理周报和月报存储。"


def test_list_all_reports(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-02-03"))
    store.save_report(_make_daily_report("2026-02-01"))

    assert store.list_all_reports() == ["2026-02-01", "2026-02-03"]


def test_save_and_get_weekly_monthly_reports(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")

    weekly = WeeklyReportData(
        week_label="2026-W06",
        date_range="2026-02-02 ~ 2026-02-08",
        overview="本周围绕报告结构简化推进。",
        completed_work="完成了日报和周报新字段设计。",
        work_summary="整体工作集中在去掉列表结构和量化字段。",
        next_plan="下周继续修改模板和聚合逻辑。",
    )
    monthly = MonthlyReportData(
        year_month="2026-02",
        overview="本月主要处理报告文本化收缩。",
        completed_work="完成了数据库和模板的改造准备。",
        work_summary="整体方向是让输出回到自然段表达。",
        next_plan="下月继续验证生成质量。",
    )

    store.save_weekly_report(weekly)
    store.save_monthly_report(monthly)

    loaded_weekly = store.get_weekly_report("2026-W06")
    loaded_monthly = store.get_monthly_report("2026-02")

    assert loaded_weekly is not None
    assert loaded_weekly.overview.startswith("本周")
    assert loaded_monthly is not None
    assert loaded_monthly.overview.startswith("本月")
