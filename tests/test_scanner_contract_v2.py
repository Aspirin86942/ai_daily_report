"""Scanner evidence、settings 与 worker 合同测试。"""

from __future__ import annotations

import pytest

from src.models.scanner_contract import (
    BuildContextRequest,
    Diagnostic,
    FileAuditV2,
    InspectRunResponseV2,
    PdfClassificationAuditV1,
    ScannerSettings,
    TransportErrorResponse,
    WORKER_DIAGNOSTIC_V1_ERROR_CODES,
    WORKER_DIAGNOSTIC_V1_STAGES,
    WorkerDiagnosticV1,
    WorkerParseResponse,
)


def test_worker_diagnostic_v1_error_code_set_is_frozen() -> None:
    """scanner-side 新 ErrorCode/DiagnosticStage 绝不进入冻结 worker v1 集合。"""
    assert len(WORKER_DIAGNOSTIC_V1_ERROR_CODES) == 25
    assert "STAGE_DEADLINE_EXHAUSTED" not in WORKER_DIAGNOSTIC_V1_ERROR_CODES
    assert "BUDGET_MODEL_MISMATCH" not in WORKER_DIAGNOSTIC_V1_ERROR_CODES
    assert "INSPECT_V2_PROVENANCE_UNAVAILABLE" not in WORKER_DIAGNOSTIC_V1_ERROR_CODES
    assert len(WORKER_DIAGNOSTIC_V1_STAGES) == 9
    assert "maintenance" not in WORKER_DIAGNOSTIC_V1_STAGES


def test_worker_diagnostic_v1_rejects_scanner_side_codes() -> None:
    with pytest.raises(ValueError, match="STAGE_DEADLINE_EXHAUSTED"):
        WorkerDiagnosticV1.model_validate(
            {
                "error_code": "STAGE_DEADLINE_EXHAUSTED",
                "message": "scanner-only code must not enter worker v1",
                "retryable": True,
                "stage": "parse",
                "file_path": None,
                "backend": None,
            }
        )
    with pytest.raises(ValueError, match="maintenance"):
        WorkerDiagnosticV1.model_validate(
            {
                "error_code": "PARSER_FAILED",
                "message": "scanner-only stage must not enter worker v1",
                "retryable": False,
                "stage": "maintenance",
                "file_path": None,
                "backend": None,
            }
        )


def _worker_parse_response_payload(error_code: str, *, stage: str) -> dict:
    return {
        "contract": "ai_daily_worker",
        "protocol_version": 1,
        "request_id": "34444444-3444-4444-8444-344444444444",
        "status": "error",
        "file_path": "C:\\scanner-fixtures\\工作 目录\\legacy-export.doc",
        "file_type": ".doc",
        "content": "",
        "parser_backend": "python_sharepoint_text_v2",
        "worker_lane": "python_document_process_v2",
        "truncated": False,
        "warnings": [],
        "error": {
            "error_code": error_code,
            "message": "synthetic worker failure",
            "retryable": False,
            "stage": stage,
            "file_path": "C:\\scanner-fixtures\\工作 目录\\legacy-export.doc",
            "backend": "python_sharepoint_text_v2",
        },
        "duration_ms": 3,
        "worker_contract_version": "ai_daily_worker_v1",
        "worker_version": "0.1.0",
        "worker_build": "fixture-python-worker-build-v1",
        "observed_source_version": "mtime_ns=4000000001:size=1024",
    }


def test_worker_parse_response_rejects_scanner_side_error_code() -> None:
    """WorkerParseResponse 必须使用 frozen WorkerDiagnosticV1：scanner-side code 拒绝。"""
    with pytest.raises(ValueError, match="STAGE_DEADLINE_EXHAUSTED"):
        WorkerParseResponse.model_validate(
            _worker_parse_response_payload("STAGE_DEADLINE_EXHAUSTED", stage="parse")
        )


