use ai_daily_scanner_contract::{ReportMode, ScannerSettings};
use ai_daily_scanner_core::config::normalize_scanner_settings;
use serde_json::{json, Value};

fn normalized(config: Value) -> Vec<u8> {
    let config: ScannerSettings = serde_json::from_value(config).unwrap();
    serde_json::to_vec(&normalize_scanner_settings(&config, ReportMode::Daily).unwrap()).unwrap()
}

#[test]
fn a_disabled_ocr_section_preserves_the_existing_normalized_bytes() {
    let old: ScannerSettings = serde_json::from_str("{}").unwrap();
    assert_eq!(serde_json::to_string(&old).unwrap(), "{}");
    let historical = normalized(json!({}));
    let disabled =
        normalized(json!({"pdf_image_ocr":{"enabled":false,"max_files":1,"max_pages":2}}));
    assert_eq!(historical, disabled);
}

#[test]
fn enabling_image_ocr_and_changing_its_global_limits_changes_the_normalized_identity() {
    let enabled = |files, pages| {
        normalized(json!({"pdf_image_ocr":{"enabled":true,"max_files":files,"max_pages":pages}}))
    };
    let profile = enabled(1, 2);
    assert_ne!(profile, normalized(json!({})));
    assert_ne!(profile, enabled(2, 2));
    assert_ne!(profile, enabled(1, 1));
}

#[test]
fn invalid_ocr_quotas_and_unknown_fields_are_rejected() {
    for raw in [
        json!({"enabled":true,"max_files":0}),
        json!({"max_files":33}),
        json!({"max_pages":0}),
        json!({"max_pages":97}),
        json!({"pages":2}),
    ] {
        let settings = serde_json::from_value::<ScannerSettings>(json!({"pdf_image_ocr":raw}));
        assert!(
            settings.map_or(true, |s| normalize_scanner_settings(&s, ReportMode::Daily)
                .is_err())
        );
    }
}
