"""测试唯一的应用级 ContextScheduler/ContextBuildResult 边界。"""

from dataclasses import fields
from datetime import date

import pytest

from src.models.scanner_contract import (
    ContextEnvelope,
    ContextSummary,
    Diagnostic,
)
from src.services.context_scheduler import (
    ContextBuildResult,
    ContextScheduleRequest,
    ContextScheduler,
)


def _summary() -> ContextSummary:
    return ContextSummary(
        source_file_count=2,
        success_count=2,
        timeout_count=0,
        included_file_count=2,
        omitted_file_count=0,
        error_file_count=0,
        input_chars=20,
        output_chars=20,
        total_duration_ms=9,
        discovery_duration_ms=1,
        parse_duration_ms=5,
        compression_duration_ms=1,
    )


def _diagnostic(message: str = "synthetic degradation") -> Diagnostic:
    return Diagnostic(
        error_code="PARSER_FAILED",
        message=message,
        retryable=False,
        stage="parse",
        file_path=None,
        backend=None,
    )


def _envelope(status: str) -> ContextEnvelope:
    warning = _diagnostic()
    if status == "error":
        return ContextEnvelope(
            contract="ai_daily_context",
            protocol_version=1,
            request_id="11111111-1111-4111-8111-111111111111",
            engine_version="test-v1",
            engine_build="test-build",
            status="error",
            file_context="",
            summary=_summary(),
            scan_run_id=7,
            context_run_id=None,
            warnings=[],
            error=_diagnostic("synthetic terminal failure"),
        )
    return ContextEnvelope(
        contract="ai_daily_context",
        protocol_version=1,
        request_id="11111111-1111-4111-8111-111111111111",
        engine_version="test-v1",
        engine_build="test-build",
        status=status,
        file_context="synthetic final context",
        summary=_summary(),
        scan_run_id=7,
        context_run_id=8,
        warnings=[warning] if status == "partial" else [],
        error=None,
    )


class FakeEngine:
    def __init__(self, envelope: ContextEnvelope) -> None:
        self.envelope = envelope
        self.calls: list[ContextScheduleRequest] = []

    def build_context(self, request: ContextScheduleRequest) -> ContextEnvelope:
        self.calls.append(request)
        return self.envelope


def _request() -> ContextScheduleRequest:
    return ContextScheduleRequest(
        report_mode="daily",
        source="scan",
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
    )


@pytest.mark.parametrize("status", ["ok", "partial", "error"])
def test_scheduler_maps_one_engine_envelope_to_small_result(status: str) -> None:
    engine = FakeEngine(_envelope(status))

    result = ContextScheduler(engine=engine).build_context(_request())

    assert isinstance(result, ContextBuildResult)
    assert result.status == status
    assert result.summary == engine.envelope.summary
    assert result.scan_run_id == engine.envelope.scan_run_id
    assert result.context_run_id == engine.envelope.context_run_id
    assert result.warnings == engine.envelope.warnings
    assert result.error == engine.envelope.error
    assert len(engine.calls) == 1
    assert {item.name for item in fields(result)} == {
        "file_context",
        "status",
        "summary",
        "scan_run_id",
        "context_run_id",
        "warnings",
        "error",
    }
    assert not hasattr(result, "scan_result")
    assert not hasattr(result, "compressed_context")
    assert not hasattr(result, "decisions")


def test_scheduler_validates_before_calling_the_selected_engine() -> None:
    engine = FakeEngine(_envelope("ok"))
    invalid = ContextScheduleRequest(
        report_mode="daily",
        source="db",
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
    )

    with pytest.raises(ValueError, match="unsupported context source"):
        ContextScheduler(engine=engine).build_context(invalid)

    assert engine.calls == []


def test_error_envelope_is_returned_without_a_second_engine_call() -> None:
    rust_engine = FakeEngine(_envelope("error"))

    result = ContextScheduler(engine=rust_engine).build_context(_request())

    assert result.status == "error"
    assert result.file_context == ""
    assert result.error is not None
    assert len(rust_engine.calls) == 1
