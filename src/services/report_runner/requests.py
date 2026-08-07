"""封闭的 report-run request variants。"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date
from typing import Literal, TypeAlias

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
