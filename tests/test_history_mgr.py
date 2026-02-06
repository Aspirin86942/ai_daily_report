"""测试历史记录管理"""
import json
import pytest
from datetime import date, datetime
from src.services.history_mgr import HistoryManager
from src.models.schemas import (
    DailyReportData, WorkItem, RiskItem,
    WeeklyReportData, MonthlyReportData, CategorySummary
)


def test_history_manager_init():
    """测试管理器初始化"""
    mgr = HistoryManager()
    assert mgr.db_dir.exists()


def test_save_and_load_report():
    """测试保存和读取日报"""
    mgr = HistoryManager()

    report = DailyReportData(
        date="2026-01-28",
        summary="测试日报",
        achievements=[
            WorkItem(
                category="测试",
                content="单元测试",
                status="已完成",
                quantitative="1项"
            )
        ],
        risks=[],
        plans=["明日继续测试"]
    )

    file_path = mgr.save_report(report)
    assert file_path.exists()

    reports = mgr.get_month_reports("2026-01")
    assert len(reports) > 0
    assert any(r.date == "2026-01-28" for r in reports)


def test_get_reports_in_range():
    """测试日期范围内日报读取"""
    mgr = HistoryManager()

    # 先保存测试日报 (2026-01-27 ~ 2026-01-29)
    for day in (27, 28, 29):
        report = DailyReportData(
            date=f"2026-01-{day:02d}",
            summary=f"测试日报 {day}",
            achievements=[
                WorkItem(category="测试", content="内容", status="已完成")
            ],
            risks=[],
            plans=["计划"]
        )
        mgr.save_report(report)

    start = date(2026, 1, 27)
    end = date(2026, 1, 29)
    reports, missing = mgr.get_reports_in_range(start, end)

    # 27(Tue), 28(Wed), 29(Thu) 都是工作日
    assert len(reports) == 3
    dates_found = {r.date for r in reports}
    assert "2026-01-27" in dates_found
    assert "2026-01-28" in dates_found
    assert "2026-01-29" in dates_found


def test_get_reports_in_range_missing_days():
    """测试缺失日期检测"""
    mgr = HistoryManager()

    # 确保 2026-01-30 的日报不存在 (删除如果有)
    test_file = mgr.db_dir / "2026-01-30.json"
    if test_file.exists():
        test_file.unlink()

    start = date(2026, 1, 29)
    end = date(2026, 1, 30)
    reports, missing = mgr.get_reports_in_range(start, end)

    # 30 是周五 (工作日)，应该在缺失列表中
    assert "2026-01-30" in missing


def test_get_reports_in_range_skips_weekends():
    """测试跳过周末"""
    mgr = HistoryManager()

    # 2026-01-31 (Saturday), 2026-02-01 (Sunday)
    start = date(2026, 1, 31)
    end = date(2026, 2, 1)
    reports, missing = mgr.get_reports_in_range(start, end)

    # 周末不计入缺失
    assert "2026-01-31" not in missing
    assert "2026-02-01" not in missing


def test_get_week_reports():
    """测试按 ISO 周读取日报"""
    mgr = HistoryManager()

    # 2026-W05: Monday 2026-01-26 ~ Sunday 2026-02-01
    reports, missing = mgr.get_week_reports(2026, 5)
    assert isinstance(reports, list)
    assert isinstance(missing, list)


def test_save_weekly_report():
    """测试保存周报"""
    mgr = HistoryManager()

    report = WeeklyReportData(
        week_label="2026-W05",
        date_range="2026-01-26 ~ 2026-02-01",
        summary="测试周报",
        category_summaries=[
            CategorySummary(category="测试", items=["项目A"], total_count=1)
        ],
        key_achievements=["完成测试"],
        next_week_plans=["继续测试"],
        data_source="db"
    )

    file_path = mgr.save_weekly_report(report)
    assert file_path.exists()
    assert file_path.name == "2026-W05.json"

    # 验证文件内容
    with open(file_path, 'r', encoding='utf-8') as f:
        data = json.load(f)
    assert data['week_label'] == "2026-W05"


def test_save_monthly_report():
    """测试保存月报"""
    mgr = HistoryManager()

    report = MonthlyReportData(
        year_month="2026-01",
        summary="测试月报",
        category_summaries=[],
        statistics={"审计项目数": "5"},
        key_achievements=["完成年度审计"],
        next_month_plans=["启动新项目"],
        data_source="db"
    )

    file_path = mgr.save_monthly_report(report)
    assert file_path.exists()
    assert file_path.name == "2026-01.json"

    with open(file_path, 'r', encoding='utf-8') as f:
        data = json.load(f)
    assert data['year_month'] == "2026-01"
    assert data['statistics']['审计项目数'] == "5"
