"""封闭的 report-run request variants。"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date, timedelta
from typing import Literal, TypeAlias

from .context import RunContext, parse_report_date

ReportSource: TypeAlias = Literal["db", "scan"]


def _validate_source(source: str) -> None:
    if source not in {"db", "scan"}:
        raise ValueError("source must be 'db' or 'scan'")


@dataclass(frozen=True, slots=True)
class DailyReportRunRequest:
    as_of_date: date
    save: bool
    user_input: str | None = None
    report_date_override: str | None = None
    context: RunContext | None = None

    def __post_init__(self) -> None:
        if self.report_date_override is not None:
            target = parse_report_date(self.report_date_override)
            if self.context is not None and self.context.target_date != target:
                raise ValueError("date override does not match the frozen context")
        if self.target_date >= self.as_of_date:
            raise ValueError("target date must be a completed calendar day")

    @property
    def target_date(self) -> date:
        if self.context is not None:
            return self.context.target_date
        if self.report_date_override is not None:
            return parse_report_date(self.report_date_override)
        return self.as_of_date - timedelta(days=1)


@dataclass(frozen=True, slots=True)
class WeeklyReportRunRequest:
    as_of_date: date
    source: ReportSource
    save: bool
    week_label: str | None = None
    supplemental_input: str | None = None

    def __post_init__(self) -> None:
        _validate_source(self.source)


@dataclass(frozen=True, slots=True)
class MonthlyReportRunRequest:
    as_of_date: date
    source: ReportSource
    save: bool
    year_month: str | None = None
    supplemental_input: str | None = None

    def __post_init__(self) -> None:
        _validate_source(self.source)


ReportRunRequest: TypeAlias = (
    DailyReportRunRequest | WeeklyReportRunRequest | MonthlyReportRunRequest
)
