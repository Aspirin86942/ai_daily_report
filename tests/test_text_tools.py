"""测试文本处理工具"""

from datetime import date

import pytest

from src.models.schemas import DailyReportData
from src.utils.text_tools import (
    estimate_tokens,
    format_period_report_context,
    get_month_date_range,
    parse_week_label,
    truncate_text,
)


def test_truncate_text_short():
    """测试短文本不截断"""
    text = "hello"
    assert truncate_text(text, 100) == "hello"


def test_truncate_text_long():
    """测试长文本截断"""
    text = "a" * 100
    result = truncate_text(text, 50)
    assert len(result) < 100
    assert "截断" in result


def test_estimate_tokens():
    """测试 Token 估算"""
    assert estimate_tokens("Hello World") > 0
    assert estimate_tokens("") == 0


def test_parse_week_label_valid():
    """测试有效的周标签解析"""
    year, week = parse_week_label("2026-W05")
    assert year == 2026
    assert week == 5


def test_parse_week_label_w01():
    """测试第一周"""
    year, week = parse_week_label("2026-W01")
    assert year == 2026
    assert week == 1


def test_parse_week_label_high_week():
    """测试大周数"""
    year, week = parse_week_label("2020-W53")
    assert year == 2020
    assert week == 53


def test_parse_week_label_invalid_format():
    """测试无效格式"""
    with pytest.raises(ValueError, match="无效的周标签格式"):
        parse_week_label("2026-05")


def test_parse_week_label_invalid_week():
    """测试无效周数"""
    with pytest.raises(ValueError):
        parse_week_label("2026-W54")


def test_parse_week_label_non_numeric():
    """测试非数字周标签"""
    with pytest.raises(ValueError):
        parse_week_label("abcd-Wxy")


def test_get_month_date_range_feb():
    """测试二月日期范围"""
    start, end = get_month_date_range("2026-02")
    assert start == date(2026, 2, 1)
    assert end == date(2026, 2, 28)


def test_get_month_date_range_leap_year():
    """测试闰年二月"""
    start, end = get_month_date_range("2024-02")
    assert start == date(2024, 2, 1)
    assert end == date(2024, 2, 29)


def test_get_month_date_range_jan():
    """测试一月"""
    start, end = get_month_date_range("2026-01")
    assert start == date(2026, 1, 1)
    assert end == date(2026, 1, 31)


def test_get_month_date_range_dec():
    """测试十二月"""
    start, end = get_month_date_range("2026-12")
    assert start == date(2026, 12, 1)
    assert end == date(2026, 12, 31)


def test_get_month_date_range_invalid_format():
    """测试无效格式"""
    with pytest.raises(ValueError, match="无效的年月格式"):
        get_month_date_range("2026/01")


def test_get_month_date_range_invalid_month():
    """测试无效月份"""
    with pytest.raises(ValueError):
        get_month_date_range("2026-13")


def test_format_period_report_context_groups_daily_sections():
    """周期报告上下文需要按日期和三段标题拼接"""
    reports = [
        DailyReportData(
            date="2026-04-01",
            completed_work="完成项目A底稿复核，并更新问题跟踪表。",
            work_summary="当天主要完成底稿收口，确认后续整改口径。",
            next_plan="继续整理项目A遗留问题，并准备项目B进场材料。",
        ),
        DailyReportData(
            date="2026-04-02",
            completed_work="完成项目B资料清单初审，并与业务方确认缺失项。",
            work_summary="当天工作重点转向项目B准备，已梳理出需要补件的关键资料。",
            next_plan="跟进补件情况，同时开始整理周报聚合素材。",
        ),
    ]

    result = format_period_report_context(reports)

    assert "## 2026-04-01" in result
    assert "## 2026-04-02" in result
    assert "### 今日工作完成内容" in result
    assert "### 今日工作小结" in result
    assert "### 明日工作计划" in result
    assert "完成项目A底稿复核" in result
    assert "开始整理周报聚合素材" in result


def test_format_period_report_context_sorts_reports_by_date():
    """周期报告上下文需要先按日期升序排序"""
    reports = [
        DailyReportData(
            date="2026-04-03",
            completed_work="第三天内容。",
            work_summary="第三天小结。",
            next_plan="第三天计划。",
        ),
        DailyReportData(
            date="2026-04-01",
            completed_work="第一天内容。",
            work_summary="第一天小结。",
            next_plan="第一天计划。",
        ),
        DailyReportData(
            date="2026-04-02",
            completed_work="第二天内容。",
            work_summary="第二天小结。",
            next_plan="第二天计划。",
        ),
    ]

    result = format_period_report_context(reports)

    assert result.index("## 2026-04-01") < result.index("## 2026-04-02")
    assert result.index("## 2026-04-02") < result.index("## 2026-04-03")


def test_format_period_report_context_returns_empty_message_for_empty_reports():
    """空日报列表需要返回统一空文案"""
    assert format_period_report_context([]) == "无日报数据"
