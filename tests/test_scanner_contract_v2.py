"""Scanner profile v2 与 v2 wire 类型的跨语言 fixture 测试。"""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from src.models.scanner_contract import (
    BuildContextRequest,
    Diagnostic,
    FileAuditV2,
    InspectRunResponseV2,
    MaintenanceRequestV1,
    MaintenanceResponseV1,
    RawScannerProfileV2,
    UpgradeDatabaseRequestV1,
    UpgradeDatabaseResponseV1,
    VersionResponseV2,
    WORKER_DIAGNOSTIC_V1_ERROR_CODES,
    WORKER_DIAGNOSTIC_V1_STAGES,
    WorkerDiagnosticV1,
    WorkerParseResponse,
)
from src.services.scanner_config import (
    SCANNER_PROFILE_V2_ONLY_FIELDS,
    extract_scanner_profile,
    normalize_scanner_profile_v2,
)


def test_raw_profile_v2_is_strict_superset() -> None:
    # v1 默认叶子的 JSON 在 v2 中必须可解析
    v1_json = {"schema_version": "scanner_profile_v1", "max_file_size_mb": 50}
    v2 = RawScannerProfileV2.model_validate(
        {**v1_json, "schema_version": "scanner_profile_v2"}
    )
    assert v2.max_file_size_mb == 50


def test_version_v2_exposes_new_capabilities() -> None:
    r = VersionResponseV2.model_validate(
        {
            "contract": "ai_daily_context",
            "protocol_version": 1,
            "response_version": 2,
            "binary_name": "ai-daily-scanner",
            "engine_version": "0.1.0",
            "engine_build": "sha256-source-v1:" + "a" * 64,
            "target_triple": "x86_64-pc-windows-msvc",
            "supported_commands": [
                "version",
                "doctor",
                "build-context",
                "inspect-run",
                "maintenance",
                "upgrade-db",
            ],
            "office_worker_contract_version": "ai_daily_worker_v1",
            "python_worker_contract_version": "ai_daily_worker_v1",
            "accepted_scanner_profile_versions": [
                "scanner_profile_v1",
                "scanner_profile_v2",
            ],
            "inspect_response_versions": [1, 2],
            "classifier_contract_versions": ["ai_daily_pdf_classifier_v1"],
            "session_contract_versions": ["ai_daily_python_session_v1"],
            "maintenance_contract_versions": ["ai_daily_scanner_maintenance_v1"],
            "upgrade_contract_versions": ["ai_daily_scanner_upgrade_v1"],
            "source_guard_policy": "source_guard_v2",
            "max_source_files_per_run": 1_000_000,
            "cache_retention_policy": {
                "policy_version": "cache_retention_v1",
                "parse_cache_max_bytes": 1073741824,
                "classification_cache_max_bytes": 134217728,
                "context_artifacts_max_bytes": 536870912,
                "terminal_audit_max_bytes": 2147483648,
                "terminal_run_max_count": 500,
                "terminal_run_max_age_days": 90,
                "opportunistic_gc_budget_ms": 10,
            },
        }
    )
    assert r.max_source_files_per_run == 1_000_000


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
        "parser_backend": "python_sharepoint_text_v1",
        "worker_lane": "python_document_process",
        "truncated": False,
        "warnings": [],
        "error": {
            "error_code": error_code,
            "message": "synthetic worker failure",
            "retryable": False,
            "stage": stage,
            "file_path": "C:\\scanner-fixtures\\工作 目录\\legacy-export.doc",
            "backend": "python_sharepoint_text_v1",
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
    assert response.error.backend == "python_sharepoint_text_v1"


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


def test_v2_only_leaf_outputs_scanner_profile_v2() -> None:
    cfg = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".xlsx"],
            max_workers=3,
            total_deadline_ms=25_000,
        )
    )
    profile = extract_scanner_profile(cfg.scanner)
    assert profile["schema_version"] == "scanner_profile_v2"
    assert profile["total_deadline_ms"] == 25_000
    assert profile["allowed_extensions"] == [".xlsx"]


def test_without_v2_only_leaf_keeps_outputting_v1() -> None:
    cfg = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".xlsx"],
            max_workers=3,
        )
    )
    profile = extract_scanner_profile(cfg.scanner)
    assert profile["schema_version"] == "scanner_profile_v1"
    assert profile["max_workers"] == 3


def test_v2_only_allowlist_is_exact() -> None:
    assert SCANNER_PROFILE_V2_ONLY_FIELDS == frozenset(
        {
            "max_candidate_files",
            "max_pdf_text_extractions",
            "max_total_pdf_classification_pages",
            "admission_policy_version",
            "classifier_policy_version",
            "pdf_classification_timeout_ms",
            "total_deadline_ms",
            "session_concurrency",
            "max_requests_per_session",
            "session_idle_ttl_ms",
            "session_rss_limit_bytes",
        }
    )


