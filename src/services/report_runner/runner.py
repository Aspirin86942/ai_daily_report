"""ReportRunner：单一应用 seam 与 daily/period 私有 recipes。

分层：orchestration —— 报告运行配方与错误/发布结果编排。
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date, datetime, time
from pathlib import Path
from typing import Callable, Literal, Protocol, cast

from ...models.scanner_contract import Diagnostic
from ...models.schemas import (
    DailyReportData,
    MonthlyReportData,
    WeeklyReportData,
)
from ..context_engine import ContextBuildResult
from ..context_scheduler import ContextScheduleRequest
from .input_adapter import DailyInputAdapter
from .model_port import (
    DailyGenerationRequest,
    GeneratedReport,
    GenerationRequest,
    MonthlyGenerationRequest,
    ReportModelPort,
    WeeklyGenerationRequest,
)
from .outcomes import (
    DatabaseEvidence,
    ErrorCode,
    PublicationReceipt,
    ReportError,
    ReportRunFailure,
    ReportRunOutcome,
    ReportRunSuccess,
    ScanEvidence,
)
from .period import (
    ReportMode,
    ResolvedPeriod,
    resolve_daily_period,
    resolve_monthly_period,
    resolve_weekly_period,
)
from .requests import (
    DailyReportRunRequest,
    MonthlyReportRunRequest,
    ReportRunRequest,
    ReportSource,
    WeeklyReportRunRequest,
)

PeriodMode = Literal["weekly", "monthly"]


class _ContextSchedulerPort(Protocol):
    def build_context(self, request: ContextScheduleRequest) -> ContextBuildResult:
        ...


class _ReportStore(Protocol):
    def get_yesterday_plan(self, target_date: datetime | None = None) -> str:
        ...

    def get_week_reports(
        self, year: int, week: int
    ) -> tuple[list[DailyReportData], list[str]]:
        ...

    def get_reports_in_range(
        self, start_date: date, end_date: date
    ) -> tuple[list[DailyReportData], list[str]]:
        ...

    def save_report(self, report: DailyReportData) -> object:
        ...

    def save_weekly_report(self, report: WeeklyReportData) -> object:
        ...

    def save_monthly_report(self, report: MonthlyReportData) -> object:
        ...


class _ReportRenderer(Protocol):
    def render_markdown(self, report: DailyReportData) -> str:
        ...

    def render_weekly_markdown(self, report: WeeklyReportData) -> str:
        ...

    def render_monthly_markdown(self, report: MonthlyReportData) -> str:
        ...

    def save_markdown(self, content: str, report_date: str) -> Path:
        ...

    def save_weekly_markdown(self, content: str, year: int, week: int) -> Path:
        ...

    def save_monthly_markdown(self, content: str, year_month: str) -> Path:
        ...


@dataclass(frozen=True, slots=True)
class _AcceptedScan:
    file_context: str
    status: Literal["ok", "partial"]
    warnings: list[Diagnostic]
    evidence: ScanEvidence


@dataclass(frozen=True, slots=True)
class _PeriodSourceData:
    reports: list[DailyReportData]
    missing_days: list[str]
    file_context: str
    status: Literal["ok", "partial"]
    warnings: list[Diagnostic]
    evidence: ScanEvidence | DatabaseEvidence


def _not_attempted(requested: bool) -> PublicationReceipt:
    return PublicationReceipt(
        requested=requested,
        sqlite_state="not_attempted",
        markdown_state="not_attempted",
    )


@dataclass(slots=True)
class ReportRunner:
    scheduler: _ContextSchedulerPort
    store: _ReportStore
    renderer: _ReportRenderer
    model_port: ReportModelPort
    daily_input: DailyInputAdapter

    def run(self, request: ReportRunRequest) -> ReportRunOutcome:
        """运行一个封闭 request variant；预期业务失败返回 typed outcome。"""
        if isinstance(request, DailyReportRunRequest):
            return self._run_daily(request)
        if isinstance(request, WeeklyReportRunRequest):
            return self._run_weekly(request)
        if isinstance(request, MonthlyReportRunRequest):
            return self._run_monthly(request)
        raise TypeError(f"unknown request variant: {type(request).__name__}")

    def _run_daily(self, request: DailyReportRunRequest) -> ReportRunOutcome:
        period = resolve_daily_period(request.as_of_date)
        accepted = self._build_scan_context(period, requested=request.save)
        if isinstance(accepted, ReportRunFailure):
            return accepted

        try:
            target = datetime.combine(request.as_of_date, time.min)
            yesterday_plan = self.store.get_yesterday_plan(target_date=target)
        except Exception as exc:
            return self._failure(
                mode="daily",
                source="scan",
                period=period,
                phase="source",
                error_code=ErrorCode.SOURCE_READ_FAILED,
                message=str(exc),
                retryable=False,
                requested=request.save,
                warnings=accepted.warnings,
                evidence=accepted.evidence,
                cause=type(exc).__name__,
            )

        user_input = request.user_input
        if user_input is None:
            user_input = self.daily_input.read()
        if not user_input.strip():
            return self._failure(
                mode="daily",
                source="scan",
                period=period,
                phase="request",
                error_code=ErrorCode.EMPTY_DAILY_INPUT,
                message="未输入工作内容",
                retryable=False,
                requested=request.save,
                warnings=accepted.warnings,
                evidence=accepted.evidence,
            )

        generated = self._generate(
            DailyGenerationRequest(
                user_input=user_input,
                file_context=accepted.file_context,
                yesterday_plan=yesterday_plan,
            ),
            expected_type=DailyReportData,
            mode="daily",
            source="scan",
            period=period,
            requested=request.save,
            warnings=accepted.warnings,
            evidence=accepted.evidence,
        )
        if isinstance(generated, ReportRunFailure):
            return generated
        report = cast(DailyReportData, generated)
        if request.report_date_override:
            report = report.model_copy(update={"date": request.report_date_override})

        return self._render_and_publish(
            mode="daily",
            source="scan",
            period=period,
            status=accepted.status,
            report=report,
            save=request.save,
            render=self.renderer.render_markdown,
            save_sqlite=self.store.save_report,
            save_markdown=lambda markdown: self.renderer.save_markdown(
                markdown, report.date
            ),
            warnings=accepted.warnings,
            evidence=accepted.evidence,
        )

    def _run_weekly(self, request: WeeklyReportRunRequest) -> ReportRunOutcome:
        try:
            period = resolve_weekly_period(
                request.as_of_date,
                request.week_label,
                source=request.source,
            )
        except ValueError as exc:
            return self._failure(
                mode="weekly",
                source=request.source,
                period=None,
                phase="request",
                error_code=ErrorCode.INVALID_WEEK,
                message=str(exc),
                retryable=False,
                requested=request.save,
                cause=type(exc).__name__,
            )
        return self._run_period(
            mode="weekly",
            period=period,
            save=request.save,
            supplemental=request.supplemental_input,
        )

    def _run_monthly(self, request: MonthlyReportRunRequest) -> ReportRunOutcome:
        try:
            period = resolve_monthly_period(
                request.as_of_date,
                request.year_month,
                source=request.source,
            )
        except ValueError as exc:
            return self._failure(
                mode="monthly",
                source=request.source,
                period=None,
                phase="request",
                error_code=ErrorCode.INVALID_MONTH,
                message=str(exc),
                retryable=False,
                requested=request.save,
                cause=type(exc).__name__,
            )
        return self._run_period(
            mode="monthly",
            period=period,
            save=request.save,
            supplemental=request.supplemental_input,
        )

    def _run_period(
        self,
        *,
        mode: PeriodMode,
        period: ResolvedPeriod,
        save: bool,
        supplemental: str | None,
    ) -> ReportRunOutcome:
        source_data = self._load_period_source(mode, period, requested=save)
        if isinstance(source_data, ReportRunFailure):
            return source_data

        file_context = source_data.file_context
        if supplemental is not None and supplemental.strip():
            file_context = f"{file_context}\n\n---\n\n用户补充: {supplemental}"

        if mode == "weekly":
            year, week, _ = period.start_date.isocalendar()
            generation_request: GenerationRequest = WeeklyGenerationRequest(
                reports=source_data.reports,
                file_context=file_context,
                year=year,
                week=week,
                missing_days=source_data.missing_days,
                data_source=period.source,
            )
            expected_type = WeeklyReportData
        else:
            generation_request = MonthlyGenerationRequest(
                reports=source_data.reports,
                file_context=file_context,
                year_month=period.display_label,
                missing_days=source_data.missing_days,
                data_source=period.source,
            )
            expected_type = MonthlyReportData

        generated = self._generate(
            generation_request,
            expected_type=expected_type,
            mode=mode,
            source=period.source,
            period=period,
            requested=save,
            warnings=source_data.warnings,
            evidence=source_data.evidence,
        )
        if isinstance(generated, ReportRunFailure):
            return generated

        if mode == "weekly":
            report = cast(WeeklyReportData, generated)
            return self._render_and_publish(
                mode=mode,
                source=period.source,
                period=period,
                status=source_data.status,
                report=report,
                save=save,
                render=self.renderer.render_weekly_markdown,
                save_sqlite=self.store.save_weekly_report,
                save_markdown=lambda markdown: self.renderer.save_weekly_markdown(
                    markdown, year, week
                ),
                warnings=source_data.warnings,
                evidence=source_data.evidence,
            )

        report = cast(MonthlyReportData, generated)
        return self._render_and_publish(
            mode=mode,
            source=period.source,
            period=period,
            status=source_data.status,
            report=report,
            save=save,
            render=self.renderer.render_monthly_markdown,
            save_sqlite=self.store.save_monthly_report,
            save_markdown=lambda markdown: self.renderer.save_monthly_markdown(
                markdown, period.display_label
            ),
            warnings=source_data.warnings,
            evidence=source_data.evidence,
        )

    def _load_period_source(
        self,
        mode: PeriodMode,
        period: ResolvedPeriod,
        *,
        requested: bool,
    ) -> _PeriodSourceData | ReportRunFailure:
        if period.source == "scan":
            accepted = self._build_scan_context(period, requested=requested)
            if isinstance(accepted, ReportRunFailure):
                return accepted
            return _PeriodSourceData(
                reports=[],
                missing_days=[],
                file_context=accepted.file_context,
                status=accepted.status,
                warnings=accepted.warnings,
                evidence=accepted.evidence,
            )

        try:
            reports, missing_days = self._read_period_reports(mode, period)
        except Exception as exc:
            return self._failure(
                mode=mode,
                source="db",
                period=period,
                phase="source",
                error_code=ErrorCode.SOURCE_READ_FAILED,
                message=str(exc),
                retryable=False,
                requested=requested,
                cause=type(exc).__name__,
            )
        if not reports:
            return self._failure(
                mode=mode,
                source="db",
                period=period,
                phase="source",
                error_code=ErrorCode.NO_SOURCE_REPORTS,
                message=f"未找到 {period.display_label} 的日报数据",
                retryable=False,
                requested=requested,
            )

        reports = sorted(reports, key=lambda report: report.date)
        missing_days = sorted(missing_days)
        return _PeriodSourceData(
            reports=reports,
            missing_days=missing_days,
            file_context="无文件证据",
            status="ok",
            warnings=[],
            evidence=DatabaseEvidence(
                report_count=len(reports),
                missing_days=list(missing_days),
            ),
        )

    def _build_scan_context(
        self, period: ResolvedPeriod, *, requested: bool
    ) -> _AcceptedScan | ReportRunFailure:
        schedule = ContextScheduleRequest(
            report_mode=period.mode,
            source="scan",
            start_date=period.start_date,
            end_date=period.end_date,
        )
        try:
            result = self.scheduler.build_context(schedule)
        except Exception as exc:
            return self._failure(
                mode=period.mode,
                source="scan",
                period=period,
                phase="source",
                error_code=ErrorCode.SCANNER_FAILED,
                message=str(exc),
                retryable=False,
                requested=requested,
                cause=type(exc).__name__,
            )

        evidence = ScanEvidence(
            status=result.status,
            source_file_count=result.summary.source_file_count,
            success_count=result.summary.success_count,
            scan_run_id=result.scan_run_id,
            context_run_id=result.context_run_id,
        )
        if result.status == "error":
            diagnostic = result.error
            message = (
                diagnostic.message
                if diagnostic is not None
                else "context engine returned an invalid error result"
            )
            return self._failure(
                mode=period.mode,
                source="scan",
                period=period,
                phase="source",
                error_code=ErrorCode.SCANNER_FAILED,
                message=message,
                retryable=diagnostic.retryable if diagnostic is not None else False,
                requested=requested,
                warnings=list(result.warnings),
                evidence=evidence,
                cause=diagnostic.error_code if diagnostic is not None else None,
            )
        return _AcceptedScan(
            file_context=result.file_context,
            status=result.status,
            warnings=list(result.warnings),
            evidence=evidence,
        )

    def _read_period_reports(
        self, mode: PeriodMode, period: ResolvedPeriod
    ) -> tuple[list[DailyReportData], list[str]]:
        if mode == "weekly":
            year, week, _ = period.start_date.isocalendar()
            return self.store.get_week_reports(year, week)
        return self.store.get_reports_in_range(period.start_date, period.end_date)

    def _generate(
        self,
        request: GenerationRequest,
        *,
        expected_type: type[DailyReportData]
        | type[WeeklyReportData]
        | type[MonthlyReportData],
        mode: ReportMode,
        source: ReportSource,
        period: ResolvedPeriod,
        requested: bool,
        warnings: list[Diagnostic],
        evidence: ScanEvidence | DatabaseEvidence,
    ) -> GeneratedReport | ReportRunFailure:
        try:
            report = self.model_port.generate(request)
        except Exception as exc:
            return self._failure(
                mode=mode,
                source=source,
                period=period,
                phase="generation",
                error_code=ErrorCode.LLM_GENERATION_FAILED,
                message=str(exc),
                retryable=False,
                requested=requested,
                warnings=warnings,
                evidence=evidence,
                cause=type(exc).__name__,
            )
        if not isinstance(report, expected_type):
            raise TypeError(
                f"{mode} model returned {type(report).__name__}, "
                f"expected {expected_type.__name__}"
            )
        return report

    def _render_and_publish(
        self,
        *,
        mode: ReportMode,
        source: ReportSource,
        period: ResolvedPeriod,
        status: Literal["ok", "partial"],
        report: GeneratedReport,
        save: bool,
        render: Callable[[object], str],
        save_sqlite: Callable[[object], object],
        save_markdown: Callable[[str], Path],
        warnings: list[Diagnostic],
        evidence: ScanEvidence | DatabaseEvidence,
    ) -> ReportRunOutcome:
        try:
            markdown = render(report)
        except Exception as exc:
            return self._failure(
                mode=mode,
                source=source,
                period=period,
                phase="render",
                error_code=ErrorCode.MARKDOWN_RENDER_FAILED,
                message=str(exc),
                retryable=False,
                requested=save,
                warnings=warnings,
                evidence=evidence,
                cause=type(exc).__name__,
            )

        receipt = _not_attempted(save)
        if save:
            try:
                save_sqlite(report)
            except Exception as exc:
                receipt = PublicationReceipt(
                    requested=True,
                    sqlite_state="failed",
                    markdown_state="not_attempted",
                )
                return self._failure(
                    mode=mode,
                    source=source,
                    period=period,
                    phase="sqlite_publish",
                    error_code=ErrorCode.SQLITE_PUBLISH_FAILED,
                    message=str(exc),
                    retryable=False,
                    requested=True,
                    warnings=warnings,
                    evidence=evidence,
                    cause=type(exc).__name__,
                    publication=receipt,
                )
            receipt = PublicationReceipt(
                requested=True,
                sqlite_state="committed",
                markdown_state="not_attempted",
            )
            try:
                path = save_markdown(markdown)
            except Exception as exc:
                receipt = PublicationReceipt(
                    requested=True,
                    sqlite_state="committed",
                    markdown_state="failed",
                )
                return self._failure(
                    mode=mode,
                    source=source,
                    period=period,
                    phase="markdown_publish",
                    error_code=ErrorCode.MARKDOWN_PUBLISH_FAILED,
                    message=str(exc),
                    retryable=False,
                    requested=True,
                    warnings=warnings,
                    evidence=evidence,
                    cause=type(exc).__name__,
                    publication=receipt,
                )
            receipt = PublicationReceipt(
                requested=True,
                sqlite_state="committed",
                markdown_state="written",
                markdown_path=Path(path),
            )

        return ReportRunSuccess(
            mode=mode,
            source=source,
            status=status,
            period=period,
            report=report,
            markdown=markdown,
            warnings=list(warnings),
            source_evidence=evidence,
            publication=receipt,
        )

    @staticmethod
    def _failure(
        *,
        mode: ReportMode,
        source: ReportSource,
        period: ResolvedPeriod | None,
        phase: Literal[
            "request",
            "source",
            "generation",
            "render",
            "sqlite_publish",
            "markdown_publish",
        ],
        error_code: ErrorCode,
        message: str,
        retryable: bool,
        requested: bool,
        warnings: list[Diagnostic] | None = None,
        evidence: ScanEvidence | DatabaseEvidence | None = None,
        cause: str | None = None,
        publication: PublicationReceipt | None = None,
    ) -> ReportRunFailure:
        return ReportRunFailure(
            mode=mode,
            source=source,
            period=period,
            phase=phase,
            error=ReportError(
                error_code=error_code,
                message=message,
                retryable=retryable,
                cause=cause,
            ),
            warnings=list(warnings or []),
            source_evidence=evidence,
            publication=publication or _not_attempted(requested),
        )
