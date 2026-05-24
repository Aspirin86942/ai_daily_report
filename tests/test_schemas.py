import pytest

from pydantic import ValidationError

from src.models.schemas import (
    DataSource,
    DailyReportData,
    FileContext,
    MonthlyReportData,
    ReportMode,
    ScanResult,
    WeeklyReportData,
)


def test_daily_report_data():
    report = DailyReportData(
        date="2026-04-03",
        completed_work="今天完成了日报结构精简，去掉了量化字段。",
        work_summary="今天主要确认了报告应当回到自然语言描述，不再输出伪精确数据。",
        next_plan="明天继续修改周报和月报的聚合逻辑。",
    )
    assert report.date == "2026-04-03"
    assert "量化字段" in report.completed_work


def test_weekly_report_data():
    report = WeeklyReportData(
        week_label="2026-W14",
        date_range="2026-03-30 ~ 2026-04-05",
        completed_work="本周完成了日报字段收缩、模板方向确认和数据重建方案确认。",
        self_growth="本周在整理需求和测试边界的过程中，更加重视先锁定输出合同再实现的工作方式。",
        improvement_actions="有些旧字段仍然散落在周报链路中，后续会逐步清点影响面并在改造时同步补齐验证。",
        work_summary="整体工作集中在模型与输出风格瘦身，重点是去掉无依据的量化表达。",
        next_plan="下周开始修改 SQLite、Prompt 和模板。",
        support_needed="如需兼容历史周报入库结构，后续需要在持久化层补充过渡方案。",
        other_notes="本次先锁定周报七段正文合同，暂不调整月报与日报路径。",
    )
    assert report.week_label == "2026-W14"
    assert report.self_growth.startswith("本周")


def test_weekly_report_rejects_legacy_overview_field():
    with pytest.raises(ValidationError):
        WeeklyReportData(
            week_label="2026-W14",
            date_range="2026-03-30 ~ 2026-04-05",
            overview="旧版周报概览字段",
            completed_work="本周完成内容。",
            self_growth="本周自我成长。",
            improvement_actions="本周改善措施。",
            work_summary="本周工作小结。",
            next_plan="下周工作计划。",
            support_needed="本周需要支持。",
            other_notes="本周其他说明。",
        )


def test_monthly_report_data():
    report = MonthlyReportData(
        year_month="2026-04",
        overview="本月以报告风格收缩和生成逻辑简化为主。",
        completed_work="完成了日报、周报、月报统一为段落文本的设计。",
        work_summary="本月工作重点从结构化统计转向自然语言归纳。",
        next_plan="下月继续优化聚合质量和模板表达。",
    )
    assert report.year_month == "2026-04"
    assert "自然语言" in report.work_summary


def test_daily_report_rejects_unknown_fields():
    with pytest.raises(ValidationError):
        DailyReportData(
            date="2026-04-03",
            completed_work="完成了任务",
            work_summary="总结",
            next_plan="计划",
            unexpected="something",
        )


def test_models_package_exports():
    from src.models import DailyReportData as PackageDailyReportData

    assert PackageDailyReportData is DailyReportData


def test_scanner_models_available():
    ctx = FileContext(
        file_path="a", file_type="txt", content="foo", error=None
    )
    assert ctx.error is None
    assert ctx.parser_backend is None
    assert ctx.truncated is False
    assert ScanResult(total_files=0, success_count=0, error_count=0, contexts=[ctx])
