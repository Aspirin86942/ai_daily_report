use super::*;
use crate::contract::WorkerLane;
use serde_json::{json, Value};

#[test]
fn scanner_settings_preserve_raw_serialization() {
    let fixture = include_str!("../settings_fixtures/scanner-settings.json");
    let settings: ScannerSettings = serde_json::from_str(fixture).unwrap();
    settings.validate().unwrap();
    assert_eq!(
        serde_json::to_string_pretty(&settings).unwrap(),
        fixture.trim_end().replace("\r\n", "\n")
    );

    let empty: ScannerSettings = serde_json::from_str("{}").unwrap();
    empty.validate().unwrap();
    assert_eq!(serde_json::to_string(&empty).unwrap(), "{}");
}

#[test]
fn invalid_scanner_settings_are_rejected() {
    #[derive(Deserialize)]
    struct InvalidCases {
        cases: Vec<InvalidCase>,
    }

    #[derive(Deserialize)]
    struct InvalidCase {
        name: String,
        payload: Value,
    }

    let corpus: InvalidCases =
        serde_json::from_str(include_str!("../settings_fixtures/invalid-cases.json")).unwrap();
    for case in corpus.cases {
        let result = serde_json::from_value::<ScannerSettings>(case.payload)
            .map_err(|error| error.to_string())
            .and_then(|settings| settings.validate());
        assert!(result.is_err(), "{} unexpectedly passed", case.name);
    }
}

const DAILY_PROFILE: &str = include_str!("../settings_fixtures/normalized-settings-daily.json");

#[test]
fn retained_normalized_profiles_round_trip() {
    for fixture in [
        DAILY_PROFILE,
        include_str!("../settings_fixtures/normalized-settings-weekly.json"),
        include_str!("../settings_fixtures/normalized-settings-monthly.json"),
    ] {
        let expected: Value = serde_json::from_str(fixture).unwrap();
        let profile: NormalizedScannerSettings = serde_json::from_value(expected.clone()).unwrap();
        profile.discovery.validate().unwrap();
        profile.execution.validate().unwrap();
        profile.parse.text.validate().unwrap();
        profile.parse.office.validate().unwrap();
        assert_eq!(serde_json::to_value(profile).unwrap(), expected);
    }
}

#[test]
fn normalized_profile_rejects_unknown_execution_fields_and_unsorted_extensions() {
    let mut value: Value = serde_json::from_str(DAILY_PROFILE).unwrap();
    value["discovery"]["allowed_extensions"] = json!([".xlsx", ".csv"]);
    let profile: NormalizedScannerSettings = serde_json::from_value(value.clone()).unwrap();
    assert!(profile.discovery.validate().is_err());

    value["execution"]["deprecated_worker_mode"] = json!("legacy");
    assert!(serde_json::from_value::<NormalizedScannerSettings>(value).is_err());
}

#[test]
fn absolute_paths_reject_embedded_nul() {
    assert!(require_absolute_path("C:\\evidence\0hidden.xlsx", "file_path").is_err());
}

#[test]
fn pdf_page_audit_requires_matching_observations_and_complete_image_coverage() {
    let value = json!({
        "policy_version": "pdf_text_presence_v2",
        "checked_pages": [1, 2],
        "observations": [
            { "page": 1, "has_text": false, "has_image": true },
            { "page": 2, "has_text": false, "has_image": true }
        ]
    });
    let mut audit: PdfPageAudit = serde_json::from_value(value.clone()).unwrap();
    audit.validate().unwrap();
    assert_eq!(serde_json::to_value(&audit).unwrap(), value);
    assert!(audit.all_pages_are_images(2));
    assert!(!audit.all_pages_are_images(3));

    audit.observations.as_mut().unwrap()[1].has_image = None;
    assert!(!audit.all_pages_are_images(2));
    audit.checked_pages.swap(0, 1);
    assert!(audit.validate().is_err());
}

#[test]
fn cached_diagnostic_codes_and_refill_rounds_keep_legacy_json() {
    let mut value: Value =
        serde_json::from_str(include_str!("../settings_fixtures/diagnostic.json")).unwrap();
    let diagnostic: Diagnostic = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(diagnostic).unwrap(), value);
    value["stage"] = json!("worker");
    assert!(serde_json::from_value::<Diagnostic>(value).is_err());

    for code in [
        "CACHE_OPEN_FAILED",
        "REQUEST_ID_CONFLICT",
        "PARSE_CACHE_NOT_APPLICABLE_PROJECTED_AS_MISS",
    ] {
        let error_code: ErrorCode = serde_json::from_value(json!(code)).unwrap();
        assert_eq!(serde_json::to_value(error_code).unwrap(), code);
    }

    let value = json!({
        "round": 1,
        "planned_source_count": 2,
        "plan_sha256": "a".repeat(64),
        "candidate_slots_used": 2,
        "classification_pages_held": 3,
        "extraction_slots_used": 1,
        "reserved_chars": 5000,
        "refunded_chars": 600,
        "refunded_classification_pages": 1
    });
    let round: RefillRoundAudit = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(round).unwrap(), value);
}

#[test]
fn worker_lane_labels_match_the_wire_representation() {
    for lane in [
        WorkerLane::RustCore,
        WorkerLane::RustOfficeProcessV3,
        WorkerLane::RustOfficeInProcess,
        WorkerLane::RustPdfProcessV3,
    ] {
        assert_eq!(serde_json::to_value(lane).unwrap(), lane.as_str());
    }

    let guard: crate::contract::SourceGuardKind =
        serde_json::from_str("\"content_sha256_v1\"").unwrap();
    assert_eq!(guard, SourceGuardKind::ContentSha256V1);
}