def _build_context_request_payload(scanner_profile: dict) -> dict:
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
        "scanner_profile": scanner_profile,
        "adapters": {
            "office_worker_path": "C:\\scanner-fixtures\\bin\\ai-daily-office-parser.exe",
            "python_executable": "C:\\scanner-fixtures\\venv\\Scripts\\python.exe",
            "python_module_root": "C:\\scanner-fixtures\\repo",
            "python_document_worker_module": "src.workers.document_parser_worker",
        },
    }


def test_build_context_request_accepts_v1_profile() -> None:
    request = BuildContextRequest.model_validate(
        _build_context_request_payload(
            {"schema_version": "scanner_profile_v1", "max_workers": 3}
        )
    )
    assert request.scanner_profile.schema_version == "scanner_profile_v1"


def test_build_context_request_accepts_v2_profile_with_v2_only_leaf() -> None:
    request = BuildContextRequest.model_validate(
        _build_context_request_payload(
            {
                "schema_version": "scanner_profile_v2",
                "max_workers": 3,
                "total_deadline_ms": 45_000,
            }
        )
    )
    assert request.scanner_profile.schema_version == "scanner_profile_v2"
    assert request.scanner_profile.total_deadline_ms == 45_000


def test_build_context_request_rejects_unknown_profile_schema_version() -> None:
    with pytest.raises(ValueError, match="scanner_profile_v3"):
        BuildContextRequest.model_validate(
            _build_context_request_payload({"schema_version": "scanner_profile_v3"})
        )


def test_v1_and_v2_profiles_normalize_to_same_frozen_defaults() -> None:
    v1_leaves = {"schema_version": "scanner_profile_v1", "max_workers": 3}
    v2_leaves = {**v1_leaves, "schema_version": "scanner_profile_v2"}
    normalized_v1 = normalize_scanner_profile_v2(v1_leaves, "monthly")
    normalized_v2 = normalize_scanner_profile_v2(v2_leaves, "monthly")
    assert normalized_v1 == normalized_v2
    assert normalized_v1["total_deadline_ms"] == 25_000
    assert normalized_v1["max_candidate_files"] == 384
    assert normalized_v1["max_total_pdf_classification_pages"] == 370
    assert normalized_v1["max_pdf_text_extractions"] == 16
    assert normalized_v1["pdf_classification_timeout_ms"] == 2_000
    assert normalized_v1["session_concurrency"] == min(3, 4)


def test_v2_only_leaf_flows_through_request_mirror() -> None:
    request = BuildContextRequest.model_validate(
        _build_context_request_payload(
            {
                "schema_version": "scanner_profile_v2",
                "total_deadline_ms": 45_000,
            }
        )
    )
    normalized = normalize_scanner_profile_v2(
        request.scanner_profile.model_dump(mode="json"),
        request.report_mode,
    )
    assert normalized["total_deadline_ms"] == 45_000
    assert normalized["max_candidate_files"] == 384


def test_maintenance_request_and_response_round_trip() -> None:
    request = MaintenanceRequestV1.model_validate(
        {
            "contract": "ai_daily_scanner_maintenance",
            "protocol_version": 1,
            "request_id": "123e4567-e89b-42d3-a456-426614174000",
            "scan_db_path": "C:\\scan\\db.sqlite3",
            "mode": "gc",
            "dry_run": True,
        }
    )
    assert request.dry_run is True
    response = MaintenanceResponseV1.model_validate(
        {
            "contract": "ai_daily_scanner_maintenance",
            "protocol_version": 1,
            "request_id": "123e4567-e89b-42d3-a456-426614174000",
            "status": "ok",
            "cache_retention_policy": {
                "policy_version": "cache_retention_v1",
                "parse_cache_max_bytes": 1073741824,
                "classification_cache_max_bytes": 134217728,
                "context_artifacts_max_bytes": 536870912,
                "terminal_audit_max_bytes": 2147483648,
                "terminal_run_max_count": 500,
                "terminal_run_max_age_days": 90,
                "opportunistic_gc_budget_ms": 10,
            },
            "before": {
                "parse_cache_logical_bytes": 1,
                "classification_cache_logical_bytes": 2,
                "context_artifacts_logical_bytes": 3,
                "terminal_audit_logical_bytes": 4,
                "database_file_bytes": 5,
                "wal_file_bytes": 6,
                "shm_file_bytes": 7,
                "total_physical_bytes": 8,
                "freelist_bytes": 9,
                "auto_vacuum_mode": "incremental",
            },
            "after": {
                "parse_cache_logical_bytes": 1,
                "classification_cache_logical_bytes": 2,
                "context_artifacts_logical_bytes": 3,
                "terminal_audit_logical_bytes": 4,
                "database_file_bytes": 5,
                "wal_file_bytes": 6,
                "shm_file_bytes": 7,
                "total_physical_bytes": 8,
                "freelist_bytes": 9,
                "auto_vacuum_mode": "incremental",
            },
            "after_complete": True,
            "deleted": {
                "parse_cache_rows": 0,
                "classification_cache_rows": 0,
                "context_artifacts_rows": 0,
                "context_artifact_files_rows": 0,
                "context_artifact_decisions_rows": 0,
                "scan_runs_rows": 0,
                "scan_run_attempts_rows": 0,
                "run_diagnostics_rows": 0,
                "scan_file_results_rows": 0,
                "scan_stage_metrics_rows": 0,
                "scan_extension_metrics_rows": 0,
                "context_runs_rows": 0,
                "context_decisions_rows": 0,
                "file_inventory_rows": 0,
            },
            "pre_integrity_check": "ok",
            "post_integrity_check": "not_run",
            "vacuum": {
                "mode": "gc",
                "status": "skipped_dry_run",
                "pages_changed": 0,
            },
            "warnings": [],
            "error": None,
        }
    )
    assert response.status == "ok"
    assert response.error is None


