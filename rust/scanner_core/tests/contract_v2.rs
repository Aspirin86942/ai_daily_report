//! Contract fixtures for scanner profile v2 and the v2 wire surface.
//!
//! RawScannerProfileV2 is a strict superset of RawScannerProfileV1: every v1
//! leaf must parse under v2 with the same value, and the v2-only leaves must
//! fall back to the frozen report-mode defaults during normalization.

use ai_daily_scanner_contract::{RawScannerProfileV2, ReportMode, Validate};

#[test]
fn raw_profile_v2_defaults_map_like_v1() {
    let raw = serde_json::from_str::<RawScannerProfileV2>(
        r#"{"schema_version":"scanner_profile_v2"}"#,
    )
    .expect("minimal v2 raw profile");
    // v2 是 v1 严格超集：v1 的所有叶子在 v2 里同样解析
    assert_eq!(raw.schema_version, "scanner_profile_v2");
    raw.validate().expect("minimal v2 raw profile must validate");
}

#[test]
fn every_v1_leaf_parses_under_v2_and_preserves_value() {
    let v1_json: serde_json::Value = serde_json::from_str(
        r#"{
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".xlsx", ".txt"],
        "ignored_patterns": ["~$*", "*.tmp"],
        "excluded_dirs": ["archive"],
        "max_workers": 3,
        "max_file_size_mb": 50,
        "discovery_timeout_seconds": 30,
        "file_timeout_seconds": 45,
        "file_timeout_by_extension": {".pdf": 60, ".xlsx": 60},
        "total_max_chars": 50000,
        "parser_profile_version": "v1",
        "office_parser_backend": "rust_office_oxide_v1",
        "pdf_parser_backend": "pdf_text_v1",
        "office_fallback_policy_version": "hybrid_v1",
        "office_parser_fallback_enabled": true,
        "office_fallback_after_timeout": true,
        "office_legacy_extensions_enabled": true,
        "pptx_include_notes": true,
        "office_parser_fallback_order": ["python_office_v1"],
        "direct_text_max_bytes": 262144,
        "direct_text_read_bytes": 131072,
        "log_tail_read_bytes": 131072,
        "text_excerpt_max_chars": 6000,
        "excel_max_rows": 50,
        "pdf_max_pages": 5,
        "text_max_chars": 6000,
        "excel_max_sheets": 5,
        "excel_max_columns": 20,
        "docx_max_paragraphs": 200,
        "docx_max_tables": 20,
        "docx_table_max_rows": 50,
        "docx_table_max_cols": 12,
        "pptx_max_slides": 50,
        "document_excerpt_max_chars": 6000,
        "summary_excel_max_rows": 10,
        "summary_pdf_max_pages": 2,
        "summary_text_max_chars": 2000,
        "summary_excel_max_sheets": 2,
        "summary_excel_max_columns": 12,
        "summary_docx_max_paragraphs": 80,
        "summary_docx_max_tables": 8,
        "summary_docx_table_max_rows": 20,
        "summary_docx_table_max_cols": 8,
        "summary_pptx_max_slides": 15,
        "summary_document_excerpt_max_chars": 2000
    }"#,
    )
    .expect("v1 fixture should parse");
    let mut v2_json = v1_json.as_object().unwrap().clone();
    v2_json.insert(
        "schema_version".to_string(),
        serde_json::Value::String("scanner_profile_v2".to_string()),
    );
    let v2 = serde_json::from_value::<RawScannerProfileV2>(serde_json::Value::Object(v2_json))
        .expect("every v1 leaf must parse under v2");
    v2.validate().expect("v2 raw profile must validate");

    assert_eq!(v2.max_workers, Some(3));
    assert_eq!(v2.max_file_size_mb, Some(50));
    assert_eq!(v2.pdf_max_pages, Some(5));
    assert_eq!(v2.summary_pdf_max_pages, Some(2));
    assert_eq!(
        v2.office_parser_fallback_order.as_deref(),
        Some(&[ai_daily_scanner_contract::FallbackBackend::PythonOfficeV1][..])
    );
}

