//! PDF 分类器 wire 与本地 port（spec Part 7.1 / Part 3.2）的测试。

use ai_daily_scanner_core::session::{
    build_classify_request, session_classify, session_parse, PythonSession, SessionParams,
    SESSION_CONTRACT_VERSION,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use ai_daily_scanner_contract::{
    ClassificationTransport, ClassifierResponseStatus, ClassifierVersionResponseV1, PdfClassifierRequestV1,
    PdfClassifierResponseV1, PdfClassifierResultStatus, PdfClassifierResultV1,
    PythonOperationDiagnosticV1, PythonOperationErrorCode, PythonOperationStage,
    PythonSessionHelloV1, PythonSessionOperation, PythonSessionRequestV1,
    PythonSessionResponseStatus, PythonSessionResponseV1, PythonSessionResultV1,
    PythonSessionVersionResponseV1, Validate,
};
use ai_daily_scanner_core::config::normalize_scanner_profile_v2;
use ai_daily_scanner_core::parsers::classifier::{
    classify_pdf_oneshot, ClassifierPort, PdfClassifierExecution, PdfClassifierPort,
};
use ai_daily_scanner_core::parsers::{register_session_version, WorkerCommand};
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
fn classifier_result_is_bounded_by_the_request_window() {
    let mut text = text_result();
    text.page_count = ai_daily_scanner_contract::Nullable(Some(10));
    text.result_examined_pages = ai_daily_scanner_contract::Nullable(Some(6));
    text.validate()
        .expect("generic typed result is structurally valid");
    assert!(
        text.validate_for_max_pages(5).is_err(),
        "text result pages must not exceed request.max_pages"
    );

    let mut no_text = text_result();
    no_text.status = PdfClassifierResultStatus::NoTextInParseWindow;
    no_text.page_count = ai_daily_scanner_contract::Nullable(Some(10));
    no_text.result_examined_pages = ai_daily_scanner_contract::Nullable(Some(4));
    assert!(
        no_text.validate_for_max_pages(5).is_err(),
        "no-text must examine the complete request window"
    );

    let unknown = PdfClassifierResultV1 {
        status: PdfClassifierResultStatus::Unknown,
        page_count: ai_daily_scanner_contract::Nullable(None),
        result_examined_pages: ai_daily_scanner_contract::Nullable(Some(6)),
        diagnostic: ai_daily_scanner_contract::Nullable(Some(PythonOperationDiagnosticV1 {
            error_code: PythonOperationErrorCode::ParserFailed,
            message: "transient classifier failure".to_string(),
            retryable: true,
            stage: PythonOperationStage::Parse,
            file_path: ai_daily_scanner_contract::Nullable(None),
            backend: ai_daily_scanner_contract::Nullable(None),
        })),
    };
    assert!(
        unknown.validate_for_max_pages(5).is_err(),
        "typed failures must also stay within request.max_pages"
    );
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
    ) -> PdfClassifierExecution {
        PdfClassifierExecution::test_oneshot(self.result.clone().ok_or_else(|| {
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
        }))
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
        .outcome
        .expect("stub returns the programmed result");
    assert_eq!(outcome.status, PdfClassifierResultStatus::TextInParseWindow);
}

#[test]
fn classifier_oneshot_source_rejection_does_not_guess_a_started_attempt() {
    let directory = tempfile::tempdir().expect("temporary classifier root");
    let file = directory.path().join("changed.pdf");
    std::fs::write(&file, b"%PDF-1.4\n").expect("write classifier fixture");
    let command = WorkerCommand {
        program: directory.path().join("must-not-start.exe"),
        base_args: Vec::new(),
        current_dir: None,
        expected_kind: WorkerKind::PythonDocument,
        required_backends: vec!["pdf_text_v1".to_string()],
        required_extensions: vec![".pdf".to_string()],
    };
    let execution = ClassifierPort::new(command).classify_pdf(
        &request(&file.to_string_lossy(), "mtime_ns=1:size=1"),
        Duration::from_secs(1),
    );

    assert_eq!(execution.attempt_count, 0);
    assert_eq!(execution.duration_ms, 0);
    assert_eq!(execution.transport, ClassificationTransport::NotApplicable);
    assert_eq!(
        execution
            .outcome
            .expect_err("stale source must fail before spawn")
            .diagnostic
            .error_code,
        ai_daily_scanner_contract::ErrorCode::SourceVersionChanged
    );
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
            rss_tracker: None,
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

#[test]
fn session_wire_types_round_trip_and_validate() {
    let version = PythonSessionVersionResponseV1 {
        contract: "ai_daily_python_session".to_string(),
        protocol_version: 1,
        session_contract_version: "ai_daily_python_session_v1".to_string(),
        worker_build: "a".repeat(64),
        classifier_build: "b".repeat(64),
        supported_operations: vec!["classify_pdf_v1".to_string(), "parse_v1".to_string()],
    };
    version.validate().expect("session version validates");
    let json = serde_json::to_string(&version).expect("serialize");
    let parsed: PythonSessionVersionResponseV1 =
        serde_json::from_str(&json).expect("deserialize");
    parsed.validate().expect("round-trip validates");
    assert_eq!(parsed, version);

    let hello = PythonSessionHelloV1 {
        contract: "ai_daily_python_session".to_string(),
        protocol_version: 1,
        frame: "hello".to_string(),
        session_contract_version: "ai_daily_python_session_v1".to_string(),
        worker_build: "a".repeat(64),
        classifier_build: "b".repeat(64),
        supported_operations: vec!["classify_pdf_v1".to_string(), "parse_v1".to_string()],
    };
    hello.validate().expect("hello validates");
    let parsed_hello: PythonSessionHelloV1 =
        serde_json::from_str(&serde_json::to_string(&hello).expect("serialize"))
            .expect("deserialize");
    assert_eq!(parsed_hello, hello);

    let request = PythonSessionRequestV1 {
        contract: "ai_daily_python_session".to_string(),
        protocol_version: 1,
        request_id: request_id(),
        operation: PythonSessionOperation::ClassifyPdfV1,
        payload: serde_json::json!({"file_path": "C:\\x.pdf"}),
    };
    request.validate().expect("session request validates");

    let response = PythonSessionResponseV1 {
        contract: "ai_daily_python_session".to_string(),
        protocol_version: 1,
        request_id: request_id(),
        operation: PythonSessionOperation::ClassifyPdfV1,
        status: PythonSessionResponseStatus::Ok,
        result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Classify(
            text_result(),
        ))),
        error: ai_daily_scanner_contract::Nullable(None),
    };
    response.validate().expect("session response validates");
    let parsed_response: PythonSessionResponseV1 =
        serde_json::from_str(&serde_json::to_string(&response).expect("serialize"))
            .expect("deserialize");
    assert_eq!(parsed_response, response);

    let bad_response = PythonSessionResponseV1 {
        contract: "ai_daily_python_session".to_string(),
        protocol_version: 1,
        request_id: request_id(),
        operation: PythonSessionOperation::ClassifyPdfV1,
        status: PythonSessionResponseStatus::Error,
        result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Classify(
            text_result(),
        ))),
        error: ai_daily_scanner_contract::Nullable(None),
    };
    assert!(bad_response.validate().is_err());
}

