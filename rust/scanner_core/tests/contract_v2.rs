//! Contract fixtures for scanner profile v2 and the v2 wire surface.
//!
//! RawScannerProfileV2 is a strict superset of RawScannerProfileV1: every v1
//! leaf must parse under v2 with the same value, and the v2-only leaves must
//! fall back to the frozen report-mode defaults during normalization.

use ai_daily_scanner_contract::{
    BuildContextRequest, RawScannerProfileV1, RawScannerProfileV2, ReportMode, ScannerProfile,
    Validate,
};

#[test]
fn raw_profile_v2_defaults_map_like_v1() {
    let raw =
        serde_json::from_str::<RawScannerProfileV2>(r#"{"schema_version":"scanner_profile_v2"}"#)
            .expect("minimal v2 raw profile");
    // v2 是 v1 严格超集：v1 的所有叶子在 v2 里同样解析
    assert_eq!(raw.schema_version, "scanner_profile_v2");
    raw.validate()
        .expect("minimal v2 raw profile must validate");
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
    let raw = serde_json::from_str::<RawScannerProfileV2>(
        r#"{
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
    }"#,
    )
    .expect("v2-only leaves should be accepted at their range edges");
    raw.validate()
        .expect("v2-only leaves must validate at range edges");

    let out_of_range = serde_json::from_str::<RawScannerProfileV2>(
        r#"{
        "schema_version": "scanner_profile_v2",
        "total_deadline_ms": 4999
    }"#,
    )
    .expect("out-of-range v2 leaf should still decode");
    assert!(out_of_range.validate().is_err());

    let wrong_policy = serde_json::from_str::<RawScannerProfileV2>(
        r#"{
        "schema_version": "scanner_profile_v2",
        "admission_policy_version": "budget_admission_v1"
    }"#,
    )
    .expect("non-constant policy version should still decode");
    assert!(
        wrong_policy.validate().is_err(),
        "policy version leaves are constants and must reject other values"
    );

    let unknown = serde_json::from_str::<RawScannerProfileV2>(
        r#"{
        "schema_version": "scanner_profile_v2",
        "not_a_leaf": 1
    }"#,
    );
    assert!(
        unknown.is_err(),
        "v2 raw profile must reject unknown fields"
    );
}

#[test]
fn normalize_v2_merges_frozen_report_mode_defaults() {
    use ai_daily_scanner_contract::v2_quota_defaults;
    use ai_daily_scanner_core::config::normalize_scanner_profile_v2;

    for (mode, expected) in [
        (ReportMode::Daily, (96, 800, 8, 10_000)),
        (ReportMode::Weekly, (192, 1200, 12, 15_000)),
        (ReportMode::Monthly, (384, 1600, 16, 25_000)),
    ] {
        assert_eq!(v2_quota_defaults(mode), expected, "{mode:?}");
        let raw: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
            "schema_version": "scanner_profile_v2"
        }))
        .expect("minimal raw profile should decode");
        let normalized = normalize_scanner_profile_v2(&ScannerProfile::V2(raw), mode)
            .expect("minimal raw profile should normalize");
        normalized.validate().expect("normalized v2 must validate");
        assert_eq!(normalized.max_candidate_files, expected.0);
        assert_eq!(normalized.max_total_pdf_classification_pages, expected.1);
        assert_eq!(normalized.max_pdf_text_extractions, expected.2);
        assert_eq!(normalized.total_deadline_ms, expected.3);
        assert_eq!(
            normalized.pdf_classification_timeout_ms, 10_000,
            "classifier timeout defaults to 10,000ms for every report mode"
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
fn normalize_defaults_preserve_full_file_content_budget() {
    use ai_daily_scanner_core::config::{normalize_scanner_profile, normalize_scanner_profile_v2};

    for mode in [
        ReportMode::Daily,
        ReportMode::Weekly,
        ReportMode::Monthly,
    ] {
        let raw_v2: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
            "schema_version": "scanner_profile_v2"
        }))
        .expect("minimal v2 raw profile should decode");
        let normalized = normalize_scanner_profile_v2(&ScannerProfile::V2(raw_v2), mode)
            .expect("minimal v2 profile should normalize");
        assert_eq!(normalized.context.global_max_chars, 500_000, "{mode:?}");
        assert_eq!(normalized.context.per_file_max_chars, 100_000, "{mode:?}");
        assert_eq!(normalized.context.compression_policy_version, "markdown_context_v3");
        assert_eq!(normalized.parse.text.max_chars, 100_000, "{mode:?}");
        assert_eq!(normalized.parse.text.read_head_bytes, 2 * 1024 * 1024, "{mode:?}");
        assert_eq!(normalized.parse.text.read_tail_bytes, 2 * 1024 * 1024, "{mode:?}");
        assert_eq!(normalized.parse.pdf.max_pages, 100, "{mode:?}");
        assert_eq!(normalized.parse.office.excel_max_rows, 20_000, "{mode:?}");
        assert_eq!(normalized.parse.office.excel_max_sheets, 100, "{mode:?}");
        assert_eq!(normalized.parse.office.docx_max_paragraphs, 50_000, "{mode:?}");
        assert_eq!(normalized.parse.office.pptx_max_slides, 500, "{mode:?}");
        assert_eq!(normalized.parse.office.document_excerpt_max_chars, 100_000, "{mode:?}");
        assert_eq!(normalized.parse.aggregate_max_chars, 500_000, "{mode:?}");

        let raw_v1: RawScannerProfileV1 = serde_json::from_value(serde_json::json!({
            "schema_version": "scanner_profile_v1"
        }))
        .expect("minimal v1 raw profile should decode");
        let v1 = normalize_scanner_profile(&raw_v1, mode).expect("minimal v1 profile should normalize");
        assert_eq!(v1.context.global_max_chars, 500_000, "{mode:?}");
        assert_eq!(v1.context.per_file_max_chars, 100_000, "{mode:?}");
        assert_eq!(v1.parse.text.max_chars, 100_000, "{mode:?}");
        assert_eq!(v1.parse.office.document_excerpt_max_chars, 100_000, "{mode:?}");
        assert_eq!(v1.parse.pdf.max_pages, 100, "{mode:?}");
        assert_eq!(v1.parse.aggregate_max_chars, 500_000, "{mode:?}");
    }
}

