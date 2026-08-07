"""测试 Rust inspect-run DTO 驱动的 scanner benchmark。"""

from datetime import date
from pathlib import Path
from types import SimpleNamespace

import pytest

import scripts.benchmark_scanner as benchmark_module
from scripts.benchmark_scanner import (
    build_benchmark_payload,
    build_parser_backend_summary,
    calculate_files_per_second,
    render_markdown_report,
    run_benchmark,
    write_report_files,
)
from src.models.scanner_contract import (
    ContextDecision,
    ContextEnvelope,
    ContextSummary,
    Diagnostic,
    ExtensionMetric,
    FileAudit,
    InspectRunResponse,
    StageMetric,
)


def _diagnostic() -> Diagnostic:
    return Diagnostic(
        error_code="PARSER_FAILED",
        message="synthetic parser degradation",
        retryable=False,
        stage="parse",
        file_path=None,
        backend=None,
    )


def _summary() -> ContextSummary:
    return ContextSummary(
        source_file_count=2,
        success_count=1,
        timeout_count=0,
        included_file_count=1,
        omitted_file_count=0,
        error_file_count=1,
        input_chars=10,
        output_chars=100,
        total_duration_ms=25,
        discovery_duration_ms=2,
        parse_duration_ms=18,
        compression_duration_ms=1,
    )


def _files() -> list[FileAudit]:
    return [
        FileAudit(
            relative_path="notes\\a.md",
            file_identity="fixture:a",
            source_version="mtime_ns=1:size=10",
            parse_status="success",
            parser_backend="light_text_v1",
            worker_lane="rust_core",
            cache_status="fresh",
            cache_miss_reason="",
            truncated=False,
            content_sha256="a" * 64,
            parse_duration_ms=0,
            failure_class="",
            fallback_backend="",
            fallback_reason_code="",
        ),
        FileAudit(
            relative_path="docs\\broken.pdf",
            file_identity="fixture:b",
            source_version="mtime_ns=2:size=20",
            parse_status="error",
            parser_backend="pdf_text_v1",
            worker_lane="python_document_process",
            cache_status="miss",
            cache_miss_reason="new_file",
            truncated=True,
            content_sha256="b" * 64,
            parse_duration_ms=18,
            failure_class="recoverable_parser_failure",
            fallback_backend="",
            fallback_reason_code="",
        ),
    ]


def _envelope() -> ContextEnvelope:
    return ContextEnvelope(
        contract="ai_daily_context",
        protocol_version=1,
        request_id="11111111-1111-4111-8111-111111111111",
        engine_version="0.1.0",
        engine_build="synthetic-build",
        status="partial",
        file_context="private synthetic context",
        summary=_summary(),
        scan_run_id=7,
        context_run_id=7,
        warnings=[_diagnostic()],
        error=None,
    )


def _inspection() -> InspectRunResponse:
    return InspectRunResponse(
        contract="ai_daily_context",
        protocol_version=1,
        request_id="21111111-2111-4111-8111-211111111111",
        scan_run_id=7,
        context_run_id=7,
        status="ok",
        run_status="partial",
        summary=_summary(),
        stage_metrics=[
            StageMetric(stage="discovery", item_count=2, duration_ms=2),
            StageMetric(stage="cache", item_count=2, duration_ms=1),
            StageMetric(stage="parse", item_count=1, duration_ms=18),
            StageMetric(stage="context", item_count=2, duration_ms=1),
        ],
        extension_metrics=[
            ExtensionMetric(
                extension=".md",
                file_count=1,
                parse_duration_ms=0,
                success_count=1,
                error_count=0,
                timeout_count=0,
            ),
            ExtensionMetric(
                extension=".pdf",
                file_count=1,
                parse_duration_ms=18,
                success_count=0,
                error_count=1,
                timeout_count=0,
            ),
        ],
        files=_files(),
        decisions=[
            ContextDecision(
                relative_path="notes\\a.md",
                action="keep",
                reason="small_file_keep",
                priority=30,
                input_chars=10,
                output_chars=10,
                truncated=False,
                error_code="",
            ),
            ContextDecision(
                relative_path="docs\\broken.pdf",
                action="error",
                reason="parse_error",
                priority=80,
                input_chars=0,
                output_chars=0,
                truncated=True,
                error_code="PARSER_FAILED",
            ),
        ],
        warnings=[_diagnostic()],
        error=None,
    )


