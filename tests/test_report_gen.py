"""测试报告生成"""

import json

import pytest

from src.core.llm import LLMClient
from src.models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from src.services.report_gen import ReportGenerator


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
        overview="本周围绕两个审计项目推进资料核验和底稿收口，整体节奏保持稳定。",
        completed_work="本周完成项目A底稿复核、项目B资料清单初审，并同步补齐问题跟踪记录。",
        work_summary="本周工作以阶段性收口和新项目准备并行为主，关键依赖已经基本识别清楚。",
        next_plan="下周继续推进项目B补件核验，并启动项目C现场访谈准备。",
    )

    markdown = gen.render_weekly_markdown(report)
    assert "2026-W05" in markdown
    assert "## 本周总览" in markdown
    assert "## 本周完成内容" in markdown
    assert "## 本周工作小结" in markdown
    assert "## 下周工作计划" in markdown
    assert "项目A底稿复核" in markdown
    assert "项目C现场访谈准备" in markdown
    assert "category_summaries" not in markdown
    assert "风险与问题" not in markdown


def test_render_monthly_markdown():
    """测试月报 Markdown 渲染"""
    gen = ReportGenerator()

    report = MonthlyReportData(
        year_month="2026-01",
        overview="本月围绕审计计划执行、资料核验和报告整理持续推进，整体进度符合预期。",
        completed_work="本月完成年度审计计划拆解、两个项目的阶段性底稿整理以及一轮报告初稿编写。",
        work_summary="本月工作重点在于把前期分散事项收束成可交付成果，并提前暴露后续依赖。",
        next_plan="下月继续完善报告初稿，跟进整改反馈，并启动新项目进场准备。",
    )

    markdown = gen.render_monthly_markdown(report)
    assert "2026-01" in markdown
    assert "## 本月总览" in markdown
    assert "## 本月完成内容" in markdown
    assert "## 本月工作小结" in markdown
    assert "## 下月工作计划" in markdown
    assert "年度审计计划拆解" in markdown
    assert "启动新项目进场准备" in markdown
    assert "statistics" not in markdown
    assert "| 指标 | 数值 |" not in markdown


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


@pytest.fixture
def llm_client_for_prompt_tests():
    """构造一个不依赖外部 API 的 LLMClient 测试实例"""
    client = object.__new__(LLMClient)
    client.prompt_templates = {
        "weekly_prompt": (
            "周标签:{week_label}\n"
            "数据来源:{data_source}\n"
            "缺失日报:{missing_days}\n"
            "日报聚合上下文:\n{reports_summary}\n"
            "文件证据:{file_context}\n"
            "Schema:{schema}"
        ),
        "monthly_prompt": (
            "年月:{year_month}\n"
            "数据来源:{data_source}\n"
            "缺失日报:{missing_days}\n"
            "日报聚合上下文:\n{reports_summary}\n"
            "文件证据:{file_context}\n"
            "Schema:{schema}"
        ),
    }
    return client


def test_generate_weekly_report_prompt_contains_period_context_and_metadata(
    monkeypatch, llm_client_for_prompt_tests
):
    """周报 prompt 需要带入三段日报上下文、缺失日期和数据来源"""
    client = llm_client_for_prompt_tests
    captured: dict[str, str] = {}

    def fake_call(prompt: str, response_model: type[object]) -> WeeklyReportData:
        captured["prompt"] = prompt
        return WeeklyReportData(
            week_label="2026-W14",
            date_range="2026-03-30 ~ 2026-04-05",
            overview="本周总览。",
            completed_work="本周完成内容。",
            work_summary="本周工作小结。",
            next_plan="下周工作计划。",
        )

    monkeypatch.setattr(client, "_call_llm_with_json", fake_call)

    reports = [
        DailyReportData(
            date="2026-04-01",
            completed_work="完成项目A底稿复核。",
            work_summary="同步收口问题台账。",
            next_plan="继续跟进整改。",
        )
    ]

    client.generate_weekly_report(
        reports=reports,
        file_context="附件中有会议纪要。",
        year=2026,
        week=14,
        missing_days=["2026-04-03"],
        data_source="db",
    )

    prompt = captured["prompt"]
    assert "周标签:2026-W14" in prompt
    assert "数据来源:db" in prompt
    assert "缺失日报:2026-04-03" in prompt
    assert "## 2026-04-01" in prompt
    assert "### 今日工作完成内容" in prompt
    assert "### 今日工作小结" in prompt
    assert "### 明日工作计划" in prompt
    assert "完成项目A底稿复核。" in prompt


def test_generate_monthly_report_prompt_uses_empty_message_for_empty_reports(
    monkeypatch, llm_client_for_prompt_tests
):
    """月报 prompt 在没有日报时需要带入空日报文案和元数据"""
    client = llm_client_for_prompt_tests
    captured: dict[str, str] = {}

    def fake_call(prompt: str, response_model: type[object]) -> MonthlyReportData:
        captured["prompt"] = prompt
        return MonthlyReportData(
            year_month="2026-04",
            overview="本月总览。",
            completed_work="本月完成内容。",
            work_summary="本月工作小结。",
            next_plan="下月工作计划。",
        )

    monkeypatch.setattr(client, "_call_llm_with_json", fake_call)

    client.generate_monthly_report(
        reports=[],
        file_context="无文件补充。",
        year_month="2026-04",
        missing_days=["2026-04-04", "2026-04-18"],
        data_source="scan",
    )

    prompt = captured["prompt"]
    assert "年月:2026-04" in prompt
    assert "数据来源:scan" in prompt
    assert "缺失日报:2026-04-04、2026-04-18" in prompt
    assert "日报聚合上下文:\n无日报数据" in prompt
    assert "### 今日工作完成内容" not in prompt
    assert json.dumps(MonthlyReportData.model_json_schema(), ensure_ascii=False, indent=2) in prompt
