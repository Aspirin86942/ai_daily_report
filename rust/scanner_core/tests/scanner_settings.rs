use ai_daily_scanner_contract::{BuildContextRequest, ReportMode, ScannerSettings, Validate};
use ai_daily_scanner_core::config::normalize_scanner_settings;

#[test]
fn empty_settings_receive_frozen_current_defaults() {
    let raw: ScannerSettings = serde_json::from_value(serde_json::json!({})).unwrap();
    let normalized = normalize_scanner_settings(&raw, ReportMode::Daily).unwrap();

    assert_eq!(normalized.execution.max_workers, 4);
    assert_eq!(normalized.parse.text.backend, "light_text_v2");
    assert_eq!(
        normalized.parse.office.primary_backend,
        "rust_office_oxide_v2"
    );
    assert_eq!(normalized.parse.pdf.backend, "python_pdf_text_v2");
    assert_eq!(normalized.worker_max_requests, 128);
    assert_eq!(normalized.worker_idle_ttl_ms, 30_000);
    assert_eq!(normalized.worker_rss_limit_bytes, 512 * 1024 * 1024);
}

#[test]
fn removed_strategy_and_session_keys_are_rejected() {
    for key in [
        "schema_version",
        "parser_profile_version",
        "office_parser_backend",
        "pdf_parser_backend",
        "office_parser_fallback_enabled",
        "office_parser_fallback_order",
        "office_fallback_policy_version",
        "admission_policy_version",
        "classifier_policy_version",
        "session_concurrency",
        "max_requests_per_session",
        "session_idle_ttl_ms",
        "session_rss_limit_bytes",
    ] {
        let result = serde_json::from_value::<ScannerSettings>(serde_json::json!({key: 1}));
        assert!(result.is_err(), "removed key {key} must be rejected");
    }
}

#[test]
fn current_pool_and_policy_switches_flow_through_normalization() {
    let raw: ScannerSettings = serde_json::from_value(serde_json::json!({
        "max_workers": 6,
        "worker_max_requests": 7,
        "worker_idle_ttl_ms": 2_000,
        "worker_rss_limit_bytes": 67_108_864,
        "legacy_office_enabled": true,
        "fallback_after_timeout": true,
        "total_deadline_ms": 45_000
    }))
    .unwrap();
    let normalized = normalize_scanner_settings(&raw, ReportMode::Monthly).unwrap();

    assert_eq!(normalized.execution.max_workers, 6);
    assert_eq!(normalized.worker_max_requests, 7);
    assert!(normalized.parse.office.legacy_extensions_enabled);
    assert!(normalized.parse.office.fallback_after_timeout);
    assert_eq!(normalized.total_deadline_ms, 45_000);
}

#[test]
fn build_request_uses_the_single_settings_shape() {
    let request: BuildContextRequest = serde_json::from_value(serde_json::json!({
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "00000000-0000-4000-8000-000000000001",
        "work_dir": "C:/work",
        "start_date": "2026-01-01",
        "end_date": "2026-01-01",
        "report_mode": "daily",
        "compression_profile": null,
        "scan_db_path": "C:/state/scan_index_v3.sqlite3",
        "scanner_settings": {"max_workers": 2},
        "adapters": {
            "office_worker_path": "C:/bin/worker.exe",
            "python_executable": "C:/Python/python.exe",
            "python_module_root": "C:/app",
            "python_document_worker_module": "src.workers.document_parser_worker"
        }
    }))
    .unwrap();
    request.validate().unwrap();
    assert_eq!(request.scanner_settings.max_workers, Some(2));
}
