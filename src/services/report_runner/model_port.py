"""报告模型端口及现有 LLM client 的惰性 production adapter。"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Protocol, TypeAlias

from ...models.schemas import (
    DailyReportData,
    MonthlyReportData,
    WeeklyReportData,
)


@dataclass(frozen=True, slots=True)
class DailyGenerationRequest:
    user_input: str
    file_context: str
    yesterday_plan: str


@dataclass(frozen=True, slots=True)
class WeeklyGenerationRequest:
    reports: list[DailyReportData]
    file_context: str
    year: int
    week: int
    missing_days: list[str]
    data_source: str


@dataclass(frozen=True, slots=True)
class MonthlyGenerationRequest:
    reports: list[DailyReportData]
    file_context: str
    year_month: str
    missing_days: list[str]
    data_source: str


GenerationRequest: TypeAlias = (
    DailyGenerationRequest | WeeklyGenerationRequest | MonthlyGenerationRequest
)
GeneratedReport: TypeAlias = DailyReportData | WeeklyReportData | MonthlyReportData


class ReportModelPort(Protocol):
    def generate(self, request: GenerationRequest) -> GeneratedReport:
        ...


class _LLMClientProtocol(Protocol):
    def generate_report(
        self, *, user_input: str, file_context: str, yesterday_plan: str
    ) -> DailyReportData:
        ...

    def generate_weekly_report(
        self,
        *,
        reports: list[DailyReportData],
        file_context: str,
        year: int,
        week: int,
        missing_days: list[str],
        data_source: str,
    ) -> WeeklyReportData:
        ...

    def generate_monthly_report(
        self,
        *,
        reports: list[DailyReportData],
        file_context: str,
        year_month: str,
        missing_days: list[str],
        data_source: str,
    ) -> MonthlyReportData:
        ...


@dataclass(slots=True)
class LLMModelPort:
    """仅在首次 generate 时构造现有 LLM client。"""

    client_factory: Callable[[], _LLMClientProtocol]
    _client: _LLMClientProtocol | None = field(default=None, init=False)

    def generate(self, request: GenerationRequest) -> GeneratedReport:
        client = self._get_client()
        if isinstance(request, DailyGenerationRequest):
            return client.generate_report(
                user_input=request.user_input,
                file_context=request.file_context,
                yesterday_plan=request.yesterday_plan,
            )
        if isinstance(request, WeeklyGenerationRequest):
            return client.generate_weekly_report(
                reports=request.reports,
                file_context=request.file_context,
                year=request.year,
                week=request.week,
                missing_days=request.missing_days,
                data_source=request.data_source,
            )
        return client.generate_monthly_report(
            reports=request.reports,
            file_context=request.file_context,
            year_month=request.year_month,
            missing_days=request.missing_days,
            data_source=request.data_source,
        )

    def _get_client(self) -> _LLMClientProtocol:
        if self._client is None:
            self._client = self.client_factory()
        return self._client