#[test]
fn v2_only_leaves_are_optional_and_validated() {
    let raw = serde_json::from_str::<RawScannerProfileV2>(r#"{
        "schema_version": "scanner_profile_v2",
        "max_candidate_files": 1000000,
        "max_pdf_text_extractions": 0,
        "max_total_pdf_classification_pages": 0,
        "pdf_classification_timeout_ms": 2000,
        "total_deadline_ms": 25000,
        "session_concurrency": 8,
        "max_requests_per_session": 10000,
        "session_idle_ttl_ms": 600000,
        "session_rss_limit_bytes": 8589934592
    }"#)
    .expect("v2-only leaves should be accepted at their range edges");
    raw.validate().expect("v2-only leaves must validate at range edges");

    let out_of_range = serde_json::from_str::<RawScannerProfileV2>(r#"{
        "schema_version": "scanner_profile_v2",
        "total_deadline_ms": 4999
    }"#)
    .expect("out-of-range v2 leaf should still decode");
    assert!(out_of_range.validate().is_err());

    let wrong_policy = serde_json::from_str::<RawScannerProfileV2>(r#"{
        "schema_version": "scanner_profile_v2",
        "admission_policy_version": "budget_admission_v1"
    }"#)
    .expect("non-constant policy version should still decode");
    assert!(
        wrong_policy.validate().is_err(),
        "policy version leaves are constants and must reject other values"
    );

    let unknown = serde_json::from_str::<RawScannerProfileV2>(r#"{
        "schema_version": "scanner_profile_v2",
        "not_a_leaf": 1
    }"#);
    assert!(unknown.is_err(), "v2 raw profile must reject unknown fields");
}

#[test]
fn normalize_v2_merges_frozen_report_mode_defaults() {
    use ai_daily_scanner_contract::v2_quota_defaults;
    use ai_daily_scanner_core::config::normalize_scanner_profile_v2;

    for (mode, expected) in [
        (ReportMode::Daily, (96, 80, 8, 10_000)),
        (ReportMode::Weekly, (192, 100, 12, 15_000)),
        (ReportMode::Monthly, (384, 370, 16, 25_000)),
    ] {
        assert_eq!(v2_quota_defaults(mode), expected, "{mode:?}");
        let raw: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
            "schema_version": "scanner_profile_v2"
        }))
        .expect("minimal raw profile should decode");
        let normalized =
            normalize_scanner_profile_v2(&raw, mode).expect("minimal raw profile should normalize");
        normalized.validate().expect("normalized v2 must validate");
        assert_eq!(normalized.max_candidate_files, expected.0);
        assert_eq!(normalized.max_total_pdf_classification_pages, expected.1);
        assert_eq!(normalized.max_pdf_text_extractions, expected.2);
        assert_eq!(normalized.total_deadline_ms, expected.3);
        assert_eq!(
            normalized.pdf_classification_timeout_ms,
            2_000,
            "classifier timeout defaults to 2,000ms for every report mode"
        );
        assert_eq!(
            normalized.session_concurrency,
            normalized.execution.max_workers.min(4)
        );
        assert_eq!(normalized.max_requests_per_session, 128);
        assert_eq!(normalized.session_idle_ttl_ms, 30_000);
        assert_eq!(normalized.session_rss_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(
            normalized.admission_policy_version,
            ai_daily_scanner_contract::ADMISSION_POLICY_VERSION
        );
        assert_eq!(
            normalized.classifier_policy_version,
            ai_daily_scanner_contract::CLASSIFIER_POLICY_VERSION
        );
        assert_eq!(
            normalized.context.priority_policy_version,
            ai_daily_scanner_contract::PRIORITY_POLICY_VERSION
        );
        assert_eq!(
            normalized.context.compression_policy_version,
            ai_daily_scanner_contract::COMPRESSION_POLICY_VERSION
        );
    }
}

#[test]
fn normalize_v2_keeps_report_mode_pdf_page_defaults() {
    use ai_daily_scanner_core::config::normalize_scanner_profile_v2;

    let raw: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
        "schema_version": "scanner_profile_v2"
    }))
    .expect("minimal raw profile should decode");

    let daily = normalize_scanner_profile_v2(&raw, ReportMode::Daily)
        .expect("daily profile should normalize");
    assert_eq!(daily.parse.pdf.max_pages, 5, "daily keeps pdf_max_pages=5");

    let weekly = normalize_scanner_profile_v2(&raw, ReportMode::Weekly)
        .expect("weekly profile should normalize");
    assert_eq!(
        weekly.parse.pdf.max_pages, 2,
        "weekly keeps summary_pdf_max_pages=2"
    );

    let monthly = normalize_scanner_profile_v2(&raw, ReportMode::Monthly)
        .expect("monthly profile should normalize");
    assert_eq!(
        monthly.parse.pdf.max_pages, 2,
        "monthly keeps summary_pdf_max_pages=2"
    );
}

