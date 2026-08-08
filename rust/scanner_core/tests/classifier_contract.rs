//! PDF 分类器 wire 与本地 port（spec Part 7.1 / Part 3.2）的测试。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use ai_daily_scanner_contract::{
    ClassifierResponseStatus, ClassifierVersionResponseV1, PdfClassifierRequestV1,
    PdfClassifierResponseV1, PdfClassifierResultStatus, PdfClassifierResultV1,
    PythonOperationDiagnosticV1, PythonOperationErrorCode, PythonOperationStage, Validate,
};
use ai_daily_scanner_core::config::normalize_scanner_profile_v2;
use ai_daily_scanner_core::parsers::classifier::{classify_pdf_oneshot, PdfClassifierPort};
use ai_daily_scanner_core::parsers::WorkerCommand;
use ai_daily_scanner_core::store::classifier_profile_hash;
use ai_daily_scanner_contract::{RawScannerProfileV2, ReportMode, ScannerProfile, WorkerKind};

fn v2_profile(mode: ReportMode) -> ai_daily_scanner_contract::NormalizedScannerProfileV2 {
    let raw: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
        "schema_version": "scanner_profile_v2"
    }))
    .expect("minimal v2 raw profile");
    normalize_scanner_profile_v2(&ScannerProfile::V2(raw), mode).expect("normalized v2 profile")
}

fn request_id() -> String {
    "61111111-6111-4111-8111-611111111111".to_string()
}

fn text_result() -> PdfClassifierResultV1 {
    PdfClassifierResultV1 {
        status: PdfClassifierResultStatus::TextInParseWindow,
        page_count: ai_daily_scanner_contract::Nullable(Some(1)),
        result_examined_pages: ai_daily_scanner_contract::Nullable(Some(1)),
        diagnostic: ai_daily_scanner_contract::Nullable(None),
    }
}

fn request(file_path: &str, source_version: &str) -> PdfClassifierRequestV1 {
    PdfClassifierRequestV1 {
        contract: "ai_daily_pdf_classifier".to_string(),
        protocol_version: 1,
        request_id: request_id(),
        file_path: file_path.to_string(),
        source_version: source_version.to_string(),
        max_pages: 5,
        policy_version: "pdf_text_presence_v1".to_string(),
    }
}

#[test]
fn classifier_wire_types_round_trip_and_validate() {
    let response = PdfClassifierResponseV1 {
        contract: "ai_daily_pdf_classifier".to_string(),
        protocol_version: 1,
        request_id: request_id(),
        status: ClassifierResponseStatus::Ok,
        result: ai_daily_scanner_contract::Nullable(Some(text_result())),
        error: ai_daily_scanner_contract::Nullable(None),
    };
    response.validate().expect("ok response validates");
    let json = serde_json::to_string(&response).expect("serialize");
    let parsed: PdfClassifierResponseV1 =
        serde_json::from_str(&json).expect("deserialize");
    parsed.validate().expect("round-trip validates");
    assert_eq!(parsed, response);
}

#[test]
fn classifier_wire_rejects_bad_status_invariants() {
    // text result must not carry a diagnostic
    let bad_result = PdfClassifierResultV1 {
        status: PdfClassifierResultStatus::TextInParseWindow,
        page_count: ai_daily_scanner_contract::Nullable(Some(1)),
        result_examined_pages: ai_daily_scanner_contract::Nullable(Some(1)),
        diagnostic: ai_daily_scanner_contract::Nullable(Some(PythonOperationDiagnosticV1 {
            error_code: PythonOperationErrorCode::ParserFailed,
            message: "unexpected diagnostic".to_string(),
            retryable: false,
            stage: PythonOperationStage::Parse,
            file_path: ai_daily_scanner_contract::Nullable(None),
            backend: ai_daily_scanner_contract::Nullable(None),
        })),
    };
    assert!(bad_result.validate().is_err());

    // unknown must carry a retryable diagnostic
    let bad_unknown = PdfClassifierResultV1 {
        status: PdfClassifierResultStatus::Unknown,
        page_count: ai_daily_scanner_contract::Nullable(None),
        result_examined_pages: ai_daily_scanner_contract::Nullable(None),
        diagnostic: ai_daily_scanner_contract::Nullable(None),
    };
    assert!(bad_unknown.validate().is_err());

    // error must not be retryable
    let bad_error = PdfClassifierResultV1 {
        status: PdfClassifierResultStatus::Error,
        page_count: ai_daily_scanner_contract::Nullable(None),
        result_examined_pages: ai_daily_scanner_contract::Nullable(None),
        diagnostic: ai_daily_scanner_contract::Nullable(Some(PythonOperationDiagnosticV1 {
            error_code: PythonOperationErrorCode::ParserFailed,
            message: "deterministic error".to_string(),
            retryable: true,
            stage: PythonOperationStage::Parse,
            file_path: ai_daily_scanner_contract::Nullable(None),
            backend: ai_daily_scanner_contract::Nullable(None),
        })),
    };
    assert!(bad_error.validate().is_err());

    // outer error response must not carry a result
    let bad_response = PdfClassifierResponseV1 {
        contract: "ai_daily_pdf_classifier".to_string(),
        protocol_version: 1,
        request_id: request_id(),
        status: ClassifierResponseStatus::Error,
        result: ai_daily_scanner_contract::Nullable(Some(text_result())),
        error: ai_daily_scanner_contract::Nullable(None),
    };
    assert!(bad_response.validate().is_err());
}