#[test]
fn normalize_v2_unifies_pdf_page_defaults() {
    use ai_daily_scanner_core::config::normalize_scanner_profile_v2;

    let raw: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
        "schema_version": "scanner_profile_v2"
    }))
    .expect("minimal raw profile should decode");

    let daily = normalize_scanner_profile_v2(&ScannerProfile::V2(raw.clone()), ReportMode::Daily)
        .expect("daily profile should normalize");
    assert_eq!(daily.parse.pdf.max_pages, 100, "daily pdf_max_pages unifies at 100");

    let weekly = normalize_scanner_profile_v2(&ScannerProfile::V2(raw.clone()), ReportMode::Weekly)
        .expect("weekly profile should normalize");
    assert_eq!(
        weekly.parse.pdf.max_pages, 100,
        "weekly pdf_max_pages unifies at 100"
    );

    let monthly = normalize_scanner_profile_v2(&ScannerProfile::V2(raw), ReportMode::Monthly)
        .expect("monthly profile should normalize");
    assert_eq!(
        monthly.parse.pdf.max_pages, 100,
        "monthly pdf_max_pages unifies at 100"
    );
}

#[test]
fn scanner_profile_union_parses_both_variants() {
    let v1 = serde_json::from_str::<ScannerProfile>(
        r#"{"schema_version":"scanner_profile_v1","max_workers":3}"#,
    )
    .expect("v1 variant should parse");
    assert_eq!(v1.schema_version(), "scanner_profile_v1");
    v1.validate().expect("v1 variant must validate");

    let v2 = serde_json::from_str::<ScannerProfile>(
        r#"{"schema_version":"scanner_profile_v2","total_deadline_ms":25000}"#,
    )
    .expect("v2 variant should parse");
    assert_eq!(v2.schema_version(), "scanner_profile_v2");
    v2.validate().expect("v2 variant must validate");
}

#[test]
fn scanner_profile_union_rejects_unknown_schema_version() {
    let result =
        serde_json::from_str::<ScannerProfile>(r#"{"schema_version":"scanner_profile_v3"}"#);
    assert!(
        result.is_err(),
        "unknown schema_version must be rejected at the union boundary"
    );
}

fn build_context_request_json(scanner_profile: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "11111111-1111-4111-8111-111111111111",
        "work_dir": "C:\\scanner-fixtures\\工作 目录",
        "start_date": "2026-07-14",
        "end_date": "2026-07-15",
        "report_mode": "monthly",
        "compression_profile": null,
        "scan_db_path": "C:\\scanner-fixtures\\state\\scan-index-v2.sqlite3",
        "scanner_profile": scanner_profile,
        "adapters": {
            "office_worker_path": "C:\\scanner-fixtures\\bin\\ai-daily-office-parser.exe",
            "python_executable": "C:\\scanner-fixtures\\venv\\Scripts\\python.exe",
            "python_module_root": "C:\\scanner-fixtures\\repo",
            "python_document_worker_module": "src.workers.document_parser_worker"
        }
    })
}