def test_worker_parse_response_round_trips_frozen_error_code() -> None:
    """Frozen legit code 在 worker v1 wire 上往返并保留原值。"""
    response = WorkerParseResponse.model_validate(
        _worker_parse_response_payload("PARSER_TIMEOUT", stage="parse")
    )
    assert response.status == "error"
    assert response.error is not None
    assert response.error.error_code == "PARSER_TIMEOUT"
    assert response.error.stage == "parse"
    assert response.error.retryable is False
    assert response.error.file_path == "C:\\scanner-fixtures\\工作 目录\\legacy-export.doc"
    assert response.error.backend == "python_sharepoint_text_v2"


def test_transport_error_rejects_scanner_side_code() -> None:
    """ai_daily_transport wire 也使用 frozen WorkerDiagnosticV1：scanner-side code 拒绝。"""
    with pytest.raises(ValueError, match="STAGE_DEADLINE_EXHAUSTED"):
        TransportErrorResponse.model_validate(
            {
                "contract": "ai_daily_transport",
                "protocol_version": 1,
                "status": "error",
                "error": {
                    "error_code": "STAGE_DEADLINE_EXHAUSTED",
                    "message": "scanner-only code must not enter the transport wire",
                    "retryable": True,
                    "stage": "parse",
                    "file_path": None,
                    "backend": None,
                },
            }
        )


def test_transport_error_round_trips_frozen_invalid_request() -> None:
    """Frozen INVALID_REQUEST 在 ai_daily_transport wire 上往返并保留原值。"""
    response = TransportErrorResponse.model_validate(
        {
            "contract": "ai_daily_transport",
            "protocol_version": 1,
            "status": "error",
            "error": {
                "error_code": "INVALID_REQUEST",
                "message": "stdin is not a valid worker request",
                "retryable": False,
                "stage": "request",
                "file_path": None,
                "backend": None,
            },
        }
    )
    assert response.error.error_code == "INVALID_REQUEST"
    assert response.error.stage == "request"
    assert response.error.file_path is None
    assert response.error.backend is None


def test_scanner_diagnostic_accepts_the_new_codes_and_stage() -> None:
    Diagnostic(
        error_code="STAGE_DEADLINE_EXHAUSTED",
        message="work deadline exhausted",
        retryable=True,
        stage="parse",
        file_path=None,
        backend=None,
    )
    Diagnostic(
        error_code="MAINTENANCE_MODE_UNAVAILABLE",
        message="incremental vacuum unavailable",
        retryable=False,
        stage="maintenance",
        file_path=None,
        backend=None,
    )


def _build_context_request_payload(scanner_settings: dict) -> dict:
    return {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "123e4567-e89b-42d3-a456-426614174000",
        "work_dir": "C:\\scanner-fixtures\\work",
        "start_date": "2026-07-14",
        "end_date": "2026-07-15",
        "report_mode": "monthly",
        "compression_profile": None,
        "scan_db_path": "C:\\scanner-fixtures\\state\\scan.sqlite3",
        "scanner_settings": scanner_settings,
        "adapters": {
            "office_worker_path": "C:\\scanner-fixtures\\bin\\ai-daily-office-parser.exe",
            "python_executable": "C:\\scanner-fixtures\\venv\\Scripts\\python.exe",
            "python_module_root": "C:\\scanner-fixtures\\repo",
            "python_document_worker_module": "src.workers.document_parser_worker",
        },
    }


def test_build_context_request_accepts_single_settings_shape() -> None:
    request = BuildContextRequest.model_validate(
        _build_context_request_payload(
            {
                "max_workers": 3,
                "total_deadline_ms": 45_000,
                "worker_max_requests": 64,
            }
        )
    )
    assert request.scanner_settings.max_workers == 3
    assert request.scanner_settings.total_deadline_ms == 45_000
    assert request.scanner_settings.worker_max_requests == 64


def test_scanner_settings_reject_removed_profile_and_session_keys() -> None:
    for key in (
        "schema_version",
        "parser_profile_version",
        "office_parser_backend",
        "session_concurrency",
        "max_requests_per_session",
    ):
        with pytest.raises(ValueError, match=key):
            ScannerSettings.model_validate({key: 1})


def test_build_context_request_rejects_unknown_settings_key() -> None:
    with pytest.raises(ValueError, match="unknown_setting"):
        BuildContextRequest.model_validate(
            _build_context_request_payload({"unknown_setting": 1})
        )