#[test]
fn classifier_version_response_round_trips() {
    let version = ClassifierVersionResponseV1 {
        contract: "ai_daily_pdf_classifier".to_string(),
        protocol_version: 1,
        classifier_contract_version: "ai_daily_pdf_classifier_v1".to_string(),
        classifier_build: "a".repeat(64),
        policy_version: "pdf_text_presence_v1".to_string(),
        python_implementation: "cpython".to_string(),
        python_version: "3.11.15".to_string(),
        unicode_data_version: "14.0.0".to_string(),
        pypdfium2_version: "5.12.1".to_string(),
        pdfium_version: "152.0.7947.0".to_string(),
        target_triple: "amd64-pc-windows-msvc".to_string(),
    };
    version.validate().expect("classifier version validates");
    let json = serde_json::to_string(&version).expect("serialize");
    let parsed: ClassifierVersionResponseV1 = serde_json::from_str(&json).expect("deserialize");
    parsed.validate().expect("round-trip validates");
    assert_eq!(parsed, version);

    let mut bad = version;
    bad.classifier_build = "not-a-sha".to_string();
    assert!(bad.validate().is_err());
}

#[derive(Clone)]
struct StubClassifier {
    result: Option<PdfClassifierResultV1>,
}

impl PdfClassifierPort for StubClassifier {
    fn classify_pdf(
        &self,
        _request: &PdfClassifierRequestV1,
        _timeout: Duration,
    ) -> Result<PdfClassifierResultV1, ai_daily_scanner_core::fallback::ParseFailure> {
        self.result.clone().ok_or_else(|| {
            ai_daily_scanner_core::fallback::ParseFailure {
                class: ai_daily_scanner_core::fallback::FailureClass::Deterministic,
                diagnostic: ai_daily_scanner_contract::Diagnostic {
                    error_code: ai_daily_scanner_contract::ErrorCode::ParserFailed,
                    message: "stub failure".to_string(),
                    retryable: false,
                    stage: ai_daily_scanner_contract::DiagnosticStage::Parse,
                    file_path: ai_daily_scanner_contract::Nullable(None),
                    backend: ai_daily_scanner_contract::Nullable(None),
                },
            }
        })
    }
}

#[test]
fn stub_classifier_port_returns_in_memory_results() {
    let stub = StubClassifier {
        result: Some(text_result()),
    };
    let outcome = stub
        .classify_pdf(
            &request(r"D:\fixture.pdf", "mtime_ns=1:size=2"),
            Duration::from_secs(1),
        )
        .expect("stub returns the programmed result");
    assert_eq!(outcome.status, PdfClassifierResultStatus::TextInParseWindow);
}

#[test]
fn classifier_profile_hash_is_canonical_and_input_sensitive() {
    let profile = v2_profile(ReportMode::Daily);
    let base = classifier_profile_hash(&profile).expect("profile hash");

    let mut changed_timeout = profile.clone();
    changed_timeout.pdf_classification_timeout_ms += 1_000;
    assert_ne!(
        base,
        classifier_profile_hash(&changed_timeout).expect("hash"),
        "timeout must enter the classification profile hash"
    );

    let mut changed_pages = profile.clone();
    changed_pages.parse.pdf.max_pages += 1;
    assert_ne!(
        base,
        classifier_profile_hash(&changed_pages).expect("hash"),
        "pdf_max_pages must enter the classification profile hash"
    );

    assert_eq!(
        base,
        classifier_profile_hash(&profile).expect("hash"),
        "same profile must produce the same hash"
    );
}

