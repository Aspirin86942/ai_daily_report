"""测试文本处理工具"""
import pytest
from datetime import date
from src.utils.text_tools import (
    truncate_text, estimate_tokens, parse_week_label, get_month_date_range
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


# --- parse_week_label ---

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
    """测试无效周数 (超出范围)"""
    with pytest.raises(ValueError):
        parse_week_label("2026-W54")


def test_parse_week_label_non_numeric():
    """测试非数字"""
    with pytest.raises(ValueError):
        parse_week_label("abcd-Wxy")


# --- get_month_date_range ---

def test_get_month_date_range_feb():
    """测试2月边界"""
    start, end = get_month_date_range("2026-02")
    assert start == date(2026, 2, 1)
    assert end == date(2026, 2, 28)


def test_get_month_date_range_leap_year():
    """测试闰年2月"""
    start, end = get_month_date_range("2024-02")
    assert start == date(2024, 2, 1)
    assert end == date(2024, 2, 29)


def test_get_month_date_range_jan():
    """测试1月"""
    start, end = get_month_date_range("2026-01")
    assert start == date(2026, 1, 1)
    assert end == date(2026, 1, 31)


def test_get_month_date_range_dec():
    """测试12月"""
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
