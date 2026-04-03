from importlib import util
from pathlib import Path

schema_path = Path(__file__).resolve().parents[1] / "src" / "models" / "schemas.py"
spec = util.spec_from_file_location("schemas_module", schema_path)
schemas = util.module_from_spec(spec)
assert spec.loader
spec.loader.exec_module(schemas)

DataSource = schemas.DataSource
DailyReportData = schemas.DailyReportData
MonthlyReportData = schemas.MonthlyReportData
ReportMode = schemas.ReportMode
WeeklyReportData = schemas.WeeklyReportData


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
        overview="本周主要完成了报告结构调整方案确认。",
        completed_work="本周完成了日报字段收缩、模板方向确认和数据重建方案确认。",
        work_summary="整体工作集中在模型与输出风格瘦身，重点是去掉无依据的量化表达。",
        next_plan="下周开始修改 SQLite、Prompt 和模板。",
    )
    assert report.week_label == "2026-W14"
    assert report.overview.startswith("本周")


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
