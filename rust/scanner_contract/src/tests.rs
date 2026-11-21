use super::*;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct FixtureManifest {
    valid_fixtures: Vec<FixtureEntry>,
}

#[derive(Debug, Deserialize)]
struct FixtureEntry {
    file: String,
    schema: String,
}

#[derive(Debug, Deserialize)]
struct InvalidCorpus {
    cases: Vec<InvalidCase>,
}

#[derive(Debug, Deserialize)]
struct InvalidCase {
    name: String,
    schema: String,
    payload: Value,
    #[serde(default)]
    related_payloads: Vec<RelatedPayload>,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read_json<T: DeserializeOwned>(path: &Path) -> T {
    let text = fs::read_to_string(path).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn every_golden_fixture_round_trips_through_typed_dtos() {
    let fixture_dir = fixture_dir();
    let manifest: FixtureManifest = read_json(&fixture_dir.join("fixture-manifest.json"));
    for entry in manifest.valid_fixtures {
        let value: Value = read_json(&fixture_dir.join(&entry.file));
        let round_trip = validate_contract_payload(&entry.schema, &value, &[])
            .unwrap_or_else(|error| panic!("{}: {error}", entry.file));
        assert_eq!(round_trip, value, "{}", entry.file);
    }
}

#[test]
fn every_invalid_fixture_is_rejected() {
    let fixture_dir = fixture_dir();
    let corpus: InvalidCorpus = read_json(&fixture_dir.join("invalid-cases.json"));
    for case in corpus.cases {
        let result = validate_contract_payload(&case.schema, &case.payload, &case.related_payloads);
        assert!(result.is_err(), "{} unexpectedly passed", case.name);
    }
}

#[test]
fn cached_pdf_open_errors_require_native_proof_and_zero_real_work() {
    let mut audit = PdfClassificationAuditV1 {
        pdf_open_failure_key: None,
        page_audit: None,
        status: PdfClassificationStatus::Error,
        page_count: Nullable(None),
        classification_cache_status: ClassificationCacheStatus::Miss,
        classification_cache_miss_reason: "new_file".into(),
        result_examined_pages: Nullable(None),
        run_inspected_pages: Nullable(None),
        nominal_charged_pages: 5,
        duration_ms: 1,
        transport: ClassificationTransport::Session,
        attempt_count: 1,
        classifier_build: "a".repeat(64),
        classifier_profile_hash: "b".repeat(64),
    };
    audit.validate().unwrap();
    let historical = serde_json::to_value(&audit).unwrap();
    assert!(historical.get("pdf_open_failure_key").is_none());
    assert_eq!(
        serde_json::from_value::<PdfClassificationAuditV1>(historical).unwrap(),
        audit
    );
    audit.classification_cache_status = ClassificationCacheStatus::Fresh;
    audit.classification_cache_miss_reason.clear();
    audit.run_inspected_pages = Nullable(Some(0));
    audit.attempt_count = 0;
    audit.duration_ms = 0;
    audit.transport = ClassificationTransport::NotApplicable;
    assert!(audit.validate().is_err());
    audit.pdf_open_failure_key = Some("c".repeat(64));
    audit.validate().unwrap();
    audit.attempt_count = 1;
    assert!(audit.validate().is_err());
    audit.attempt_count = 0;
    audit.status = PdfClassificationStatus::Unknown;
    assert!(audit.validate().is_err());
}

#[test]
fn image_only_worker_queue_failure_preserves_zero_attempts_and_page_proof() {
    let mut audit = PdfClassificationAuditV1 {
        pdf_open_failure_key: None,
        page_audit: Some(PdfPageAudit {
            policy_version: "pdf_text_presence_v2".into(),
            checked_pages: vec![1],
            observations: Some(vec![PdfPageObservation {
                page: 1,
                has_text: false,
                has_image: Some(true),
            }]),
        }),
        status: PdfClassificationStatus::NoTextInParseWindow,
        page_count: Nullable(Some(1)),
        classification_cache_status: ClassificationCacheStatus::Fresh,
        classification_cache_miss_reason: String::new(),
        result_examined_pages: Nullable(Some(1)),
        run_inspected_pages: Nullable(Some(0)),
        nominal_charged_pages: 1,
        duration_ms: 0,
        transport: ClassificationTransport::NotApplicable,
        attempt_count: 0,
        classifier_build: "a".repeat(64),
        classifier_profile_hash: "b".repeat(64),
    };
    audit.validate().unwrap();
    let check = |status, attempts, audit: &PdfClassificationAuditV1| {
        validate_file_parse_provenance(
            status,
            "not_parsed",
            AuditWorkerLane::NotParsed,
            ParseCacheStatus::NotApplicable,
            ParseTransport::NotApplicable,
            attempts,
            Some(audit),
        )
    };
    for status in [ParseStatus::Timeout, ParseStatus::Error] {
        check(status, 0, &audit).unwrap();
        assert!(check(status, 1, &audit).is_err());
    }
    audit.page_audit = None;
    assert!(check(ParseStatus::Timeout, 0, &audit).is_err());
}

#[test]
fn ocr_provenance_requires_a_no_text_classification_and_pdf_process() {
    use AuditWorkerLane::{RustCore, RustPdfProcessV3};
    use ParseCacheStatus::{Fresh, Miss, NotApplicable, Snapshot};
    use ParseStatus::{Error, NotParsed, Success, Timeout};
    use PdfClassificationStatus::{NoTextInParseWindow, TextInParseWindow};
    let check = |status, lane, cache, classification| {
        validate_v2_parse_provenance(status, OCR_BACKEND, lane, cache, classification).is_ok()
    };
    let image = Some(NoTextInParseWindow);
    for cache in [Fresh, Miss, Snapshot] {
        assert!(check(Success, RustPdfProcessV3, cache, image));
    }
    for status in [Error, Timeout] {
        assert!(check(status, RustPdfProcessV3, Miss, image));
        assert!(!check(status, RustPdfProcessV3, Fresh, image));
    }
    assert!(!check(Success, RustCore, Miss, image));
    assert!(!check(NotParsed, RustPdfProcessV3, Miss, image));
    assert!(!check(Success, RustPdfProcessV3, NotApplicable, image));
    for classification in [
        None,
        Some(TextInParseWindow),
        Some(PdfClassificationStatus::Unknown),
    ] {
        assert!(!check(Success, RustPdfProcessV3, Miss, classification));
    }
}

#[test]
fn absolute_paths_reject_embedded_nul() {
    assert!(require_absolute_path("C:\\evidence\0hidden.xlsx", "file_path").is_err());
}

#[test]
fn file_audit_v2_fail_closes_non_miss_row_with_non_empty_miss_reason() {
    // spec Part 5.3: cache_miss_reason is only valid for parse_cache_status=miss.
    // A not_applicable row with a non-empty reason (real-corpus RUN_CORRUPT
    // regression) must fail validation.
    fn audit(cache_miss_reason: &str) -> FileAuditV2 {
        FileAuditV2 {
            relative_path: "evidence.txt".to_string(),
            file_identity: "file-a".to_string(),
            source_version: "mtime_ns=100:size=5".to_string(),
            source_guard_kind: SourceGuardKind::Unavailable,
            source_guard_sha256: Nullable(None),
            parse_status: ParseStatus::NotParsed,
            parser_backend: "not_parsed".to_string(),
            worker_lane: AuditWorkerLane::NotParsed,
            parse_cache_status: ParseCacheStatus::NotApplicable,
            cache_miss_reason: cache_miss_reason.to_string(),
            truncated: false,
            content_sha256: "0".repeat(64),
            parse_duration_ms: 0,
            failure_class: String::new(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ParseTransport::NotApplicable,
            parse_attempt_count: 0,
            final_diagnostic: Nullable(None),
            pdf_classification: Nullable(None),
        }
    }
    assert!(
        audit("new_file").validate().is_err(),
        "not_applicable row with a non-empty miss reason must fail validation"
    );
    assert!(
        audit("").validate().is_ok(),
        "not_applicable row with an empty miss reason must validate"
    );
    // The same holds for fresh/snapshot rows.
    for status in [ParseCacheStatus::Fresh, ParseCacheStatus::Snapshot] {
        let mut row = audit("");
        row.parse_cache_status = status;
        row.cache_miss_reason = "new_file".to_string();
        assert!(row.validate().is_err(), "{status:?} with reason must fail");
    }
}
