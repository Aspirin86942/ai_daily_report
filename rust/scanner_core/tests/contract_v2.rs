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
