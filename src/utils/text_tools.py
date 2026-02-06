"""文本处理工具"""
from datetime import date
import calendar


def truncate_text(text: str, max_chars: int) -> str:
    """智能截断文本

    Args:
        text: 原始文本
        max_chars: 最大字符数

    Returns:
        截断后的文本
    """
    if len(text) <= max_chars:
        return text

    # 截断并添加省略标记
    return text[:max_chars] + "\n...(内容过长已截断)"


def estimate_tokens(text: str) -> int:
    """估算 Token 数量

    简单估算: 中文约 1 字符 = 1 token, 英文约 4 字符 = 1 token

    Args:
        text: 文本内容

    Returns:
        估算的 token 数量
    """
    # 简化估算: 按字符数除以 4
    return len(text) // 4


def parse_week_label(label: str) -> tuple[int, int]:
    """解析 ISO 周标签

    Args:
        label: 周标签 (如 "2026-W05")

    Returns:
        (year, week) 元组

    Raises:
        ValueError: 标签格式错误或周数无效
    """
    parts = label.split("-W")
    if len(parts) != 2:
        raise ValueError(f"无效的周标签格式: {label}，应为 YYYY-Wnn")

    try:
        year = int(parts[0])
        week = int(parts[1])
    except ValueError:
        raise ValueError(f"无效的周标签格式: {label}，应为 YYYY-Wnn")

    # 用 fromisocalendar 校验周数是否合法
    date.fromisocalendar(year, week, 1)

    return year, week


def get_month_date_range(year_month: str) -> tuple[date, date]:
    """获取月份的起止日期

    Args:
        year_month: 年月字符串 (如 "2026-02")

    Returns:
        (start_date, end_date) 元组

    Raises:
        ValueError: 格式错误
    """
    parts = year_month.split("-")
    if len(parts) != 2:
        raise ValueError(f"无效的年月格式: {year_month}，应为 YYYY-MM")

    try:
        year = int(parts[0])
        month = int(parts[1])
    except ValueError:
        raise ValueError(f"无效的年月格式: {year_month}，应为 YYYY-MM")

    start = date(year, month, 1)
    last_day = calendar.monthrange(year, month)[1]
    end = date(year, month, last_day)

    return start, end