def test_inspect_run_response_v2_error_sentinel_is_strict() -> None:
    payload = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "response_version": 2,
        "request_id": "123e4567-e89b-42d3-a456-426614174000",
        "scan_run_id": 1,
        "context_run_id": None,
        "status": "error",
        "run_status": None,
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
        "stage_metrics": [],
        "extension_metrics": [],
        "files": [],
        "decisions": [],
        "warnings": [],
        "error": {
            "error_code": "INSPECT_V2_PROVENANCE_UNAVAILABLE",
            "message": "migrated v1 run lacks v2 provenance",
            "retryable": False,
            "stage": "inspect",
            "file_path": None,
            "backend": None,
        },
        "artifact_id": None,
        "reused_from_context_run_id": None,
        "reuse_kind": "none",
        "execution_metrics": {
            "discovery_observed_file_count": 0,
            "source_guard_content_hash_file_count": 0,
            "source_guard_unavailable_count": 0,
            "source_guard_bytes_read": 0,
            "candidate_file_count": 0,
            "admitted_file_count": 0,
            "classification_slot_count": 0,
            "confirmed_run_inspected_pages_total": 0,
            "unobserved_classification_attempt_count": 0,
            "nominal_charged_pages_total": 0,
            "extraction_slot_count": 0,
            "pdfplumber_invocations": 0,
            "snapshot_hit": False,
            "parse_cache_lookup_count": 0,
            "classification_cache_lookup_count": 0,
            "parse_cache_all_hit": None,
            "classification_cache_all_hit": None,
            "stage_deadline_exhausted_count": 0,
            "session_restart_count": 0,
            "session_fallback_count": 0,
            "classify_attempt_count": 0,
            "parse_attempt_count": 0,
            "reserved_chars": 0,
            "rendered_chars": 0,
            "worker_handshake_ms": 0,
            "discovery_ms": 0,
            "snapshot_lookup_ms": 0,
            "current_run_audit_write_ms": 0,
            "terminal_precommit_ms": 0,
            "deadline_precommit_elapsed_ms": 0,
            "envelope_rebuild_ms": 0,
            "terminal_rows_written": 0,
            "peak_worker_rss_bytes": None,
        },
    }
    InspectRunResponseV2.model_validate(payload)


def _ok_inspect_v2_payload() -> dict:
    return {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "response_version": 2,
        "request_id": "123e4567-e89b-42d3-a456-426614174000",
        "scan_run_id": 1,
        "context_run_id": 1,
        "status": "ok",
        "run_status": "success",
        "summary": {
            "source_file_count": 1,
            "success_count": 1,
            "timeout_count": 0,
            "included_file_count": 1,
            "omitted_file_count": 0,
            "error_file_count": 0,
            "input_chars": 1,
            "output_chars": 1,
            "total_duration_ms": 1,
            "discovery_duration_ms": 0,
            "parse_duration_ms": 0,
            "compression_duration_ms": 0,
        },
        "stage_metrics": [],
        "extension_metrics": [],
        "files": [],
        "decisions": [],
        "warnings": [],
        "error": None,
        "artifact_id": 1,
        "reused_from_context_run_id": None,
        "reuse_kind": "none",
        "execution_metrics": {
            "discovery_observed_file_count": 0,
            "source_guard_content_hash_file_count": 0,
            "source_guard_unavailable_count": 0,
            "source_guard_bytes_read": 0,
            "candidate_file_count": 0,
            "admitted_file_count": 0,
            "classification_slot_count": 0,
            "confirmed_run_inspected_pages_total": 0,
            "unobserved_classification_attempt_count": 0,
            "nominal_charged_pages_total": 0,
            "extraction_slot_count": 0,
            "pdfplumber_invocations": 0,
            "snapshot_hit": False,
            "parse_cache_lookup_count": 0,
            "classification_cache_lookup_count": 0,
            "parse_cache_all_hit": None,
            "classification_cache_all_hit": None,
            "stage_deadline_exhausted_count": 0,
            "session_restart_count": 0,
            "session_fallback_count": 0,
            "classify_attempt_count": 0,
            "parse_attempt_count": 0,
            "reserved_chars": 0,
            "rendered_chars": 0,
            "worker_handshake_ms": 0,
            "discovery_ms": 0,
            "snapshot_lookup_ms": 0,
            "current_run_audit_write_ms": 0,
            "terminal_precommit_ms": 0,
            "deadline_precommit_elapsed_ms": 0,
            "envelope_rebuild_ms": 0,
            "terminal_rows_written": 0,
            "peak_worker_rss_bytes": None,
        },
    }


