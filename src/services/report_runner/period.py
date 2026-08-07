"""一次报告运行中各阶段共享的已解析日期范围。"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date, timedelta
from typing import Literal, TypeAlias

from ...utils.text_tools import get_month_date_range, parse_week_label
from .requests import ReportSource

ReportMode: TypeAlias = Literal["daily", "weekly", "monthly"]


@dataclass(frozen=True, slots=True)
class ResolvedPeriod:
    mode: ReportMode
    source: ReportSource
    start_date: date
    end_date: date
    display_label: str
    as_of_date: date


def resolve_daily_period(as_of_date: date) -> ResolvedPeriod:
    return ResolvedPeriod(
        mode="daily",
        source="scan",
        start_date=as_of_date - timedelta(days=1),
        end_date=as_of_date,
        display_label=as_of_date.isoformat(),
        as_of_date=as_of_date,
    )


def resolve_weekly_period(
    as_of_date: date,
    week_label: str | None,
    *,
    source: ReportSource = "scan",
) -> ResolvedPeriod:
    if week_label:
        year, week = parse_week_label(week_label)
    else:
        year, week, _ = as_of_date.isocalendar()
    monday = date.fromisocalendar(year, week, 1)
    sunday = date.fromisocalendar(year, week, 7)
    return ResolvedPeriod(
        mode="weekly",
        source=source,
        start_date=monday,
        end_date=sunday,
        display_label=f"{year}-W{week:02d}",
        as_of_date=as_of_date,
    )


def resolve_monthly_period(
    as_of_date: date,
    year_month: str | None,
    *,
    source: ReportSource = "scan",
) -> ResolvedPeriod:
    label = year_month or as_of_date.strftime("%Y-%m")
    start, end = get_month_date_range(label)
    return ResolvedPeriod(
        mode="monthly",
        source=source,
        start_date=start,
        end_date=end,
        display_label=label,
        as_of_date=as_of_date,
    )
