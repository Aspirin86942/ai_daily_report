"""测试报告生成"""

from src.services.report_gen import ReportGenerator
from src.models.schemas import (
    DailyReportData,
    WeeklyReportData,
    MonthlyReportData,
)


def test_report_generator_init():
    """测试生成器初始化"""
    gen = ReportGenerator()
    assert gen.reports_dir.exists()


def test_render_markdown():
    """测试日报 Markdown 渲染"""
    gen = ReportGenerator()

    report = DailyReportData(
        date="2026-01-28",
        completed_work="今天完成了日报模板精简，去掉了列表项和量化字段。",
        work_summary="今天的主要工作是让日报输出回到自然语言段落，而不是伪结构化清单。",
        next_plan="明天继续修改周报和月报模板。",
    )

    markdown = gen.render_markdown(report)
    assert "## 今日工作完成内容" in markdown
    assert "## 今日工作小结" in markdown
    assert "## 明日工作计划" in markdown
    assert "日报模板精简" in markdown
    assert "- **内容**" not in markdown


def test_render_weekly_markdown():
    """测试周报 Markdown 渲染"""
    gen = ReportGenerator()

    report = WeeklyReportData(
        week_label="2026-W05",
        date_range="2026-01-27 ~ 2026-02-02",
        summary="本周完成审计工作",
        category_summaries=[
            CategorySummary(
                category="现场审计",
                items=["项目A审计", "项目B审计"],
                total_count=2,
                completion_rate="100%",
            )
        ],
        risks=[RiskItem(severity="中", description="测试风险")],
        key_achievements=["完成项目A审计"],
        next_week_plans=["启动项目C审计"],
        missing_days=["2026-01-30"],
        data_source="db",
    )

    markdown = gen.render_weekly_markdown(report)
    assert "2026-W05" in markdown
    assert "本周完成审计工作" in markdown
    assert "现场审计" in markdown
    assert "项目A审计" in markdown
    assert "测试风险" in markdown
    assert "2026-01-30" in markdown


def test_render_monthly_markdown():
    """测试月报 Markdown 渲染"""
    gen = ReportGenerator()

    report = MonthlyReportData(
        year_month="2026-01",
        summary="本月完成多项工作",
        category_summaries=[
            CategorySummary(category="报告撰写", items=["审计报告A"], total_count=1)
        ],
        statistics={"审计项目数": "5", "发现问题数": "12"},
        key_achievements=["完成年度审计计划"],
        next_month_plans=["启动新项目"],
        data_source="db",
    )

    markdown = gen.render_monthly_markdown(report)
    assert "2026-01" in markdown
    assert "本月完成多项工作" in markdown
    assert "报告撰写" in markdown
    assert "审计项目数" in markdown
    assert "5" in markdown


def test_save_weekly_markdown():
    """测试周报 Markdown 保存"""
    gen = ReportGenerator()
    content = "# 测试周报\n内容"
    path = gen.save_weekly_markdown(content, 2026, 5)
    assert path.exists()
    assert path.name == "2026-W05.md"
    assert path.read_text(encoding="utf-8") == content


def test_save_monthly_markdown():
    """测试月报 Markdown 保存"""
    gen = ReportGenerator()
    content = "# 测试月报\n内容"
    path = gen.save_monthly_markdown(content, "2026-01")
    assert path.exists()
    assert path.name == "2026-01.md"
    assert path.read_text(encoding="utf-8") == content