def test_inspect_run_response_v2_ok_success_requires_artifact_id() -> None:
    InspectRunResponseV2.model_validate(_ok_inspect_v2_payload())
    payload = _ok_inspect_v2_payload()
    payload["artifact_id"] = None
    with pytest.raises(ValueError, match="artifact_id"):
        InspectRunResponseV2.model_validate(payload)


def test_inspect_run_response_v2_error_sentinel_rejects_nonzero_metrics() -> None:
    payload = _ok_inspect_v2_payload()
    payload["status"] = "error"
    payload["run_status"] = None
    payload["context_run_id"] = None
    payload["artifact_id"] = None
    payload["error"] = {
        "error_code": "INSPECT_V2_PROVENANCE_UNAVAILABLE",
        "message": "migrated v1 run lacks v2 provenance",
        "retryable": False,
        "stage": "inspect",
        "file_path": None,
        "backend": None,
    }
    InspectRunResponseV2.model_validate(payload)
    payload["execution_metrics"]["discovery_observed_file_count"] = 1
    with pytest.raises(ValueError, match="sentinel"):
        InspectRunResponseV2.model_validate(payload)


def test_inspect_run_response_v2_parse_cache_reuse_requires_all_hit() -> None:
    payload = _ok_inspect_v2_payload()
    payload["reuse_kind"] = "parse_cache"
    payload["execution_metrics"]["parse_cache_lookup_count"] = 1
    payload["execution_metrics"]["parse_cache_all_hit"] = True
    InspectRunResponseV2.model_validate(payload)
    payload["execution_metrics"]["parse_cache_all_hit"] = False
    with pytest.raises(ValueError, match="all-hit"):
        InspectRunResponseV2.model_validate(payload)


def _file_audit_v2_payload() -> dict:
    return {
        "relative_path": "evidence.pdf",
        "file_identity": "C:\\evidence\\evidence.pdf",
        "source_version": "mtime_ns=1:size=1",
        "source_guard_kind": "content_sha256_v1",
        "source_guard_sha256": "a" * 64,
        "parse_status": "error",
        "parser_backend": "python_pdf_text_v2",
        "worker_lane": "python_document_process_v2",
        "parse_cache_status": "miss",
        "cache_miss_reason": "new_file",
        "truncated": False,
        "content_sha256": "a" * 64,
        "parse_duration_ms": 1,
        "failure_class": "",
        "fallback_backend": "",
        "fallback_reason_code": "",
        "parse_transport": "one_shot",
        "parse_attempt_count": 1,
        "final_diagnostic": None,
        "pdf_classification": None,
    }


def test_file_audit_v2_final_diagnostic_nullability_is_enforced() -> None:
    payload = _file_audit_v2_payload()
    with pytest.raises(ValueError, match="final diagnostic"):
        FileAuditV2.model_validate(payload)

    payload["final_diagnostic"] = {
        "error_code": "PARSER_FAILED",
        "message": "synthetic failure",
        "retryable": False,
        "stage": "parse",
        "file_path": None,
        "backend": None,
    }
    FileAuditV2.model_validate(payload)

    payload["parse_status"] = "success"
    payload["final_diagnostic"] = None
    FileAuditV2.model_validate(payload)
    payload["final_diagnostic"] = {
        "error_code": "PARSER_FAILED",
        "message": "synthetic failure",
        "retryable": False,
        "stage": "parse",
        "file_path": None,
        "backend": None,
    }
    with pytest.raises(ValueError, match="final diagnostic"):
        FileAuditV2.model_validate(payload)


