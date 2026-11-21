use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use ai_daily_scanner_contract::SourceGuardKind;
use ai_daily_scanner_contract::{ErrorCode, ReportMode, ScannerSettings};
use ai_daily_scanner_core::admission::{ClassificationPlan, PlanAction, PlanCandidate};
use ai_daily_scanner_core::classifier::ParserRoute;
use ai_daily_scanner_core::config::normalize_scanner_settings;
use ai_daily_scanner_core::fallback::FailureClass;
use ai_daily_scanner_core::parsers::{register_worker, register_worker_pair, WorkerCommand};
use ai_daily_worker_contract::WorkerKind;

mod support;
use support::process_fixture_path;

const OFFICE_MARKER: &str = "office.started";
const PDF_MARKER: &str = "pdf.started";

#[test]
fn office_and_pdf_worker_handshakes_overlap() {
    let directory = tempfile::tempdir().expect("temporary worker root should exist");
    let office = rendezvous_worker(WorkerKind::Office, directory.path());
    let pdf = rendezvous_worker(WorkerKind::Pdf, directory.path());

    let (office_result, pdf_result) = register_worker_pair(&office, &pdf, Duration::from_secs(5));

    office_result.expect("Office handshake should rendezvous");
    pdf_result.expect("PDF handshake should rendezvous");
    assert_eq!(
        fs::read_to_string(directory.path().join(OFFICE_MARKER)).unwrap(),
        "office\n"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join(PDF_MARKER)).unwrap(),
        "pdf\n"
    );
}

#[test]
fn worker_handshake_fails_after_waiting_for_a_missing_peer() {
    let directory = tempfile::tempdir().expect("temporary worker root should exist");
    let office = rendezvous_worker(WorkerKind::Office, directory.path());

    let started = Instant::now();
    let failure = register_worker(&office, Duration::from_secs(5))
        .expect_err("Office must not handshake without its peer");

    assert_eq!(failure.class, FailureClass::ContractFailure);
    assert_eq!(
        failure.diagnostic.error_code,
        ErrorCode::WorkerHandshakeFailed
    );
    assert_eq!(
        fs::read_to_string(directory.path().join(OFFICE_MARKER)).unwrap(),
        "office\n"
    );
    assert!(started.elapsed() >= Duration::from_secs(2));
}

#[test]
fn explicitly_enabled_legacy_office_routes_are_planned_for_office_worker() {
    let profile = legacy_office_profile();
    for (extension, expected) in [
        (".xls", ParserRoute::RustXls),
        (".doc", ParserRoute::RustLegacyOffice),
        (".ppt", ParserRoute::RustLegacyOffice),
    ] {
        let name = format!("legacy_sample{extension}");
        let file = PlanCandidate {
            file_identity: name.clone(),
            relative_path: name,
            extension: extension.into(),
            size_bytes: 10,
            source_guard_kind: SourceGuardKind::ContentSha256V1,
        };
        let plans = ClassificationPlan::build(vec![file], &profile, 0);
        assert_eq!(plans[0].action, PlanAction::Admit { route: expected });
    }
}

#[test]
fn missing_office_and_pdf_workers_are_environment_unavailable() {
    let directory = tempfile::tempdir().expect("temporary root should exist");
    for (kind, backend, extension) in [
        (WorkerKind::Office, "rust_office_oxide_v2", ".docx"),
        (WorkerKind::Pdf, "rust_pdfium_v3", ".pdf"),
    ] {
        let command = WorkerCommand {
            program: directory.path().join("missing-worker.exe"),
            base_args: Vec::new(),
            current_dir: None,
            expected_kind: kind,
            required_backends: vec![backend.to_string()],
            required_extensions: vec![extension.to_string()],
        };

        let failure = register_worker(&command, Duration::from_secs(1))
            .expect_err("missing worker must fail preflight");

        assert_eq!(failure.class, FailureClass::EnvironmentUnavailable);
        assert_eq!(
            failure.diagnostic.error_code,
            ErrorCode::WorkerHandshakeFailed
        );
    }
}

fn capabilities(kind: WorkerKind) -> (Vec<String>, Vec<String>) {
    match kind {
        WorkerKind::Office => (
            vec![
                "rust_office_oxide_v2".to_string(),
                "rust_xlsx_bounded_v2".to_string(),
            ],
            vec![
                ".docx".to_string(),
                ".pptx".to_string(),
                ".xlsx".to_string(),
            ],
        ),
        WorkerKind::Pdf => (
            vec![
                "rust_pdfium_v3".to_string(),
                "rust_xls_calamine_v3".to_string(),
                "rust_legacy_office_v3".to_string(),
            ],
            vec![
                ".doc".to_string(),
                ".docx".to_string(),
                ".pdf".to_string(),
                ".ppt".to_string(),
                ".pptx".to_string(),
                ".xls".to_string(),
                ".xlsx".to_string(),
            ],
        ),
    }
}

fn legacy_office_profile() -> ai_daily_scanner_contract::NormalizedScannerSettings {
    let raw: ScannerSettings = serde_json::from_value(serde_json::json!({

        "allowed_extensions": [".xls", ".doc", ".ppt"],
        "legacy_office_enabled": true,
        "max_workers": 3,
        "file_timeout_seconds": 30
    }))
    .expect("legacy profile should decode");
    normalize_scanner_settings(&raw, ReportMode::Daily).expect("legacy profile should normalize")
}

fn rendezvous_worker(kind: WorkerKind, directory: &Path) -> WorkerCommand {
    let (required_backends, required_extensions) = capabilities(kind);
    let (role, marker, peer) = match kind {
        WorkerKind::Office => ("office", OFFICE_MARKER, PDF_MARKER),
        WorkerKind::Pdf => ("pdf", PDF_MARKER, OFFICE_MARKER),
    };
    WorkerCommand {
        program: process_fixture_path(),
        base_args: vec!["rendezvous".into(), role.into(), marker.into(), peer.into()],
        current_dir: Some(directory.to_path_buf()),
        expected_kind: kind,
        required_backends,
        required_extensions,
    }
}
