from __future__ import annotations

import importlib
from datetime import date
from types import SimpleNamespace

import pytest

from src.services.native_scanner import (
    NativeScanner,
    NativeScannerError,
    ScanRequest,
)


class StubNative:
    def __init__(self, result: object) -> None:
        self.result = result
        self.requests: list[dict[str, object]] = []

    def build_context(self, request: dict[str, object]) -> object:
        self.requests.append(request)
        return self.result

    def doctor(self) -> object:
        return {
            "contract": "ai_daily_context",
            "protocol_version": 1,
            "request_id": "11111111-1111-4111-8111-111111111111",
            "status": "ok",
            "engine_version": "0.1.0",
            "engine_build": "test-build",
            "checks": [],
            "warnings": [],
            "error": None,
        }


def _runtime_config() -> SimpleNamespace:
    return SimpleNamespace(
        work_dir="D:/audit/work",
        rust_index_db_path="data/db/scan_index_v2.sqlite3",
        rust_office_parser_bin="rust/target/release/ai-daily-office-parser",
        scanner_contract_profile=lambda: {"schema_version": "scanner_profile_v2"},
    )


def _envelope(scan_run_id: int | None = None) -> dict[str, object]:
    return {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "11111111-1111-4111-8111-111111111111",
        "engine_version": "0.1.0",
        "engine_build": "test-build",
        "status": "error",
        "file_context": "",
        "summary": {
            "source_file_count": 0,
            "success_count": 0,
            "timeout_count": 0,
            "included_file_count": 0,
            "omitted_file_count": 0,
            "error_file_count": 0,
            "input_chars": 0,
            "output_chars": 0,
            "total_duration_ms": 0,
            "discovery_duration_ms": 0,
            "parse_duration_ms": 0,
            "compression_duration_ms": 0,
        },
        "scan_run_id": scan_run_id,
        "context_run_id": None,
        "warnings": [],
        "error": {
            "error_code": "INVALID_REQUEST",
            "message": "synthetic",
            "retryable": False,
            "stage": "request",
            "file_path": None,
            "backend": None,
        },
    }


def test_native_module_is_lazy(monkeypatch) -> None:
    calls: list[str] = []

    def fake_import(name: str):
        calls.append(name)
        return SimpleNamespace(Scanner=lambda config: StubNative({"envelope": _envelope(), "evidence": None}))

    monkeypatch.setattr(importlib, "import_module", fake_import)
    assert calls == []

    NativeScanner(_runtime_config())

    assert calls == ["ai_daily_scanner_native"]


def test_build_context_maps_the_small_interface() -> None:
    native = StubNative({"envelope": _envelope(), "evidence": None})
    scanner = NativeScanner(_runtime_config(), native=native)

    result = scanner.build_context(
        ScanRequest(
            report_mode="daily",
            start_date=date(2026, 8, 12),
            end_date=date(2026, 8, 12),
        )
    )

    assert result.envelope.error is not None
    assert native.requests == [
        {
            "report_mode": "daily",
            "start_date": "2026-08-12",
            "end_date": "2026-08-12",
            "compression_profile": None,
        }
    ]


def test_result_with_run_requires_matching_evidence() -> None:
    native = StubNative({"envelope": _envelope(7), "evidence": None})
    scanner = NativeScanner(_runtime_config(), native=native)

    with pytest.raises(NativeScannerError, match="NATIVE_EVIDENCE_INVALID"):
        scanner.build_context(
            ScanRequest(
                report_mode="daily",
                start_date=date(2026, 8, 12),
                end_date=date(2026, 8, 12),
            )
        )


def test_structured_native_error_is_stable() -> None:
    class FailingNative(StubNative):
        def build_context(self, request: dict[str, object]) -> object:
            raise RuntimeError("SCANNER_BUSY", "busy", True)

    scanner = NativeScanner(_runtime_config(), native=FailingNative(None))

    with pytest.raises(NativeScannerError) as captured:
        scanner.build_context(
            ScanRequest(
                report_mode="daily",
                start_date=date(2026, 8, 12),
                end_date=date(2026, 8, 12),
            )
        )

    assert captured.value.error_code == "SCANNER_BUSY"
    assert captured.value.retryable is True


def test_doctor_uses_native_typed_result() -> None:
    result = NativeScanner(_runtime_config(), native=StubNative(None)).doctor()

    assert result.status == "ok"
