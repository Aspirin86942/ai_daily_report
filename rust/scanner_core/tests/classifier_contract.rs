//! PDF 分类器 wire 与本地 port（spec Part 7.1 / Part 3.2）的测试。

use ai_daily_scanner_core::session::{
    build_classify_request, session_classify, session_parse, SessionParams,
    WorkerSession as PythonSession, SESSION_CONTRACT_VERSION,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use ai_daily_scanner_contract::{ReportMode, ScannerSettings};
use ai_daily_scanner_core::config::normalize_scanner_settings;
use ai_daily_scanner_core::parsers::classifier::ClassifyOperation;
use ai_daily_scanner_core::parsers::classifier::{PdfClassifierExecution, PdfClassifierPort};
use ai_daily_scanner_core::parsers::{
    register_worker, worker_hello_from_registered, ParseOperation, WorkerCommand,
};
use ai_daily_scanner_core::store::classifier_profile_hash;
use ai_daily_worker_contract::{
    ClassifyRequest, ClassifyResult, ClassifyStatus, ParseRequest, ParserBackend, ParserLimits,
    WorkerDiagnostic, WorkerKind,
};

fn normalized_settings(mode: ReportMode) -> ai_daily_scanner_contract::NormalizedScannerSettings {
    let raw: ScannerSettings =
        serde_json::from_value(serde_json::json!({})).expect("minimal v2 raw profile");
    normalize_scanner_settings(&raw, mode).expect("normalized v2 profile")
}

fn request_id() -> String {
    "61111111-6111-4111-8111-611111111111".to_string()
}

fn text_result() -> ClassifyResult {
    ClassifyResult {
        status: ClassifyStatus::TextInParseWindow,
        page_count: Some(1),
        result_examined_pages: Some(1),
        diagnostic: None,
    }
}

fn request(file_path: &str, source_version: &str) -> ClassifyOperation {
    ClassifyOperation {
        request_id: request_id(),
        payload: ClassifyRequest {
            file_path: file_path.to_string(),
            source_version: source_version.to_string(),
            max_pages: 5,
            policy_version: "pdf_text_presence_v1".to_string(),
        },
    }
}

#[test]
fn classifier_domain_types_round_trip_and_validate() {
    let result = text_result();
    result.validate().expect("text result validates");
    let json = serde_json::to_string(&result).expect("serialize");
    let parsed: ClassifyResult = serde_json::from_str(&json).expect("deserialize");
    parsed.validate().expect("round-trip validates");
    assert_eq!(parsed, result);
}

#[test]
fn classifier_domain_rejects_bad_status_invariants() {
    // text result must not carry a diagnostic
    let bad_result = ClassifyResult {
        status: ClassifyStatus::TextInParseWindow,
        page_count: Some(1),
        result_examined_pages: Some(1),
        diagnostic: Some(WorkerDiagnostic {
            error_code: "PARSER_FAILED".to_string(),
            message: "unexpected diagnostic".to_string(),
            retryable: false,
            stage: "parse".to_string(),
            file_path: None,
            backend: None,
        }),
    };
    assert!(bad_result.validate().is_err());

    // unknown must carry a retryable diagnostic
    let bad_unknown = ClassifyResult {
        status: ClassifyStatus::Unknown,
        page_count: None,
        result_examined_pages: None,
        diagnostic: None,
    };
    assert!(bad_unknown.validate().is_err());

    // error must not be retryable
    let bad_error = ClassifyResult {
        status: ClassifyStatus::Error,
        page_count: None,
        result_examined_pages: None,
        diagnostic: Some(WorkerDiagnostic {
            error_code: "PARSER_FAILED".to_string(),
            message: "deterministic error".to_string(),
            retryable: true,
            stage: "parse".to_string(),
            file_path: None,
            backend: None,
        }),
    };
    assert!(bad_error.validate().is_err());
}

#[test]
fn classifier_result_is_bounded_by_the_request_window() {
    let mut text = text_result();
    text.page_count = Some(10);
    text.result_examined_pages = Some(6);
    text.validate()
        .expect("generic typed result is structurally valid");
    assert!(
        text.validate_for_max_pages(5).is_err(),
        "text result pages must not exceed request.max_pages"
    );

    let mut no_text = text_result();
    no_text.status = ClassifyStatus::NoTextInParseWindow;
    no_text.page_count = Some(10);
    no_text.result_examined_pages = Some(4);
    assert!(
        no_text.validate_for_max_pages(5).is_err(),
        "no-text must examine the complete request window"
    );

    let unknown = ClassifyResult {
        status: ClassifyStatus::Unknown,
        page_count: None,
        result_examined_pages: Some(6),
        diagnostic: Some(WorkerDiagnostic {
            error_code: "PARSER_FAILED".to_string(),
            message: "transient classifier failure".to_string(),
            retryable: true,
            stage: "parse".to_string(),
            file_path: None,
            backend: None,
        }),
    };
    assert!(
        unknown.validate_for_max_pages(5).is_err(),
        "typed failures must also stay within request.max_pages"
    );
}

#[derive(Clone)]
struct StubClassifier {
    result: Option<ClassifyResult>,
}

impl PdfClassifierPort for StubClassifier {
    fn classify_pdf(
        &self,
        _request: &ClassifyOperation,
        _timeout: Duration,
    ) -> PdfClassifierExecution {
        PdfClassifierExecution::test_execution(self.result.clone().ok_or_else(|| {
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
    assert_eq!(outcome.status, ClassifyStatus::TextInParseWindow);
}

#[test]
fn classifier_profile_hash_is_canonical_and_input_sensitive() {
    let profile = normalized_settings(ReportMode::Daily);
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
fn session_contract_version_is_frozen() {
    assert_eq!(SESSION_CONTRACT_VERSION, "ai_daily_worker_v2");
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
        required_backends: vec!["python_pdf_text_v2".to_string()],
        required_extensions: vec![".pdf".to_string()],
    };
    let registered =
        register_worker(&command, Duration::from_secs(20)).expect("worker-v2 hello preflight");
    let expected = worker_hello_from_registered(&registered);
    let params = SessionParams {
        idle_ttl: Duration::from_millis(60),
        max_requests_per_session: u64::MAX,
        ..SessionParams::default()
    };
    let mut session = PythonSession::start(&command, &expected, params, Duration::from_secs(20))
        .expect("session starts");
    assert!(!session.recycle_due(), "fresh session must not be idle");

    std::thread::sleep(Duration::from_millis(120));
    assert!(
        session.recycle_due(),
        "idle TTL must be measured from idle time"
    );

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
fn worker_v2_hello_registers_all_python_operations() {
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
        required_backends: vec!["python_pdf_text_v2".to_string()],
        required_extensions: vec![".pdf".to_string()],
    };
    let worker =
        register_worker(&command, Duration::from_secs(20)).expect("worker-v2 hello preflight");
    let hello = worker_hello_from_registered(&worker);
    assert_eq!(hello.worker_contract_version, "ai_daily_worker_v2");
    assert_eq!(
        hello.supported_operations,
        vec![
            ai_daily_worker_contract::WorkerOperation::PdfClassify,
            ai_daily_worker_contract::WorkerOperation::PdfParse,
            ai_daily_worker_contract::WorkerOperation::PythonOfficeParse,
            ai_daily_worker_contract::WorkerOperation::PythonSharepointParse,
        ]
    );
    assert_eq!(hello.worker_build.len(), 64);
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
        required_backends: vec!["python_pdf_text_v2".to_string()],
        required_extensions: vec![".pdf".to_string()],
    };
    let registered =
        register_worker(&command, Duration::from_secs(20)).expect("worker-v2 hello preflight");
    let expected = worker_hello_from_registered(&registered);

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
        &expected,
        SessionParams::default(),
        Duration::from_secs(20),
    )
    .expect("session starts and hello matches preflight");

    let classify_request = build_classify_request(request_id(), &fixture, &source_version, 5);
    let classify_result =
        session_classify(&mut session, &classify_request, Duration::from_secs(20))
            .expect("session classify completes");
    assert_eq!(classify_result.status, ClassifyStatus::TextInParseWindow);
    assert_eq!(classify_result.page_count, Some(1));

    let parse_request = ParseOperation {
        request_id: request_id(),
        payload: ParseRequest {
            file_path: fixture.to_string_lossy().into_owned(),
            file_type: ".pdf".to_string(),
            backend: ParserBackend::PythonPdfTextV2,
            remaining_timeout_ms: 30_000,
            max_file_size_bytes: 1_000_000,
            parser_limits: ParserLimits::Pdf {
                max_pages: 5,
                excerpt_max_chars: 4000,
            },
            expected_source_version: source_version.clone(),
        },
    };
    let parse_result = session_parse(&mut session, &parse_request, Duration::from_secs(20))
        .expect("session parse completes");
    assert_eq!(parse_result.parser_backend, ParserBackend::PythonPdfTextV2);
    assert_eq!(parse_result.observed_source_version, source_version);

    session.kill();
    assert!(!session.is_alive());
}