def test_upgrade_database_request_and_response_round_trip() -> None:
    request = UpgradeDatabaseRequestV1.model_validate(
        {
            "contract": "ai_daily_scanner_upgrade",
            "protocol_version": 1,
            "request_id": "123e4567-e89b-42d3-a456-426614174000",
            "scan_db_path": "C:\\scan\\db.sqlite3",
            "apply": False,
        }
    )
    assert request.apply is False
    response = UpgradeDatabaseResponseV1.model_validate(
        {
            "contract": "ai_daily_scanner_upgrade",
            "protocol_version": 1,
            "request_id": "123e4567-e89b-42d3-a456-426614174000",
            "status": "ok",
            "source_user_version": 1,
            "target_user_version": 2,
            "apply": True,
            "schema_migrated": True,
            "auto_vacuum_converted": True,
            "legacy_parse_cache_rows_detected": 7,
            "invalidated_parse_cache_rows": 7,
            "pre_integrity_check": "ok",
            "post_integrity_check": "ok",
            "warnings": [],
            "error": None,
        }
    )
    assert response.schema_migrated is True


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
        "parser_backend": "pdf_text_v1",
        "worker_lane": "python_document_process",
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


def test_maintenance_response_v1_ok_rejects_invalid_post() -> None:
    payload = {
        "contract": "ai_daily_scanner_maintenance",
        "protocol_version": 1,
        "request_id": "123e4567-e89b-42d3-a456-426614174000",
        "status": "ok",
        "cache_retention_policy": {
            "policy_version": "cache_retention_v1",
            "parse_cache_max_bytes": 1073741824,
            "classification_cache_max_bytes": 134217728,
            "context_artifacts_max_bytes": 536870912,
            "terminal_audit_max_bytes": 2147483648,
            "terminal_run_max_count": 500,
            "terminal_run_max_age_days": 90,
            "opportunistic_gc_budget_ms": 10,
        },
        "before": {
            "parse_cache_logical_bytes": 1,
            "classification_cache_logical_bytes": 2,
            "context_artifacts_logical_bytes": 3,
            "terminal_audit_logical_bytes": 4,
            "database_file_bytes": 5,
            "wal_file_bytes": 6,
            "shm_file_bytes": 7,
            "total_physical_bytes": 8,
            "freelist_bytes": 9,
            "auto_vacuum_mode": "incremental",
        },
        "after": {
            "parse_cache_logical_bytes": 1,
            "classification_cache_logical_bytes": 2,
            "context_artifacts_logical_bytes": 3,
            "terminal_audit_logical_bytes": 4,
            "database_file_bytes": 5,
            "wal_file_bytes": 6,
            "shm_file_bytes": 7,
            "total_physical_bytes": 8,
            "freelist_bytes": 9,
            "auto_vacuum_mode": "incremental",
        },
        "after_complete": True,
        "deleted": {
            "parse_cache_rows": 0,
            "classification_cache_rows": 0,
            "context_artifacts_rows": 0,
            "context_artifact_files_rows": 0,
            "context_artifact_decisions_rows": 0,
            "scan_runs_rows": 0,
            "scan_run_attempts_rows": 0,
            "run_diagnostics_rows": 0,
            "scan_file_results_rows": 0,
            "scan_stage_metrics_rows": 0,
            "scan_extension_metrics_rows": 0,
            "context_runs_rows": 0,
            "context_decisions_rows": 0,
            "file_inventory_rows": 0,
        },
        "pre_integrity_check": "ok",
        "post_integrity_check": "not_run",
        "vacuum": {"mode": "gc", "status": "skipped_dry_run", "pages_changed": 0},
        "warnings": [],
        "error": None,
    }
    MaintenanceResponseV1.model_validate(payload)
    payload["post_integrity_check"] = "failed"
    with pytest.raises(ValueError, match="status invariants"):
        MaintenanceResponseV1.model_validate(payload)
    payload["post_integrity_check"] = "not_run"
    payload["after_complete"] = False
    with pytest.raises(ValueError, match="status invariants"):
        MaintenanceResponseV1.model_validate(payload)
