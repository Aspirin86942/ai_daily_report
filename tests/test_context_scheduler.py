"""ContextScheduler 的单一路径应用编排测试。"""

from __future__ import annotations

from datetime import date
from types import SimpleNamespace

import pytest

import src.services.context_scheduler as scheduler_module
from src.models.scanner_contract import ContextEnvelope, ContextSummary
from src.services.context_scheduler import ContextScheduleRequest, ContextScheduler


def _request() -> ContextScheduleRequest:
    return ContextScheduleRequest(
        report_mode="daily",
        source="scan",
        start_date=date(2026, 7, 16),
        end_date=date(2026, 7, 16),
    )


def _envelope() -> ContextEnvelope:
    return ContextEnvelope(
        contract="ai_daily_context",
        protocol_version=1,
        request_id="11111111-1111-4111-8111-111111111111",
        engine_version="0.1.0",
        engine_build="test-build",
        status="ok",
        file_context="synthetic context",
        summary=ContextSummary(
            source_file_count=1,
            success_count=1,
            timeout_count=0,
            included_file_count=1,
            omitted_file_count=0,
            error_file_count=0,
            input_chars=17,
            output_chars=17,
            total_duration_ms=3,
            discovery_duration_ms=1,
            parse_duration_ms=1,
            compression_duration_ms=1,
        ),
        scan_run_id=7,
        context_run_id=8,
        warnings=[],
        error=None,
    )


class StubEngine:
    def __init__(self) -> None:
        self.requests: list[ContextScheduleRequest] = []

    def build_context(self, request: ContextScheduleRequest) -> ContextEnvelope:
        self.requests.append(request)
        return _envelope()


def test_scheduler_maps_the_rust_envelope_to_the_application_result() -> None:
    engine = StubEngine()
    request = _request()

    result = ContextScheduler(engine=engine).build_context(request)

    assert engine.requests == [request]
    assert result.status == "ok"
    assert result.file_context == "synthetic context"
    assert result.scan_run_id == 7
    assert result.context_run_id == 8
    assert result.summary.success_count == 1


@pytest.mark.parametrize(
    ("schedule_request", "message"),
    [
        (
            ContextScheduleRequest(
                report_mode="daily",
                source="db",
                start_date=date(2026, 7, 16),
                end_date=date(2026, 7, 16),
            ),
            "unsupported context source",
        ),
        (
            ContextScheduleRequest(
                report_mode="quarterly",
                source="scan",
                start_date=date(2026, 7, 16),
                end_date=date(2026, 7, 16),
            ),
            "unsupported report_mode",
        ),
        (
            ContextScheduleRequest(
                report_mode="daily",
                source="scan",
                start_date=date(2026, 7, 17),
                end_date=date(2026, 7, 16),
            ),
            "start_date must be earlier",
        ),
    ],
)
def test_scheduler_rejects_invalid_application_requests(
    schedule_request: ContextScheduleRequest,
    message: str,
) -> None:
    with pytest.raises(ValueError, match=message):
        ContextScheduler(engine=StubEngine()).build_context(schedule_request)


def test_scheduler_builds_only_the_rust_context_client(monkeypatch) -> None:
    captured: dict[str, object] = {}

    class StubRustContextClient(StubEngine):
        def __init__(self, **kwargs: object) -> None:
            super().__init__()
            captured.update(kwargs)

    monkeypatch.setattr(
        scheduler_module,
        "RustContextClient",
        StubRustContextClient,
    )
    runtime_config = SimpleNamespace(
        scanner_engine="rust_v2",
        rust_scanner_bin="bin/scanner",
        rust_office_parser_bin="bin/office-worker",
        rust_index_db_path="state/scan_index_v2.sqlite3",
        rust_process_timeout_seconds=45.0,
    )

    result = ContextScheduler(runtime_config=runtime_config).build_context(
        _request()
    )

    assert result.status == "ok"
    assert captured == {
        "config": runtime_config,
        "scanner_binary": "bin/scanner",
        "scan_db_path": "state/scan_index_v2.sqlite3",
        "office_worker_path": "bin/office-worker",
        "timeout_seconds": 45.0,
    }


def test_scheduler_rejects_any_retired_engine_mode() -> None:
    runtime_config = SimpleNamespace(scanner_engine="retired")

    with pytest.raises(ValueError, match="unsupported scanner engine"):
        ContextScheduler(runtime_config=runtime_config)