fn python_executable() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let candidates = if cfg!(windows) {
        vec![root.join(".venv").join("Scripts").join("python.exe")]
    } else {
        vec![root.join(".venv").join("bin").join("python")]
    };
    candidates.into_iter().find(|path| path.is_file())
}

#[test]
fn classifier_oneshot_process_classifies_text_pdf() {
    let Some(python) = python_executable() else {
        return;
    };
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let fixture = repository_root
        .join("tests")
        .join("fixtures")
        .join("pdf_benchmark")
        .join("case_01.pdf");
    if !fixture.is_file() {
        return;
    }
    let command = WorkerCommand {
        program: python,
        base_args: vec![
            OsString::from("-m"),
            OsString::from("src.workers.document_parser_worker"),
        ],
        current_dir: Some(repository_root),
        expected_kind: WorkerKind::PythonDocument,
        required_backends: vec!["pdf_text_v1".to_string()],
        required_extensions: vec![".pdf".to_string()],
    };
    let metadata = std::fs::metadata(&fixture).expect("fixture metadata");
    let modified = metadata
        .modified()
        .expect("fixture modified time")
        .duration_since(UNIX_EPOCH)
        .expect("fixture mtime after epoch");
    let source_version =
        ai_daily_discovery::build_source_version(modified.as_nanos(), metadata.len());
    let request = request(&fixture.to_string_lossy(), &source_version);

    let result = classify_pdf_oneshot(&command, &request, Duration::from_secs(20))
        .expect("classifier one-shot completes");

    assert_eq!(result.status, PdfClassifierResultStatus::TextInParseWindow);
    assert_eq!(result.page_count.0, Some(1));
    assert_eq!(result.result_examined_pages.0, Some(1));
}

#[test]
fn classifier_oneshot_reports_deterministic_error_for_corrupt_pdf() {
    let Some(python) = python_executable() else {
        return;
    };
    let directory = tempfile::tempdir().expect("temp dir");
    let corrupt = directory.path().join("corrupt.pdf");
    std::fs::write(&corrupt, b"%PDF-1.4 not a real pdf\n").expect("write corrupt pdf");
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let command = WorkerCommand {
        program: python,
        base_args: vec![
            OsString::from("-m"),
            OsString::from("src.workers.document_parser_worker"),
        ],
        current_dir: Some(repository_root),
        expected_kind: WorkerKind::PythonDocument,
        required_backends: vec!["pdf_text_v1".to_string()],
        required_extensions: vec![".pdf".to_string()],
    };
    let metadata = std::fs::metadata(&corrupt).expect("fixture metadata");
    let modified = metadata
        .modified()
        .expect("fixture modified time")
        .duration_since(UNIX_EPOCH)
        .expect("fixture mtime after epoch");
    let source_version =
        ai_daily_discovery::build_source_version(modified.as_nanos(), metadata.len());
    let request = request(&corrupt.to_string_lossy(), &source_version);

    let result = classify_pdf_oneshot(&command, &request, Duration::from_secs(20));
    if let Err(ref error) = result {
        eprintln!("DEBUG classifier error: {error:?}");
        // Reproduce the raw process output for diagnosis.
        let stdin = serde_json::to_vec(&request).expect("request serializes");
        let spec = ai_daily_scanner_core::process::ProcessSpec {
            program: command.program.clone(),
            args: vec![
                OsString::from("-m"),
                OsString::from("src.workers.document_parser_worker"),
                OsString::from("classify-pdf"),
            ],
            current_dir: command.current_dir.clone(),
            stdin,
            timeout: Duration::from_secs(20),
            capture_limit: 1024 * 1024,
        };
        if let Ok(output) = ai_daily_scanner_core::process::run_process(&spec) {
            eprintln!(
                "DEBUG exit={} stdout={:?} stderr={:?}",
                output.exit_code,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    let result = result.expect("classifier one-shot completes");
    assert_eq!(result.status, PdfClassifierResultStatus::Error);
    let diagnostic = result.diagnostic.0.as_ref().expect("error diagnostic");
    assert!(!diagnostic.retryable);
}