#[test]
fn session_contract_version_is_frozen() {
    assert_eq!(SESSION_CONTRACT_VERSION, "ai_daily_python_session_v1");
}

#[test]
fn session_recycles_on_idle_ttl_not_process_age() {
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
    let expected = register_session_version(&command, Duration::from_secs(20))
        .expect("session-version preflight")
        .expect("session capability must be present");
    let mut params = SessionParams::default();
    params.idle_ttl = Duration::from_millis(60);
    params.max_requests_per_session = u64::MAX;
    let mut session = PythonSession::start(
        &command,
        &expected.identity,
        params,
        Duration::from_secs(20),
    )
    .expect("session starts");
    assert!(!session.recycle_due(), "fresh session must not be idle");

    std::thread::sleep(Duration::from_millis(120));
    assert!(session.recycle_due(), "idle TTL must be measured from idle time");

    // A completed request resets the idle clock. If recycle were process-age
    // based, it would still be due here (120ms > 60ms from spawn).
    let metadata = std::fs::metadata(&fixture).expect("fixture metadata");
    let modified = metadata
        .modified()
        .expect("fixture modified time")
        .duration_since(UNIX_EPOCH)
        .expect("fixture mtime after epoch");
    let source_version =
        ai_daily_discovery::build_source_version(modified.as_nanos(), metadata.len());
    let classify_request = build_classify_request(request_id(), &fixture, &source_version, 5);
    session_classify(&mut session, &classify_request, Duration::from_secs(20))
        .expect("session classify completes");
    assert!(
        !session.recycle_due(),
        "a completed request must reset the idle timer, not the process age"
    );

    session.kill();
}