fn inspect_v2_ok_json() -> serde_json::Value {
    serde_json::from_str(
        r#"{
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "response_version": 2,
        "request_id": "123e4567-e89b-42d3-a456-426614174000",
        "scan_run_id": 1,
        "context_run_id": 1,
        "status": "ok",
        "run_status": "success",
        "summary": {
            "source_file_count": 1, "success_count": 1, "timeout_count": 0,
            "included_file_count": 1, "omitted_file_count": 0, "error_file_count": 0,
            "input_chars": 1, "output_chars": 1, "total_duration_ms": 1,
            "discovery_duration_ms": 0, "parse_duration_ms": 0, "compression_duration_ms": 0
        },
        "stage_metrics": [], "extension_metrics": [], "files": [], "decisions": [],
        "warnings": [], "error": null,
        "artifact_id": 1,
        "reused_from_context_run_id": null,
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
            "snapshot_hit": false,
            "parse_cache_lookup_count": 0,
            "classification_cache_lookup_count": 0,
            "parse_cache_all_hit": null,
            "classification_cache_all_hit": null,
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
            "peak_worker_rss_bytes": null
        }
    }"#,
    )
    .expect("base inspect v2 fixture should parse")
}

fn maintenance_response_v1_json() -> serde_json::Value {
    serde_json::from_str(
        r#"{
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
            "opportunistic_gc_budget_ms": 10
        },
        "before": {
            "parse_cache_logical_bytes": 1, "classification_cache_logical_bytes": 2,
            "context_artifacts_logical_bytes": 3, "terminal_audit_logical_bytes": 4,
            "database_file_bytes": 5, "wal_file_bytes": 6, "shm_file_bytes": 7,
            "total_physical_bytes": 8, "freelist_bytes": 9, "auto_vacuum_mode": "incremental"
        },
        "after": {
            "parse_cache_logical_bytes": 1, "classification_cache_logical_bytes": 2,
            "context_artifacts_logical_bytes": 3, "terminal_audit_logical_bytes": 4,
            "database_file_bytes": 5, "wal_file_bytes": 6, "shm_file_bytes": 7,
            "total_physical_bytes": 8, "freelist_bytes": 9, "auto_vacuum_mode": "incremental"
        },
        "after_complete": true,
        "deleted": {
            "parse_cache_rows": 0, "classification_cache_rows": 0,
            "context_artifacts_rows": 0, "context_artifact_files_rows": 0,
            "context_artifact_decisions_rows": 0, "scan_runs_rows": 0,
            "scan_run_attempts_rows": 0, "run_diagnostics_rows": 0,
            "scan_file_results_rows": 0, "scan_stage_metrics_rows": 0,
            "scan_extension_metrics_rows": 0, "context_runs_rows": 0,
            "context_decisions_rows": 0, "file_inventory_rows": 0
        },
        "pre_integrity_check": "ok",
        "post_integrity_check": "not_run",
        "vacuum": { "mode": "gc", "status": "skipped_dry_run", "pages_changed": 0 },
        "warnings": [],
        "error": null
    }"#,
    )
    .expect("base maintenance fixture should parse")
}

#[test]
fn maintenance_response_v1_dry_run_ok_round_trips() {
    let value = maintenance_response_v1_json();
    let response = serde_json::from_value::<ai_daily_scanner_contract::MaintenanceResponseV1>(value)
        .expect("dry-run ok maintenance response should decode");
    response
        .validate()
        .expect("dry-run ok maintenance response must validate (post=not_run, after_complete=true)");
}