def _pdf_classification_audit_payload() -> dict:
    return {
        "status": "text_in_parse_window",
        "page_count": 2,
        "classification_cache_status": "miss",
        "classification_cache_miss_reason": "new_file",
        "result_examined_pages": 1,
        "run_inspected_pages": 1,
        "nominal_charged_pages": 2,
        "duration_ms": 1,
        "transport": "one_shot",
        "attempt_count": 1,
        "classifier_build": "a" * 64,
        "classifier_profile_hash": "b" * 64,
    }


def test_pdf_classification_audit_execution_matrix_is_enforced() -> None:
    PdfClassificationAuditV1.model_validate(_pdf_classification_audit_payload())

    fresh = _pdf_classification_audit_payload()
    fresh.update(
        classification_cache_status="fresh",
        classification_cache_miss_reason="",
        run_inspected_pages=0,
        duration_ms=0,
    )
    with pytest.raises(ValueError, match="zero execution"):
        PdfClassificationAuditV1.model_validate(fresh)

    not_eligible = _pdf_classification_audit_payload()
    not_eligible.update(
        status="not_classified_by_budget",
        page_count=None,
        classification_cache_status="not_eligible",
        classification_cache_miss_reason="",
        result_examined_pages=0,
        run_inspected_pages=0,
        nominal_charged_pages=0,
        duration_ms=0,
        transport="not_applicable",
        attempt_count=0,
    )
    PdfClassificationAuditV1.model_validate(not_eligible)
    not_eligible["classification_cache_status"] = "snapshot"
    not_eligible["transport"] = "snapshot"
    with pytest.raises(ValueError, match="not_eligible"):
        PdfClassificationAuditV1.model_validate(not_eligible)


def test_pdf_classification_audit_page_matrix_and_miss_allowlist_are_enforced() -> None:
    valid = _pdf_classification_audit_payload()
    PdfClassificationAuditV1.model_validate(valid)

    snapshot = dict(valid)
    snapshot.update(
        classification_cache_status="snapshot",
        classification_cache_miss_reason="",
        run_inspected_pages=0,
        duration_ms=0,
        transport="snapshot",
        attempt_count=0,
    )
    PdfClassificationAuditV1.model_validate(snapshot)

    zero_page_count = dict(valid)
    zero_page_count["page_count"] = 0
    with pytest.raises(ValueError, match="positive"):
        PdfClassificationAuditV1.model_validate(zero_page_count)

    text_beyond_window = dict(valid)
    text_beyond_window["result_examined_pages"] = 3
    with pytest.raises(ValueError, match="window"):
        PdfClassificationAuditV1.model_validate(text_beyond_window)

    no_text_short_window = dict(valid)
    no_text_short_window.update(
        status="no_text_in_parse_window",
        result_examined_pages=1,
    )
    with pytest.raises(ValueError, match="window"):
        PdfClassificationAuditV1.model_validate(no_text_short_window)

    excessive_run_pages = dict(valid)
    excessive_run_pages.update(attempt_count=2, run_inspected_pages=5)
    with pytest.raises(ValueError, match="attempt"):
        PdfClassificationAuditV1.model_validate(excessive_run_pages)

    unknown_beyond_nominal = dict(valid)
    unknown_beyond_nominal.update(
        status="unknown",
        page_count=None,
        result_examined_pages=3,
        run_inspected_pages=3,
    )
    with pytest.raises(ValueError, match="nominal"):
        PdfClassificationAuditV1.model_validate(unknown_beyond_nominal)

    legacy_miss_reason = dict(valid)
    legacy_miss_reason["classification_cache_miss_reason"] = "error_cache"
    with pytest.raises(ValueError, match="allowlist"):
        PdfClassificationAuditV1.model_validate(legacy_miss_reason)