#[test]
fn v1_requests_are_normalized_to_v2_with_frozen_defaults() {
    use ai_daily_scanner_core::config::normalize_scanner_profile_v2;

    let request: BuildContextRequest = serde_json::from_value(build_context_request_json(
        serde_json::json!({"schema_version": "scanner_profile_v1"}),
    ))
    .expect("v1 request should decode");
    request
        .validate()
        .expect("v1 build-context request must validate");
    assert!(
        matches!(request.scanner_profile, ScannerProfile::V1(_)),
        "v1 request must carry the v1 union variant"
    );

    let normalized = normalize_scanner_profile_v2(&request.scanner_profile, request.report_mode)
        .expect("v1 request should normalize to v2");
    assert_eq!(
        normalized.total_deadline_ms, 25_000,
        "monthly v1 request fills the frozen monthly deadline"
    );
    assert_eq!(normalized.max_candidate_files, 384);
    assert_eq!(normalized.max_total_pdf_classification_pages, 1600);
    assert_eq!(normalized.max_pdf_text_extractions, 16);
    assert_eq!(
        normalized.parse.pdf.max_pages, 100,
        "monthly unifies pdf max pages at 100"
    );
    assert_eq!(
        normalized.pdf_classification_timeout_ms, 10_000,
        "classifier timeout defaults to 10,000ms"
    );
}

#[test]
fn v2_leaf_flows_through_request() {
    use ai_daily_scanner_core::config::normalize_scanner_profile_v2;

    let request: BuildContextRequest =
        serde_json::from_value(build_context_request_json(serde_json::json!({
            "schema_version": "scanner_profile_v2",
            "total_deadline_ms": 45_000
        })))
        .expect("v2 request should decode");
    request
        .validate()
        .expect("v2 build-context request must validate");
    assert!(
        matches!(request.scanner_profile, ScannerProfile::V2(_)),
        "v2 request must carry the v2 union variant"
    );

    let normalized = normalize_scanner_profile_v2(&request.scanner_profile, request.report_mode)
        .expect("v2 request should normalize");
    assert_eq!(
        normalized.total_deadline_ms, 45_000,
        "v2-only leaf must survive normalization"
    );
    assert_eq!(
        normalized.max_candidate_files, 384,
        "other v2 leaves still fall back to the frozen monthly defaults"
    );
}

#[test]
fn build_context_request_round_trip_preserves_v1_profile_json() {
    let fixture = include_str!("../../../tests/fixtures/scanner_contract/v1/request.json");
    let value: serde_json::Value = serde_json::from_str(fixture).expect("fixture should parse");
    let request: BuildContextRequest =
        serde_json::from_value(value.clone()).expect("v1 fixture request should decode");
    request
        .validate()
        .expect("v1 fixture request must validate");
    let round_trip = serde_json::to_value(&request).expect("request should serialize");
    assert_eq!(round_trip, value, "v1 request JSON shape must not change");
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
    let response =
        serde_json::from_value::<ai_daily_scanner_contract::MaintenanceResponseV1>(value)
            .expect("dry-run ok maintenance response should decode");
    response.validate().expect(
        "dry-run ok maintenance response must validate (post=not_run, after_complete=true)",
    );
}