#[test]
fn maintenance_response_v1_ok_rejects_invalid_post_or_missing_complete() {
    let value = maintenance_response_v1_json();
    let mut failed_post = value.as_object().unwrap().clone();
    failed_post.insert(
        "post_integrity_check".to_string(),
        serde_json::Value::String("failed".to_string()),
    );
    let response =
        serde_json::from_value::<ai_daily_scanner_contract::MaintenanceResponseV1>(
            serde_json::Value::Object(failed_post),
        )
        .expect("failed post should still decode");
    assert!(response.validate().is_err());

    let mut incomplete = value.as_object().unwrap().clone();
    incomplete.insert(
        "after_complete".to_string(),
        serde_json::Value::Bool(false),
    );
    let response =
        serde_json::from_value::<ai_daily_scanner_contract::MaintenanceResponseV1>(
            serde_json::Value::Object(incomplete),
        )
        .expect("missing after_complete should still decode");
    assert!(response.validate().is_err());
}

#[test]
fn inspect_run_response_v2_ok_success_requires_artifact_id() {
    let value = inspect_v2_ok_json();
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::InspectRunResponseV2>(value)
        .expect("ok success fixture should decode");
    parsed
        .validate()
        .expect("ok success with artifact_id must validate");

    let mut missing = inspect_v2_ok_json().as_object().unwrap().clone();
    missing.insert("artifact_id".to_string(), serde_json::Value::Null);
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::InspectRunResponseV2>(
        serde_json::Value::Object(missing),
    )
    .expect("missing artifact_id should still decode");
    assert!(
        parsed.validate().is_err(),
        "success/partial inspect v2 run requires artifact_id"
    );
}

#[test]
fn inspect_run_response_v2_error_sentinel_is_enforced() {
    let mut value = inspect_v2_ok_json().as_object().unwrap().clone();
    value.insert("status".to_string(), serde_json::Value::String("error".to_string()));
    value.insert("run_status".to_string(), serde_json::Value::Null);
    value.insert("context_run_id".to_string(), serde_json::Value::Null);
    value.insert("artifact_id".to_string(), serde_json::Value::Null);
    value.insert(
        "error".to_string(),
        serde_json::json!({
            "error_code": "INSPECT_V2_PROVENANCE_UNAVAILABLE",
            "message": "migrated v1 run lacks v2 provenance",
            "retryable": false,
            "stage": "inspect",
            "file_path": null,
            "backend": null
        }),
    );
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::InspectRunResponseV2>(
        serde_json::Value::Object(value),
    )
    .expect("error sentinel fixture should decode");
    parsed
        .validate()
        .expect("error sentinel shape with zero metrics must validate");

    let mut nonzero = inspect_v2_ok_json().as_object().unwrap().clone();
    nonzero.insert("status".to_string(), serde_json::Value::String("error".to_string()));
    nonzero.insert("run_status".to_string(), serde_json::Value::Null);
    nonzero.insert("context_run_id".to_string(), serde_json::Value::Null);
    nonzero.insert("artifact_id".to_string(), serde_json::Value::Null);
    nonzero.insert(
        "error".to_string(),
        serde_json::json!({
            "error_code": "INSPECT_V2_PROVENANCE_UNAVAILABLE",
            "message": "migrated v1 run lacks v2 provenance",
            "retryable": false,
            "stage": "inspect",
            "file_path": null,
            "backend": null
        }),
    );
    let metrics = nonzero
        .get_mut("execution_metrics")
        .and_then(|value| value.as_object_mut())
        .expect("execution_metrics must be an object");
    metrics.insert(
        "discovery_observed_file_count".to_string(),
        serde_json::Value::from(1_u64),
    );
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::InspectRunResponseV2>(
        serde_json::Value::Object(nonzero),
    )
    .expect("nonzero error metrics should still decode");
    assert!(
        parsed.validate().is_err(),
        "error inspect v2 requires the fixed zero sentinel metrics"
    );
}