def test_file_audit_v2_parse_execution_matrix_is_enforced() -> None:
    valid = _file_audit_v2_payload()
    valid["final_diagnostic"] = {
        "error_code": "PARSER_FAILED",
        "message": "synthetic failure",
        "retryable": False,
        "stage": "parse",
        "file_path": None,
        "backend": None,
    }
    FileAuditV2.model_validate(valid)

    miss_without_execution = dict(valid)
    miss_without_execution.update(
        parse_transport="not_applicable",
        parse_attempt_count=0,
        parse_duration_ms=0,
    )
    with pytest.raises(ValueError, match="started parser"):
        FileAuditV2.model_validate(miss_without_execution)

    fresh_with_execution = dict(valid)
    fresh_with_execution.update(
        parse_status="success",
        parse_cache_status="fresh",
        cache_miss_reason="",
        final_diagnostic=None,
    )
    with pytest.raises(ValueError, match="zero execution"):
        FileAuditV2.model_validate(fresh_with_execution)

    not_applicable_with_execution = dict(valid)
    not_applicable_with_execution.update(
        parse_cache_status="not_applicable",
        cache_miss_reason="",
    )
    with pytest.raises(ValueError, match="zero execution"):
        FileAuditV2.model_validate(not_applicable_with_execution)

    too_many_attempts = dict(valid)
    too_many_attempts["parse_attempt_count"] = 4
    with pytest.raises(ValueError):
        FileAuditV2.model_validate(too_many_attempts)


def test_file_audit_v2_backend_lane_and_miss_reason_matrix_is_enforced() -> None:
    valid_miss = _file_audit_v2_payload()
    valid_miss["final_diagnostic"] = {
        "error_code": "PARSER_FAILED",
        "message": "synthetic failure",
        "retryable": False,
        "stage": "parse",
        "file_path": None,
        "backend": None,
    }
    FileAuditV2.model_validate(valid_miss)

    legacy_miss_reason = dict(valid_miss)
    legacy_miss_reason["cache_miss_reason"] = "error_cache"
    with pytest.raises(ValueError, match="allowlist"):
        FileAuditV2.model_validate(legacy_miss_reason)

    miss_without_body_parser = dict(valid_miss)
    miss_without_body_parser.update(
        parser_backend="not_parsed",
        worker_lane="not_parsed",
    )
    with pytest.raises(ValueError, match="body parser"):
        FileAuditV2.model_validate(miss_without_body_parser)

    metadata = dict(valid_miss)
    metadata.update(
        parse_status="success",
        parser_backend="pdf_metadata_v2",
        worker_lane="rust_core",
        parse_cache_status="not_applicable",
        cache_miss_reason="",
        parse_duration_ms=0,
        parse_transport="not_applicable",
        parse_attempt_count=0,
        final_diagnostic=None,
        pdf_classification={
            **_pdf_classification_audit_payload(),
            "status": "no_text_in_parse_window",
            "result_examined_pages": 2,
            "run_inspected_pages": 2,
        },
    )
    FileAuditV2.model_validate(metadata)

    no_text_with_body_parser = dict(metadata)
    no_text_with_body_parser.update(
        parser_backend="python_pdf_text_v2",
        worker_lane="python_document_process_v2",
    )
    with pytest.raises(ValueError, match="metadata"):
        FileAuditV2.model_validate(no_text_with_body_parser)

    not_parsed_with_body_parser = dict(metadata)
    not_parsed_with_body_parser.update(
        parse_status="not_parsed",
        parser_backend="python_pdf_text_v2",
        worker_lane="python_document_process_v2",
        pdf_classification=None,
    )
    with pytest.raises(ValueError, match="not_parsed"):
        FileAuditV2.model_validate(not_parsed_with_body_parser)

    classifier_unknown = dict(valid_miss)
    classifier_unknown.update(
        parser_backend="not_parsed",
        worker_lane="not_parsed",
        parse_cache_status="not_applicable",
        cache_miss_reason="",
        parse_duration_ms=0,
        parse_transport="not_applicable",
        parse_attempt_count=0,
        pdf_classification={
            **_pdf_classification_audit_payload(),
            "status": "unknown",
            "page_count": None,
            "result_examined_pages": None,
            "run_inspected_pages": None,
        },
    )
    FileAuditV2.model_validate(classifier_unknown)

    unknown_with_body_parser = dict(classifier_unknown)
    unknown_with_body_parser.update(
        parser_backend="python_pdf_text_v2",
        worker_lane="python_document_process_v2",
    )
    with pytest.raises(ValueError, match="classifier failure"):
        FileAuditV2.model_validate(unknown_with_body_parser)