#[test]
fn maintenance_response_v1_ok_rejects_invalid_post_or_missing_complete() {
    let value = maintenance_response_v1_json();
    let mut failed_post = value.as_object().unwrap().clone();
    failed_post.insert(
        "post_integrity_check".to_string(),
        serde_json::Value::String("failed".to_string()),
    );
    let response = serde_json::from_value::<ai_daily_scanner_contract::MaintenanceResponseV1>(
        serde_json::Value::Object(failed_post),
    )
    .expect("failed post should still decode");
    assert!(response.validate().is_err());

    let mut incomplete = value.as_object().unwrap().clone();
    incomplete.insert("after_complete".to_string(), serde_json::Value::Bool(false));
    let response = serde_json::from_value::<ai_daily_scanner_contract::MaintenanceResponseV1>(
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
    value.insert(
        "status".to_string(),
        serde_json::Value::String("error".to_string()),
    );
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
    nonzero.insert(
        "status".to_string(),
        serde_json::Value::String("error".to_string()),
    );
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
    error_null.insert("final_diagnostic".to_string(), serde_json::Value::Null);
    error_null.insert("pdf_classification".to_string(), serde_json::Value::Null);
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::FileAuditV2>(
        serde_json::Value::Object(error_null),
    )
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
    success_with_diag.insert("pdf_classification".to_string(), serde_json::Value::Null);
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::FileAuditV2>(
        serde_json::Value::Object(success_with_diag),
    )
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
    error_with_diag.insert("pdf_classification".to_string(), serde_json::Value::Null);
    let parsed = serde_json::from_value::<ai_daily_scanner_contract::FileAuditV2>(
        serde_json::Value::Object(error_with_diag),
    )
    .expect("error file audit with diagnostic should decode");
    parsed
        .validate()
        .expect("error file audit with a final diagnostic must validate");
}

#[test]
fn classification_and_parse_execution_matrices_reject_impossible_provenance() {
    use ai_daily_scanner_contract::{
        ClassificationCacheStatus, ClassificationTransport, PdfClassificationAuditV1,
        PdfClassificationStatus,
    };

    let impossible_snapshot = PdfClassificationAuditV1 {
        status: PdfClassificationStatus::NotClassifiedByBudget,
        page_count: ai_daily_scanner_contract::Nullable(None),
        classification_cache_status: ClassificationCacheStatus::Snapshot,
        classification_cache_miss_reason: String::new(),
        result_examined_pages: ai_daily_scanner_contract::Nullable(Some(0)),
        run_inspected_pages: ai_daily_scanner_contract::Nullable(Some(0)),
        nominal_charged_pages: 0,
        duration_ms: 0,
        transport: ClassificationTransport::Snapshot,
        attempt_count: 0,
        classifier_build: "a".repeat(64),
        classifier_profile_hash: "b".repeat(64),
    };
    assert!(
        impossible_snapshot.validate().is_err(),
        "not_classified_by_budget is not_eligible/not_applicable, never snapshot"
    );

    let impossible_fresh_parse = serde_json::json!({
        "relative_path": "evidence.pdf",
        "file_identity": "fixture:evidence.pdf",
        "source_version": "mtime_ns=1:size=1",
        "source_guard_kind": "content_sha256_v1",
        "source_guard_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "parse_status": "success",
        "parser_backend": "pdf_text_v1",
        "worker_lane": "python_document_process",
        "parse_cache_status": "fresh",
        "cache_miss_reason": "",
        "truncated": false,
        "content_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "parse_duration_ms": 0,
        "failure_class": "",
        "fallback_backend": "",
        "fallback_reason_code": "",
        "parse_transport": "one_shot",
        "parse_attempt_count": 1,
        "final_diagnostic": null,
        "pdf_classification": null
    });
    let parsed: ai_daily_scanner_contract::FileAuditV2 =
        serde_json::from_value(impossible_fresh_parse).expect("fixture decodes");
    assert!(
        parsed.validate().is_err(),
        "an exact parse-cache hit performs no parser transport or attempt"
    );
}

#[test]
fn v2_audit_page_miss_reason_and_backend_lane_matrices_are_strict() {
    use ai_daily_scanner_contract::{
        ClassificationCacheStatus, ClassificationTransport, Nullable, PdfClassificationAuditV1,
        PdfClassificationStatus,
    };

    let valid_classification = PdfClassificationAuditV1 {
        status: PdfClassificationStatus::TextInParseWindow,
        page_count: Nullable(Some(2)),
        classification_cache_status: ClassificationCacheStatus::Miss,
        classification_cache_miss_reason: "new_file".to_string(),
        result_examined_pages: Nullable(Some(1)),
        run_inspected_pages: Nullable(Some(1)),
        nominal_charged_pages: 2,
        duration_ms: 1,
        transport: ClassificationTransport::OneShot,
        attempt_count: 1,
        classifier_build: "a".repeat(64),
        classifier_profile_hash: "b".repeat(64),
    };
    valid_classification
        .validate()
        .expect("baseline classification audit validates");

    let mut snapshot_classification = valid_classification.clone();
    snapshot_classification.classification_cache_status = ClassificationCacheStatus::Snapshot;
    snapshot_classification
        .classification_cache_miss_reason
        .clear();
    snapshot_classification.run_inspected_pages = Nullable(Some(0));
    snapshot_classification.duration_ms = 0;
    snapshot_classification.transport = ClassificationTransport::Snapshot;
    snapshot_classification.attempt_count = 0;
    snapshot_classification
        .validate()
        .expect("snapshot keeps result pages but has zero current-run pages");

    let mut text_beyond_window = valid_classification.clone();
    text_beyond_window.result_examined_pages = Nullable(Some(3));
    assert!(text_beyond_window.validate().is_err());

    let mut no_text_short_window = valid_classification.clone();
    no_text_short_window.status = PdfClassificationStatus::NoTextInParseWindow;
    assert!(no_text_short_window.validate().is_err());

    let mut excessive_run_pages = valid_classification.clone();
    excessive_run_pages.attempt_count = 2;
    excessive_run_pages.run_inspected_pages = Nullable(Some(5));
    assert!(excessive_run_pages.validate().is_err());

    let mut legacy_classification_reason = valid_classification.clone();
    legacy_classification_reason.classification_cache_miss_reason = "error_cache".to_string();
    assert!(legacy_classification_reason.validate().is_err());

    let base = serde_json::json!({
        "relative_path": "evidence.pdf",
        "file_identity": "fixture:evidence.pdf",
        "source_version": "mtime_ns=1:size=1",
        "source_guard_kind": "content_sha256_v1",
        "source_guard_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "parse_status": "error",
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
        "parse_attempt_count": 1,
        "final_diagnostic": {
            "error_code": "PARSER_FAILED",
            "message": "synthetic failure",
            "retryable": false,
            "stage": "parse",
            "file_path": null,
            "backend": null
        },
        "pdf_classification": null
    });
    let parsed: ai_daily_scanner_contract::FileAuditV2 =
        serde_json::from_value(base.clone()).expect("baseline file audit decodes");
    parsed.validate().expect("baseline file audit validates");

    let mut legacy_reason = base.as_object().unwrap().clone();
    legacy_reason.insert(
        "cache_miss_reason".to_string(),
        serde_json::Value::String("error_cache".to_string()),
    );
    let parsed: ai_daily_scanner_contract::FileAuditV2 =
        serde_json::from_value(serde_json::Value::Object(legacy_reason))
            .expect("legacy reason fixture decodes");
    assert!(parsed.validate().is_err());

    let mut miss_not_parsed = base.as_object().unwrap().clone();
    miss_not_parsed.insert(
        "parser_backend".to_string(),
        serde_json::Value::String("not_parsed".to_string()),
    );
    miss_not_parsed.insert(
        "worker_lane".to_string(),
        serde_json::Value::String("not_parsed".to_string()),
    );
    let parsed: ai_daily_scanner_contract::FileAuditV2 =
        serde_json::from_value(serde_json::Value::Object(miss_not_parsed))
            .expect("miss/not-parsed fixture decodes");
    assert!(parsed.validate().is_err());

    let metadata = serde_json::json!({
        "relative_path": "no-text.pdf",
        "file_identity": "fixture:no-text.pdf",
        "source_version": "mtime_ns=1:size=1",
        "source_guard_kind": "content_sha256_v1",
        "source_guard_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "parse_status": "success",
        "parser_backend": "pdf_metadata_v1",
        "worker_lane": "rust_core",
        "parse_cache_status": "not_applicable",
        "cache_miss_reason": "",
        "truncated": false,
        "content_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "parse_duration_ms": 0,
        "failure_class": "",
        "fallback_backend": "",
        "fallback_reason_code": "",
        "parse_transport": "not_applicable",
        "parse_attempt_count": 0,
        "final_diagnostic": null,
        "pdf_classification": {
            "status": "no_text_in_parse_window",
            "page_count": 2,
            "classification_cache_status": "miss",
            "classification_cache_miss_reason": "new_file",
            "result_examined_pages": 2,
            "run_inspected_pages": 2,
            "nominal_charged_pages": 2,
            "duration_ms": 1,
            "transport": "one_shot",
            "attempt_count": 1,
            "classifier_build": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "classifier_profile_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        }
    });
    let parsed: ai_daily_scanner_contract::FileAuditV2 =
        serde_json::from_value(metadata.clone()).expect("metadata fixture decodes");
    parsed
        .validate()
        .expect("no-text metadata provenance validates");

    let mut no_text_body = metadata.as_object().unwrap().clone();
    no_text_body.insert(
        "parser_backend".to_string(),
        serde_json::Value::String("pdf_text_v1".to_string()),
    );
    no_text_body.insert(
        "worker_lane".to_string(),
        serde_json::Value::String("python_document_process".to_string()),
    );
    let parsed: ai_daily_scanner_contract::FileAuditV2 =
        serde_json::from_value(serde_json::Value::Object(no_text_body))
            .expect("no-text body-parser fixture decodes");
    assert!(parsed.validate().is_err());
}
