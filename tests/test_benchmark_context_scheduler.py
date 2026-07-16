"""测试只依赖应用 ContextBuildResult 的 scheduler benchmark。"""

from datetime import date
from pathlib import Path
from types import SimpleNamespace

import scripts.benchmark_context_scheduler as benchmark_module
from scripts.benchmark_context_scheduler import (
    build_benchmark_payload,
    build_context_scheduler_summary,
    render_markdown_report,
    run_benchmark,
    write_report_files,
)
from src.models.scanner_contract import ContextSummary, Diagnostic
from src.services.context_scheduler import ContextBuildResult


def _result(status: str = "ok") -> ContextBuildResult:
    warning = Diagnostic(
        error_code="PARSER_FAILED",
        message="synthetic warning",
        retryable=False,
        stage="parse",
        file_path=None,
        backend=None,
    )
    return ContextBuildResult(
        file_context="" if status == "error" else "synthetic context",
        status=status,
        summary=ContextSummary(
            source_file_count=3,
            success_count=2,
            timeout_count=0,
            included_file_count=2,
            omitted_file_count=1,
            error_file_count=1,
            input_chars=1000,
            output_chars=250,
            total_duration_ms=25,
            discovery_duration_ms=2,
            parse_duration_ms=18,
            compression_duration_ms=1,
        ),
        scan_run_id=3,
        context_run_id=None if status == "error" else 7,
        warnings=[warning] if status == "partial" else [],
        error=warning if status == "error" else None,
    )


def _args() -> SimpleNamespace:
    return SimpleNamespace(
        start_date=date(2026, 5, 9),
        end_date=date(2026, 5, 24),
        report_mode="weekly",
        compression_profile=None,
    )


def test_summary_uses_application_result_without_internal_decisions() -> None:
    summary = build_context_scheduler_summary(_result("partial"))

    assert summary["source_file_count"] == 3
    assert summary["compression_ratio"] == 0.25
    assert summary["status"] == "partial"
    assert summary["warning_codes"] == ["PARSER_FAILED"]
    assert "action_counts" not in summary
    assert "parser_backend_counts" not in summary


def test_payload_contains_run_ids_and_never_serializes_file_context() -> None:
    payload = build_benchmark_payload(
        context_result=_result(),
        parameters={"report_mode": "weekly"},
    )

    assert payload["scan_run"] == {"run_id": 3}
    assert payload["context_run"] == {"context_run_id": 7, "status": "ok"}
    assert payload["context_scheduler_summary"]["source_file_count"] == 3
    assert "file_context" not in str(payload)


def test_render_and_write_report_use_stable_application_summary(tmp_path: Path) -> None:
    payload = build_benchmark_payload(
        context_result=_result(),
        parameters={
            "start_date": "2026-05-09",
            "end_date": "2026-05-24",
            "report_mode": "weekly",
            "compression_profile": "weekly_balanced_v1",
        },
    )
    markdown = render_markdown_report(payload)
    json_out = tmp_path / "context_scheduler.json"
    markdown_out = tmp_path / "context_scheduler.md"

    write_report_files(
        payload,
        json_out=json_out,
        markdown_out=markdown_out,
    )

    assert "# Context Scheduler Benchmark Report" in markdown
    assert "compression_ratio" in markdown
    assert '"context_scheduler_summary"' in json_out.read_text(encoding="utf-8")
    assert "# Context Scheduler Benchmark Report" in markdown_out.read_text(
        encoding="utf-8"
    )


def test_run_benchmark_uses_scheduler_result_without_file_scanner(monkeypatch) -> None:
    class StubScheduler:
        def build_context(self, request) -> ContextBuildResult:
            assert request.report_mode == "weekly"
            return _result()

    monkeypatch.setattr(benchmark_module, "ContextScheduler", StubScheduler)

    payload = run_benchmark(_args())

    assert payload["scan_run"]["run_id"] == 3
    assert payload["context_run"]["context_run_id"] == 7


def test_error_result_remains_explicit_in_benchmark_payload() -> None:
    payload = build_benchmark_payload(
        context_result=_result("error"),
        parameters={"report_mode": "daily"},
    )

    assert payload["context_run"]["status"] == "error"
    assert payload["context_scheduler_summary"]["error_code"] == "PARSER_FAILED"