#[test]
fn inspect_run_response_v2_parse_cache_reuse_requires_all_hit() {
    let mut value = inspect_v2_ok_json().as_object().unwrap().clone();
    value.insert(
        "reuse_kind".to_string(),
        serde_json::Value::String("parse_cache".to_string()),
    );
    let metrics = value
        .get_mut("execution_metrics")
        .and_then(|value| value.as_object_mut())
        .expect("execution_metrics must be an object");
    metrics.insert(
        "parse_cache_lookup_count".to_string(),
        serde_json::Value::from(1_u64),
    );
    metrics.insert(
        "parse_cache_all_hit".to_string(),
        serde_json::Value::Bool(true),
    );
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::InspectRunResponseV2>(
        serde_json::Value::Object(value),
    )
    .expect("parse_cache reuse fixture should decode");
    parsed
        .validate()
        .expect("parse_cache reuse with lookup all-hit must validate");

    let mut partial = inspect_v2_ok_json().as_object().unwrap().clone();
    partial.insert(
        "reuse_kind".to_string(),
        serde_json::Value::String("parse_cache".to_string()),
    );
    let metrics = partial
        .get_mut("execution_metrics")
        .and_then(|value| value.as_object_mut())
        .expect("execution_metrics must be an object");
    metrics.insert(
        "parse_cache_lookup_count".to_string(),
        serde_json::Value::from(1_u64),
    );
    metrics.insert(
        "parse_cache_all_hit".to_string(),
        serde_json::Value::Bool(false),
    );
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::InspectRunResponseV2>(
        serde_json::Value::Object(partial),
    )
    .expect("partial parse_cache reuse should still decode");
    assert!(
        parsed.validate().is_err(),
        "parse_cache reuse requires parse_cache_all_hit=true"
    );
}

#[test]
fn file_audit_v2_final_diagnostic_nullability_is_enforced() {
    let base = serde_json::json!({
        "relative_path": "evidence.pdf",
        "file_identity": "C:\\evidence\\evidence.pdf",
        "source_version": "mtime_ns=1:size=1",
        "source_guard_kind": "content_sha256_v1",
        "source_guard_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "parser_backend": "pdf_text_v1",
        "worker_lane": "python_document_process",
        "parse_cache_status": "miss",
        "cache_miss_reason": "new_file",
        "truncated": false,
        "content_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "parse_duration_ms": 1,
        "failure_class": "",
        "fallback_backend": "",
        "fallback_reason_code": "",
        "parse_transport": "one_shot",
        "parse_attempt_count": 1
    });
    let diagnostic = serde_json::json!({
        "error_code": "PARSER_FAILED",
        "message": "synthetic failure",
        "retryable": false,
        "stage": "parse",
        "file_path": null,
        "backend": null
    });

    let mut error_null = base.as_object().unwrap().clone();
    error_null.insert(
        "parse_status".to_string(),
        serde_json::Value::String("error".to_string()),
    );
    error_null.insert(
        "final_diagnostic".to_string(),
        serde_json::Value::Null,
    );
    error_null.insert(
        "pdf_classification".to_string(),
        serde_json::Value::Null,
    );
    let parsed =
        serde_json::from_value::<ai_daily_scanner_contract::FileAuditV2>(serde_json::Value::Object(
            error_null,
        ))
        .expect("error file audit with null diagnostic should decode");
    assert!(
        parsed.validate().is_err(),
        "error/timeout file audit requires a final diagnostic"
    );

    let mut success_with_diag = base.as_object().unwrap().clone();
    success_with_diag.insert(
        "parse_status".to_string(),
        serde_json::Value::String("success".to_string()),
    );
    success_with_diag.insert("final_diagnostic".to_string(), diagnostic.clone());
    success_with_diag.insert(
        "pdf_classification".to_string(),
        serde_json::Value::Null,
    );
    let parsed =
        serde_json::from_value::<ai_daily_scanner_contract::FileAuditV2>(serde_json::Value::Object(
            success_with_diag,
        ))
        .expect("success file audit with diagnostic should decode");
    assert!(
        parsed.validate().is_err(),
        "success/not_parsed file audit must not carry a final diagnostic"
    );

    let mut error_with_diag = base.as_object().unwrap().clone();
    error_with_diag.insert(
        "parse_status".to_string(),
        serde_json::Value::String("error".to_string()),
    );
    error_with_diag.insert("final_diagnostic".to_string(), diagnostic);
    error_with_diag.insert(
        "pdf_classification".to_string(),
        serde_json::Value::Null,
    );
    let parsed =
        serde_json::from_value::<ai_daily_scanner_contract::FileAuditV2>(serde_json::Value::Object(
            error_with_diag,
        ))
        .expect("error file audit with diagnostic should decode");
    parsed
        .validate()
        .expect("error file audit with a final diagnostic must validate");
}
