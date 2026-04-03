"""文本处理工具。"""

import calendar
from datetime import date

from ..models.schemas import DailyReportData


def truncate_text(text: str, max_chars: int) -> str:
    """智能截断文本。"""
    if len(text) <= max_chars:
        return text

    return text[:max_chars] + "\n...(内容过长已截断)"


def estimate_tokens(text: str) -> int:
    """粗略估算文本 token 数。"""
    return len(text) // 4


def parse_week_label(label: str) -> tuple[int, int]:
    """解析 ISO 周标签。"""
    parts = label.split("-W")
    if len(parts) != 2:
        raise ValueError(f"无效的周标签格式: {label}，应为 YYYY-Wnn")

    try:
        year = int(parts[0])
        week = int(parts[1])
    except ValueError as exc:
        raise ValueError(f"无效的周标签格式: {label}，应为 YYYY-Wnn") from exc

    date.fromisocalendar(year, week, 1)
    return year, week


def get_month_date_range(year_month: str) -> tuple[date, date]:
    """获取月份起止日期。"""
    parts = year_month.split("-")
    if len(parts) != 2:
        raise ValueError(f"无效的年月格式: {year_month}，应为 YYYY-MM")

    try:
        year = int(parts[0])
        month = int(parts[1])
    except ValueError as exc:
        raise ValueError(f"无效的年月格式: {year_month}，应为 YYYY-MM") from exc

    start = date(year, month, 1)
    last_day = calendar.monthrange(year, month)[1]
    end = date(year, month, last_day)
    return start, end


def format_period_report_context(reports: list[DailyReportData]) -> str:
    """按日期拼接日报文本，供周报和月报聚合使用。"""
    if not reports:
        return "无日报数据"

    sections: list[str] = []
    for report in sorted(reports, key=lambda item: item.date):
        sections.append(
            f"## {report.date}\n"
            f"### 今日工作完成内容\n{report.completed_work}\n\n"
            f"### 今日工作小结\n{report.work_summary}\n\n"
            f"### 明日工作计划\n{report.next_plan}"
        )

    return "\n\n".join(sections)
