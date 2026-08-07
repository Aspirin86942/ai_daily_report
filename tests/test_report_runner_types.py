"""ReportRunner request/outcome 类型与非法组合约束。"""

from __future__ import annotations

from datetime import date
from pathlib import Path

import pytest

from src.models.schemas import DailyReportData
from src.services.report_runner.outcomes import (
    DatabaseEvidence,
    ErrorCode,
    PublicationReceipt,
    ReportError,
    ReportRunFailure,
    ReportRunSuccess,
    ScanEvidence,
)
from src.services.report_runner.period import ResolvedPeriod
from src.services.report_runner.requests import (
    DailyReportRunRequest,
    MonthlyReportRunRequest,
    WeeklyReportRunRequest,
)


def test_daily_request_has_no_source_field():
    request = DailyReportRunRequest(as_of_date=date(2026, 5, 25), save=True)

    assert not hasattr(request, "source")


def test_weekly_request_requires_source():
    with pytest.raises(TypeError):
        WeeklyReportRunRequest(as_of_date=date(2026, 5, 25), save=True)


@pytest.mark.parametrize("source", ["db", "scan"])
def test_weekly_request_accepts_db_or_scan(source: str):
    request = WeeklyReportRunRequest(
        as_of_date=date(2026, 5, 25), source=source, save=False
    )

    assert request.source == source


def test_period_request_rejects_unknown_source():
    with pytest.raises(ValueError, match="source must be 'db' or 'scan'"):
        MonthlyReportRunRequest(
            as_of_date=date(2026, 5, 25), source="api", save=False
        )


def test_monthly_request_accepts_db_or_scan():
    request = MonthlyReportRunRequest(
        as_of_date=date(2026, 5, 25), source="scan", save=False
    )

    assert request.source == "scan"


def test_success_outcome_fields():
    report = DailyReportData(
        date="2026-05-25",
        completed_work="x",
        work_summary="y",
        next_plan="z",
    )
    outcome = ReportRunSuccess(
        mode="daily",
        source="scan",
        status="ok",
        period=ResolvedPeriod(
            mode="daily",
            source="scan",
            start_date=date(2026, 5, 24),
            end_date=date(2026, 5, 25),
            display_label="2026-05-25",
            as_of_date=date(2026, 5, 25),
        ),
        report=report,
        markdown="# 预览",
        warnings=[],
        source_evidence=ScanEvidence(
            status="ok",
            source_file_count=1,
            success_count=1,
            scan_run_id=1,
            context_run_id=1,
        ),
        publication=PublicationReceipt(
            requested=True,
            sqlite_state="committed",
            markdown_state="written",
            markdown_path=Path("out/2026-05-25.md"),
        ),
    )

    assert outcome.outcome == "success"
    assert outcome.report is report


def test_failure_outcome_carries_phase_and_error_code():
    failure = ReportRunFailure(
        mode="weekly",
        source="db",
        period=None,
        phase="source",
        error=ReportError(
            error_code=ErrorCode.NO_SOURCE_REPORTS,
            message="未找到日报数据",
            retryable=False,
        ),
        warnings=[],
        source_evidence=None,
        publication=PublicationReceipt(
            requested=True,
            sqlite_state="not_attempted",
            markdown_state="not_attempted",
        ),
    )

    assert failure.outcome == "failure"
    assert failure.phase == "source"
    assert failure.error.error_code is ErrorCode.NO_SOURCE_REPORTS


def test_database_evidence_lists_missing_days():
    evidence = DatabaseEvidence(report_count=1, missing_days=["2026-05-25"])

    assert evidence.report_count == 1
    assert evidence.missing_days == ["2026-05-25"]
