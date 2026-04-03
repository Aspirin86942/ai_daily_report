"""Tests for HistoryManager using SQLite backend."""

from datetime import date, datetime

from src.models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from src.services.history_mgr import HistoryManager


def _make_daily(
    report_date: str, work_summary: str = "daily summary"
) -> DailyReportData:
    return DailyReportData(
        date=report_date,
        completed_work="今天完成了 HistoryManager 兼容层改造。",
        work_summary=work_summary,
        next_plan="明天继续处理聚合链路。",
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
    assert loaded.work_summary == "saved summary"


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

    plan_text = mgr.get_yesterday_plan(datetime(2026, 2, 11, 9, 30, 0))

    assert plan_text == "明天继续处理聚合链路。"


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
        overview="本周推进存储结构收敛。",
        completed_work="完成周报文本字段设计。",
        work_summary="主要处理字段压缩和兼容读取。",
        next_plan="下周补充聚合策略验证。",
    )

    saved_path = mgr.save_weekly_report(report)
    loaded = mgr.get_weekly_report("2026-W05")

    assert saved_path == mgr.db_path
    assert loaded is not None
    assert loaded.overview == "本周推进存储结构收敛。"


def test_save_monthly_report(tmp_path):
    mgr = HistoryManager(db_dir=tmp_path / "db")
    report = MonthlyReportData(
        year_month="2026-01",
        overview="本月重点是报告文本化。",
        completed_work="完成月报新结构落库。",
        work_summary="保留简洁文本表达并去除列表字段。",
        next_plan="下月继续优化生成稳定性。",
    )

    saved_path = mgr.save_monthly_report(report)
    loaded = mgr.get_monthly_report("2026-01")

    assert saved_path == mgr.db_path
    assert loaded is not None
    assert loaded.next_plan == "下月继续优化生成稳定性。"