#[test]
fn capability_preflight_batch_registers_classifier_and_session() {
    let Some(python) = python_executable() else {
        return;
    };
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
    let (classifier, session) =
        ai_daily_scanner_core::parsers::preflight_python_capabilities(
            &command,
            Duration::from_secs(20),
        );
    let classifier = classifier.expect("classifier-version preflight must succeed");
    let session = session
        .expect("session-version preflight must not fail")
        .expect("real worker must advertise session capability");
    assert_eq!(
        classifier.identity.classifier_contract_version,
        "ai_daily_pdf_classifier_v1"
    );
    assert_eq!(
        session.identity.session_contract_version,
        "ai_daily_python_session_v1"
    );
    assert_eq!(
        session.identity.supported_operations,
        vec!["classify_pdf_v1".to_string(), "parse_v1".to_string()]
    );
    assert_eq!(
        classifier.identity.classifier_build,
        session.identity.classifier_build,
        "classifier build must be shared by the classifier and session handshakes"
    );
}

#[test]
fn session_process_classifies_and_parses_text_pdf() {
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
        current_dir: Some(repository_root.clone()),
        expected_kind: WorkerKind::PythonDocument,
        required_backends: vec!["pdf_text_v1".to_string()],
        required_extensions: vec![".pdf".to_string()],
    };
    let expected = register_session_version(&command, Duration::from_secs(20))
        .expect("session-version preflight")
        .expect("session capability must be present");

    let metadata = std::fs::metadata(&fixture).expect("fixture metadata");
    let modified = metadata
        .modified()
        .expect("fixture modified time")
        .duration_since(UNIX_EPOCH)
        .expect("fixture mtime after epoch");
    let source_version =
        ai_daily_discovery::build_source_version(modified.as_nanos(), metadata.len());

    let mut session = PythonSession::start(
        &command,
        &expected.identity,
        SessionParams::default(),
        Duration::from_secs(20),
    )
    .expect("session starts and hello matches preflight");

    let classify_request = build_classify_request(
        request_id(),
        &fixture,
        &source_version,
        5,
    );
    let classify_result =
        session_classify(&mut session, &classify_request, Duration::from_secs(20))
            .expect("session classify completes");
    assert_eq!(classify_result.status, PdfClassifierResultStatus::TextInParseWindow);
    assert_eq!(classify_result.page_count.0, Some(1));

    let parse_request = ai_daily_scanner_contract::WorkerParseRequest {
        contract: "ai_daily_worker".to_string(),
        protocol_version: 1,
        request_id: request_id(),
        file_path: fixture.to_string_lossy().into_owned(),
        file_type: ".pdf".to_string(),
        backend: ai_daily_scanner_contract::WorkerBackend::PdfTextV1,
        remaining_timeout_ms: 30_000,
        max_file_size_bytes: 1_000_000,
        parser_limits: ai_daily_scanner_contract::WorkerParserLimits::Pdf {
            max_pages: 5,
            excerpt_max_chars: 4000,
        },
        expected_source_version: source_version.clone(),
    };
    let parse_result = session_parse(&mut session, &parse_request, Duration::from_secs(20))
        .expect("session parse completes");
    assert_eq!(parse_result.status, ai_daily_scanner_contract::WorkerStatus::Ok);
    assert_eq!(parse_result.parser_backend, ai_daily_scanner_contract::WorkerBackend::PdfTextV1);
    assert_eq!(parse_result.observed_source_version, source_version);

    session.kill();
    assert!(!session.is_alive());
}
