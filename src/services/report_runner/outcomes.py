"""ReportRunner typed outcomes、publication receipt 与错误模型。"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Literal, TypeAlias

from ...models.scanner_contract import ContextStatus, Diagnostic
from ...models.schemas import (
    DailyReportData,
    MonthlyReportData,
    WeeklyReportData,
)
from .period import ReportMode, ResolvedPeriod
from .requests import ReportSource


class ErrorCode(str, Enum):
    INVALID_WEEK = "INVALID_WEEK"
    INVALID_MONTH = "INVALID_MONTH"
    EMPTY_DAILY_INPUT = "EMPTY_DAILY_INPUT"
    NO_SOURCE_REPORTS = "NO_SOURCE_REPORTS"
    SCANNER_FAILED = "SCANNER_FAILED"
    SOURCE_READ_FAILED = "SOURCE_READ_FAILED"
    LLM_GENERATION_FAILED = "LLM_GENERATION_FAILED"
    MARKDOWN_RENDER_FAILED = "MARKDOWN_RENDER_FAILED"
    SQLITE_PUBLISH_FAILED = "SQLITE_PUBLISH_FAILED"
    MARKDOWN_PUBLISH_FAILED = "MARKDOWN_PUBLISH_FAILED"


FailurePhase: TypeAlias = Literal[
    "request",
    "source",
    "generation",
    "render",
    "sqlite_publish",
    "markdown_publish",
]
SQLitePublicationState: TypeAlias = Literal[
    "not_attempted", "committed", "failed"
]
MarkdownPublicationState: TypeAlias = Literal[
    "not_attempted", "written", "failed"
]
ReportData: TypeAlias = DailyReportData | WeeklyReportData | MonthlyReportData


@dataclass(frozen=True, slots=True)
class ReportError:
    error_code: ErrorCode
    message: str
    retryable: bool
    cause: str | None = None


@dataclass(frozen=True, slots=True)
class ScanEvidence:
    status: ContextStatus
    source_file_count: int
    success_count: int
    scan_run_id: int | None
    context_run_id: int | None


@dataclass(frozen=True, slots=True)
class DatabaseEvidence:
    report_count: int
    missing_days: list[str]


@dataclass(frozen=True, slots=True)
class PublicationReceipt:
    requested: bool
    sqlite_state: SQLitePublicationState
    markdown_state: MarkdownPublicationState
    markdown_path: Path | None = None


@dataclass(frozen=True, slots=True)
class ReportRunSuccess:
    mode: ReportMode
    source: ReportSource
    status: Literal["ok", "partial"]
    period: ResolvedPeriod
    report: ReportData
    markdown: str
    source_evidence: ScanEvidence | DatabaseEvidence
    publication: PublicationReceipt
    warnings: list[Diagnostic] = field(default_factory=list)
    outcome: Literal["success"] = field(default="success", init=False)


@dataclass(frozen=True, slots=True)
class ReportRunFailure:
    mode: ReportMode
    source: ReportSource
    period: ResolvedPeriod | None
    phase: FailurePhase
    error: ReportError
    publication: PublicationReceipt
    warnings: list[Diagnostic] = field(default_factory=list)
    source_evidence: ScanEvidence | DatabaseEvidence | None = None
    outcome: Literal["failure"] = field(default="failure", init=False)


ReportRunOutcome: TypeAlias = ReportRunSuccess | ReportRunFailure
