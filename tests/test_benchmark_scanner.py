"""测试 Rust scanner evidence 驱动的 scanner benchmark。"""

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
    run_benchmark_suite,
    write_report_files,
)
from src.models.scanner_contract import (
    ContextDecision,
    ContextEnvelope,
    ContextSummary,
    Diagnostic,
    ExecutionMetricsV2,
    ExtensionMetric,
    FileAuditV2,
    ScannerEvidence,
    StageMetric,
)
from src.services.native_scanner import ScanResult


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


def _files() -> list[FileAuditV2]:
    return [
        FileAuditV2(
            relative_path="notes\\a.md",
            file_identity="fixture:a",
            source_version="mtime_ns=1:size=10",
            source_guard_kind="content_sha256_v1",
            source_guard_sha256="c" * 64,
            parse_status="success",
            parser_backend="light_text_v2",
            worker_lane="rust_core",
            parse_cache_status="fresh",
            cache_miss_reason="",
            truncated=False,
            content_sha256="a" * 64,
            parse_duration_ms=0,
            failure_class="",
            fallback_backend="",
            fallback_reason_code="",
            parse_transport="not_applicable",
            parse_attempt_count=0,
            final_diagnostic=None,
            pdf_classification=None,
        ),
        FileAuditV2(
            relative_path="docs\\broken.pdf",
            file_identity="fixture:b",
            source_version="mtime_ns=2:size=20",
            source_guard_kind="content_sha256_v1",
            source_guard_sha256="d" * 64,
            parse_status="error",
            parser_backend="python_pdf_text_v2",
            worker_lane="python_document_process_v2",
            parse_cache_status="miss",
            cache_miss_reason="new_file",
            truncated=True,
            content_sha256="b" * 64,
            parse_duration_ms=18,
            failure_class="recoverable_parser_failure",
            fallback_backend="",
            fallback_reason_code="",
            parse_transport="session",
            parse_attempt_count=1,
            final_diagnostic=_diagnostic(),
            pdf_classification=None,
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


def _evidence() -> ScannerEvidence:
    return ScannerEvidence(
        contract="ai_daily_context",
        protocol_version=1,
        request_id="21111111-2111-4111-8111-211111111111",
        scan_run_id=7,
        context_run_id=7,
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
        artifact_id=1,
        reused_from_context_run_id=None,
        reuse_kind="none",
        execution_metrics=ExecutionMetricsV2(
            discovery_observed_file_count=2,
            source_guard_content_hash_file_count=2,
            source_guard_unavailable_count=0,
            source_guard_bytes_read=30,
            candidate_file_count=2,
            admitted_file_count=2,
            classification_slot_count=0,
            confirmed_run_inspected_pages_total=0,
            unobserved_classification_attempt_count=0,
            nominal_charged_pages_total=0,
            extraction_slot_count=0,
            pdfplumber_invocations=1,
            snapshot_hit=False,
            parse_cache_lookup_count=2,
            classification_cache_lookup_count=0,
            parse_cache_all_hit=False,
            classification_cache_all_hit=None,
            stage_deadline_exhausted_count=0,
            session_restart_count=0,
            session_fallback_count=0,
            classify_attempt_count=0,
            parse_attempt_count=1,
            reserved_chars=100,
            rendered_chars=100,
            worker_handshake_ms=1,
            discovery_ms=2,
            snapshot_lookup_ms=1,
            current_run_audit_write_ms=1,
            terminal_precommit_ms=1,
            deadline_precommit_elapsed_ms=24,
            envelope_rebuild_ms=1,
            terminal_rows_written=8,
            peak_worker_rss_bytes=1024,
        ),
    )


def test_calculate_files_per_second_handles_normal_and_zero_duration():
    assert calculate_files_per_second(file_count=2, duration_ms=25) == 80.0
    assert calculate_files_per_second(file_count=0, duration_ms=25) == 0.0
    assert calculate_files_per_second(file_count=2, duration_ms=0) == 0.0


def test_backend_summary_keeps_parser_and_worker_lane_dimensions_separate():
    summary = build_parser_backend_summary(_files())

    assert summary["backend_counts"] == {
        "light_text_v2": 1,
        "python_pdf_text_v2": 1,
    }
    assert summary["worker_lane_counts"] == {
        "python_document_process_v2": 1,
        "rust_core": 1,
    }
    assert summary["by_extension"][".pdf"]["backends"] == {
        "python_pdf_text_v2": 1
    }
    assert summary["by_extension"][".pdf"]["lanes"] == {
        "python_document_process_v2": 1
    }


def test_payload_uses_scanner_evidence_and_never_contains_context_content():
    payload = build_benchmark_payload(
        envelope=_envelope(),
        evidence=_evidence(),
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


def test_payload_rejects_build_and_evidence_identity_mismatch():
    evidence = _evidence().model_copy(update={"scan_run_id": 8})

    with pytest.raises(ValueError, match="scanner evidence disagree"):
        build_benchmark_payload(
            envelope=_envelope(),
            evidence=evidence,
            start_date=date(2026, 7, 15),
            end_date=date(2026, 7, 16),
            summary_mode=False,
        )


def test_render_and_write_report_preserve_metadata_only(tmp_path: Path):
    payload = build_benchmark_payload(
        envelope=_envelope(),
        evidence=_evidence(),
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


def test_run_benchmark_uses_single_native_result(monkeypatch, tmp_path: Path):
    calls: list[tuple[str, object]] = []
    work_dir = tmp_path / "work"
    state_dir = tmp_path / "state"
    worker = tmp_path / "ai-daily-office-parser.exe"
    work_dir.mkdir()
    state_dir.mkdir()
    worker.touch()

    class StubScanner:
        def __init__(self, runtime_config, **kwargs) -> None:
            calls.append(("init", kwargs))

        def build_context(self, request) -> ScanResult:
            calls.append(("build", request.report_mode))
            return ScanResult(envelope=_envelope(), evidence=_evidence())

    monkeypatch.setattr(benchmark_module, "NativeScanner", StubScanner)
    args = SimpleNamespace(
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
        summary_mode=False,
        work_dir=work_dir,
        state_dir=state_dir,
        office_worker_path=worker,
    )

    payload = run_benchmark(args)

    assert payload["status"] == "partial"
    assert calls == [
        (
            "init",
            {"index_db_path": state_dir / "scan_index_v3.sqlite3"},
        ),
        ("build", "daily"),
    ]


def test_benchmark_suite_pairs_cold_and_warm_on_fresh_databases(
    monkeypatch,
    tmp_path: Path,
):
    work_dir = tmp_path / "work"
    state_dir = tmp_path / "state"
    worker = tmp_path / "ai-daily-office-parser.exe"
    work_dir.mkdir()
    worker.touch()
    scanner_databases: dict[int, Path] = {}
    benchmark_calls: list[tuple[int, Path]] = []

    class StubScanner:
        def __init__(self, runtime_config, *, index_db_path: Path) -> None:
            assert index_db_path.name == "scan_index_v3.sqlite3"
            assert index_db_path.parent.is_dir()
            scanner_databases[id(self)] = index_db_path

    def stub_run_benchmark(args, *, scanner, scan_db_path):
        scanner_id = id(scanner)
        benchmark_calls.append((scanner_id, scan_db_path))
        is_warm = sum(call[0] == scanner_id for call in benchmark_calls) == 2
        duration_ms = 10 if is_warm else 100
        return {
            "status": "ok",
            "scan_result": {
                "total_files": 2,
                "error_count": 0,
                "timeout_count": 0,
            },
            "metrics": {
                "total_duration_ms": duration_ms,
                "files_per_second": 200.0 if is_warm else 20.0,
                "peak_worker_rss_bytes": 1024,
                "reparsed_count": 0 if is_warm else 2,
                "reused_count": 2 if is_warm else 0,
            },
        }

    monkeypatch.setattr(benchmark_module, "NativeScanner", StubScanner)
    monkeypatch.setattr(benchmark_module, "run_benchmark", stub_run_benchmark)
    args = SimpleNamespace(
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
        summary_mode=False,
        work_dir=work_dir,
        state_dir=state_dir,
        office_worker_path=worker,
        iterations=2,
        baseline_cold_ms=None,
        baseline_warm_ms=None,
    )

    payload = run_benchmark_suite(args)

    first_scanner, second_scanner = scanner_databases
    assert first_scanner != second_scanner
    assert benchmark_calls == [
        (first_scanner, state_dir / "pair_1" / "scan_index_v3.sqlite3"),
        (first_scanner, state_dir / "pair_1" / "scan_index_v3.sqlite3"),
        (second_scanner, state_dir / "pair_2" / "scan_index_v3.sqlite3"),
        (second_scanner, state_dir / "pair_2" / "scan_index_v3.sqlite3"),
    ]
    assert payload["cold"]["median_ms"] == 100.0
    assert payload["warm"]["median_ms"] == 10.0
    assert payload["gate"] == {
        "passed": True,
        "complete": True,
        "warm_full_reuse": True,
        "scanner_process_start_count": 0,
        "scanner_transport_serialized_bytes": 0,
        "baseline": {"evaluated": False},
    }
