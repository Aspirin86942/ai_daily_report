"""测试 Pydantic 数据模型"""
import pytest
from src.models.schemas import (
    ReportMode, DataSource, CategorySummary,
    WeeklyReportData, MonthlyReportData, RiskItem
)


def test_report_mode_enum():
    """测试 ReportMode 枚举"""
    assert ReportMode.daily == "daily"
    assert ReportMode.weekly == "weekly"
    assert ReportMode.monthly == "monthly"


def test_data_source_enum():
    """测试 DataSource 枚举"""
    assert DataSource.db == "db"
    assert DataSource.scan == "scan"


def test_category_summary():
    """测试 CategorySummary 模型"""
    cs = CategorySummary(
        category="现场审计",
        items=["审计项目A", "审计项目B"],
        total_count=2,
        completion_rate="100%"
    )
    assert cs.category == "现场审计"
    assert len(cs.items) == 2
    assert cs.total_count == 2
    assert cs.completion_rate == "100%"


def test_category_summary_optional_completion_rate():
    """测试 CategorySummary 可选字段"""
    cs = CategorySummary(
        category="数据分析",
        items=["分析任务"],
        total_count=1
    )
    assert cs.completion_rate is None


def test_weekly_report_data():
    """测试 WeeklyReportData 模型"""
    report = WeeklyReportData(
        week_label="2026-W05",
        date_range="2026-01-27 ~ 2026-02-02",
        summary="本周完成审计工作",
        category_summaries=[
            CategorySummary(
                category="现场审计",
                items=["项目A审计"],
                total_count=1,
                completion_rate="100%"
            )
        ],
        risks=[
            RiskItem(severity="中", description="测试风险")
        ],
        key_achievements=["完成项目A审计"],
        next_week_plans=["启动项目B审计"],
        data_source="db"
    )
    assert report.week_label == "2026-W05"
    assert len(report.category_summaries) == 1
    assert len(report.risks) == 1
    assert report.missing_days == []  # default_factory


def test_weekly_report_data_defaults():
    """测试 WeeklyReportData 默认值"""
    report = WeeklyReportData(
        week_label="2026-W05",
        date_range="2026-01-27 ~ 2026-02-02",
        summary="概述",
        category_summaries=[],
        key_achievements=[],
        next_week_plans=[],
        data_source="scan"
    )
    assert report.risks == []
    assert report.missing_days == []


def test_monthly_report_data():
    """测试 MonthlyReportData 模型"""
    report = MonthlyReportData(
        year_month="2026-01",
        summary="本月完成多项审计工作",
        category_summaries=[
            CategorySummary(
                category="报告撰写",
                items=["报告A", "报告B"],
                total_count=2
            )
        ],
        statistics={"审计项目数": "5", "发现问题数": "12"},
        key_achievements=["完成年度审计计划"],
        next_month_plans=["启动新项目"],
        missing_days=["2026-01-15"],
        data_source="db"
    )
    assert report.year_month == "2026-01"
    assert report.statistics["审计项目数"] == "5"
    assert len(report.missing_days) == 1


def test_monthly_report_data_defaults():
    """测试 MonthlyReportData 默认值"""
    report = MonthlyReportData(
        year_month="2026-02",
        summary="概述",
        category_summaries=[],
        key_achievements=[],
        next_month_plans=[],
        data_source="scan"
    )
    assert report.risks == []
    assert report.statistics == {}
    assert report.missing_days == []


def test_weekly_report_json_roundtrip():
    """测试 WeeklyReportData JSON 序列化/反序列化"""
    report = WeeklyReportData(
        week_label="2026-W05",
        date_range="2026-01-27 ~ 2026-02-02",
        summary="测试",
        category_summaries=[],
        key_achievements=["成果"],
        next_week_plans=["计划"],
        data_source="db"
    )
    json_str = report.model_dump_json()
    restored = WeeklyReportData.model_validate_json(json_str)
    assert restored.week_label == report.week_label
    assert restored.key_achievements == report.key_achievements


def test_monthly_report_json_roundtrip():
    """测试 MonthlyReportData JSON 序列化/反序列化"""
    report = MonthlyReportData(
        year_month="2026-01",
        summary="测试",
        category_summaries=[],
        statistics={"key": "value"},
        key_achievements=["成果"],
        next_month_plans=["计划"],
        data_source="scan"
    )
    json_str = report.model_dump_json()
    restored = MonthlyReportData.model_validate_json(json_str)
    assert restored.year_month == report.year_month
    assert restored.statistics == {"key": "value"}