def test_calculate_files_per_second_handles_normal_and_zero_duration():
    assert calculate_files_per_second(file_count=2, duration_ms=25) == 80.0
    assert calculate_files_per_second(file_count=0, duration_ms=25) == 0.0
    assert calculate_files_per_second(file_count=2, duration_ms=0) == 0.0


def test_backend_summary_keeps_parser_and_worker_lane_dimensions_separate():
    summary = build_parser_backend_summary(_files())

    assert summary["backend_counts"] == {
        "light_text_v1": 1,
        "pdf_text_v1": 1,
    }
    assert summary["worker_lane_counts"] == {
        "python_document_process": 1,
        "rust_core": 1,
    }
    assert summary["by_extension"][".pdf"]["backends"] == {
        "pdf_text_v1": 1
    }
    assert summary["by_extension"][".pdf"]["lanes"] == {
        "python_document_process": 1
    }


def test_payload_uses_inspect_dto_and_never_contains_context_content():
    payload = build_benchmark_payload(
        envelope=_envelope(),
        inspection=_inspection(),
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
        summary_mode=False,
    )

    assert payload["metrics"]["run_id"] == 7
    assert payload["metrics"]["reused_count"] == 1
    assert payload["metrics"]["reparsed_count"] == 1
    assert payload["metrics"]["cache_duration_ms"] == 1
    assert payload["metrics"]["files_per_second"] == 80.0
    assert payload["files"][0]["relative_path"] == "notes\\a.md"
    assert "private synthetic context" not in str(payload)
    assert "file_context" not in str(payload)


def test_payload_rejects_build_and_inspect_identity_mismatch():
    inspection = _inspection().model_copy(update={"scan_run_id": 8})

    with pytest.raises(ValueError, match="DTOs disagree"):
        build_benchmark_payload(
            envelope=_envelope(),
            inspection=inspection,
            start_date=date(2026, 7, 15),
            end_date=date(2026, 7, 16),
            summary_mode=False,
        )


def test_render_and_write_report_preserve_metadata_only(tmp_path: Path):
    payload = build_benchmark_payload(
        envelope=_envelope(),
        inspection=_inspection(),
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
        summary_mode=True,
    )
    markdown = render_markdown_report(payload)
    json_out = tmp_path / "scanner.json"
    markdown_out = tmp_path / "scanner.md"

    write_report_files(payload, json_out, markdown_out)

    assert "# Rust Scanner Benchmark Report" in markdown
    assert "Parser Backend And Worker Lane" in markdown
    assert "files_per_second: `80.0`" in markdown
    assert '"worker_lane": "rust_core"' in json_out.read_text(encoding="utf-8")
    assert "# Rust Scanner Benchmark Report" in markdown_out.read_text(
        encoding="utf-8"
    )


def test_run_benchmark_calls_build_then_inspect_without_file_scanner(monkeypatch):
    calls: list[tuple[str, object]] = []

    class StubClient:
        def __init__(self, **kwargs) -> None:
            calls.append(("init", kwargs))

        def build_context(self, request) -> ContextEnvelope:
            calls.append(("build", request.report_mode))
            return _envelope()

        def inspect_run(self, scan_run_id: int, *, include_content: bool):
            calls.append(("inspect", (scan_run_id, include_content)))
            return _inspection()

    monkeypatch.setattr(benchmark_module, "RustContextClient", StubClient)
    monkeypatch.setattr(
        benchmark_module,
        "config",
        SimpleNamespace(
            rust_scanner_bin="scanner.exe",
            rust_office_parser_bin="office-worker.exe",
            rust_index_db_path="state/scan_index_v2.sqlite3",
            rust_process_timeout_seconds=90,
        ),
    )
    args = SimpleNamespace(
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
        summary_mode=False,
        scanner_bin=None,
        scan_db_path=None,
    )

    payload = run_benchmark(args)

    assert payload["status"] == "partial"
    assert calls[0][1]["office_worker_path"] == "office-worker.exe"
    assert calls[1:] == [("build", "daily"), ("inspect", (7, False))]
