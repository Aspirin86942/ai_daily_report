use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use ai_daily_scanner_contract::{
    AdapterPaths, ErrorCode, FallbackBackend, OfficeParseProfile, RawScannerProfileV1, ReportMode,
    WorkerBackend, WorkerKind, WorkerParseRequest, WorkerParserLimits, WorkerVersionResponse,
};
use ai_daily_scanner_core::config::normalize_scanner_profile;
use ai_daily_scanner_core::fallback::FailureClass;
use ai_daily_scanner_core::parsers::office::parse_with_fallback;
use ai_daily_scanner_core::parsers::{
    execute_worker_request, preflight_commands_then, preflight_then, register_worker,
    register_worker_pair, ParsedPayload, ParserScheduler, RegisteredWorker, WorkerCommand,
    WorkerRegistry,
};
use ai_daily_scanner_core::planner::plan_candidates;
use tempfile::TempDir;

#[test]
fn preflight_completes_before_cache_continuation() {
    let Some(python) = python_executable() else {
        return;
    };
    let directory = tempfile::tempdir().expect("temporary worker root should exist");
    let marker = directory.path().join("handshake.marker");
    write_module_worker(directory.path(), &marker, true);
    let profile = pdf_only_profile();
    let adapters = AdapterPaths {
        office_worker_path: directory
            .path()
            .join("unused-office.exe")
            .to_string_lossy()
            .into_owned(),
        python_executable: python.to_string_lossy().into_owned(),
        python_module_root: directory.path().to_string_lossy().into_owned(),
        python_document_worker_module: "fake_preflight".to_string(),
    };
    let cache_called = AtomicBool::new(false);

    preflight_then(&profile, &adapters, Duration::from_secs(2), |_| {
        assert!(
            marker.is_file(),
            "handshake must happen before cache lookup"
        );
        cache_called.store(true, Ordering::SeqCst);
    })
    .expect("valid preflight should reach the continuation");

    assert!(cache_called.load(Ordering::SeqCst));
}

#[test]
fn failed_preflight_never_reaches_cache_continuation() {
    let Some(python) = python_executable() else {
        return;
    };
    let directory = tempfile::tempdir().expect("temporary worker root should exist");
    let marker = directory.path().join("handshake.marker");
    write_module_worker(directory.path(), &marker, false);
    let profile = pdf_only_profile();
    let adapters = AdapterPaths {
        office_worker_path: directory
            .path()
            .join("unused-office.exe")
            .to_string_lossy()
            .into_owned(),
        python_executable: python.to_string_lossy().into_owned(),
        python_module_root: directory.path().to_string_lossy().into_owned(),
        python_document_worker_module: "fake_preflight".to_string(),
    };
    let cache_called = AtomicBool::new(false);

    let failure = preflight_then(&profile, &adapters, Duration::from_secs(2), |_| {
        cache_called.store(true, Ordering::SeqCst)
    })
    .expect_err("invalid handshake must stop the run before cache lookup");

    assert_eq!(failure.class, FailureClass::ContractFailure);
    assert!(!cache_called.load(Ordering::SeqCst));
}

#[test]
fn office_and_python_handshakes_both_finish_before_cache_continuation() {
    let Some(python) = python_executable() else {
        return;
    };
    let directory = tempfile::tempdir().expect("temporary worker root should exist");
    let script = directory.path().join("fake_worker.py");
    let marker = directory.path().join("all-handshakes.marker");
    fs::write(&script, FAKE_WORKER).expect("fake worker should be writable");
    let command = |kind: WorkerKind| {
        let identity = identity(kind);
        WorkerCommand {
            program: python.clone(),
            base_args: vec![
                OsString::from(script.as_os_str()),
                OsString::from("valid"),
                OsString::from(match kind {
                    WorkerKind::Office => "office",
                    WorkerKind::PythonDocument => "python",
                }),
                OsString::from(marker.as_os_str()),
            ],
            current_dir: Some(directory.path().to_path_buf()),
            expected_kind: kind,
            required_backends: identity.supported_backends,
            required_extensions: identity.supported_extensions,
        }
    };
    let office = command(WorkerKind::Office);
    let python_document = command(WorkerKind::PythonDocument);

    preflight_commands_then(
        Some(&office),
        Some(&python_document),
        Duration::from_secs(2),
        |registry| {
            assert!(registry.office.is_some());
            assert!(registry.python_document.is_some());
            let marker_text = fs::read_to_string(&marker).expect("handshake marker should exist");
            let mut completed = marker_text.lines().collect::<Vec<_>>();
            completed.sort_unstable();
            assert_eq!(completed, ["office", "python"]);
        },
    )
    .expect("both worker handshakes should precede cache continuation");
}

#[test]
fn office_and_python_worker_handshakes_overlap() {
    let Some(python) = python_executable() else {
        return;
    };
    let directory = tempfile::tempdir().expect("temporary worker root should exist");
    let script = directory.path().join("fake_worker.py");
    fs::write(&script, FAKE_WORKER).expect("fake worker should be writable");
    let office_marker = directory.path().join("office.started");
    let python_marker = directory.path().join("python.started");
    let command = |kind: WorkerKind, own: &Path, peer: &Path| {
        let identity = identity(kind);
        WorkerCommand {
            program: python.clone(),
            base_args: vec![
                OsString::from(script.as_os_str()),
                OsString::from("rendezvous"),
                OsString::from(match kind {
                    WorkerKind::Office => "office",
                    WorkerKind::PythonDocument => "python",
                }),
                OsString::from(own.as_os_str()),
                OsString::from(peer.as_os_str()),
            ],
            current_dir: Some(directory.path().to_path_buf()),
            expected_kind: kind,
            required_backends: identity.supported_backends,
            required_extensions: identity.supported_extensions,
        }
    };
    let office = command(WorkerKind::Office, &office_marker, &python_marker);
    let python_document = command(WorkerKind::PythonDocument, &python_marker, &office_marker);

    let (office_result, python_result) =
        register_worker_pair(&office, &python_document, Duration::from_secs(5));

    office_result.expect("office handshake should rendezvous");
    python_result.expect("Python handshake should rendezvous");
}

#[test]
fn scheduler_routes_text_to_rust_core_with_stable_audit_fields() {
    let directory = tempfile::tempdir().expect("temporary text root should exist");
    let path = directory.path().join("evidence.txt");
    fs::write(&path, "Rust core text evidence").expect("text fixture should be writable");
    let profile = text_only_profile();
    let candidate = discovered_file(&path, source_version(&path));
    let planned = plan_candidates(vec![candidate], &profile);
    let scheduler = ParserScheduler::from_registry(&profile, WorkerRegistry::default());

    let parsed = scheduler
        .parse_planned_files(&planned)
        .expect("Rust parser pool should run");

    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].parser_backend.as_deref(), Some("light_text_v1"));
    assert_eq!(parsed[0].worker_lane.as_deref(), Some("rust_core"));
    assert!(!parsed[0].partial);
    assert!(matches!(
        parsed[0].payload,
        Some(ParsedPayload::LightText(_))
    ));
}

#[test]
fn scheduler_rejects_stale_text_source_before_parsing() {
    let directory = tempfile::tempdir().expect("temporary text root should exist");
    let path = directory.path().join("changed.txt");
    fs::write(&path, "changed text").expect("text fixture should be writable");
    let profile = text_only_profile();
    let candidate = discovered_file(&path, "mtime_ns=1:size=1".to_string());
    let planned = plan_candidates(vec![candidate], &profile);
    let scheduler = ParserScheduler::from_registry(&profile, WorkerRegistry::default());

    let parsed = scheduler
        .parse_planned_files(&planned)
        .expect("Rust parser pool should run");

    assert_eq!(
        parsed[0]
            .error
            .as_ref()
            .expect("stale source needs an error")
            .diagnostic
            .error_code,
        ErrorCode::SourceVersionChanged
    );
    assert!(parsed[0].payload.is_none());
}

#[test]
fn explicitly_enabled_legacy_office_routes_through_strict_python_worker() {
    let Some(python) = python_executable() else {
        return;
    };
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("project root should exist");
    let identity = identity(WorkerKind::PythonDocument);
    let worker = register_worker(
        &WorkerCommand {
            program: python,
            base_args: vec![
                OsString::from("-m"),
                OsString::from("src.workers.document_parser_worker"),
            ],
            current_dir: Some(project_root.clone()),
            expected_kind: WorkerKind::PythonDocument,
            required_backends: identity.supported_backends,
            required_extensions: identity.supported_extensions,
        },
        Duration::from_secs(10),
    )
    .expect("real Python worker should pass strict preflight");
    let profile = legacy_office_profile();
    let fixtures = project_root.join("tests/fixtures/worker_documents");
    let candidates = [
        ("legacy_sample.xls", ".xls", "python_office_v1"),
        ("legacy_sample.doc", ".doc", "python_sharepoint_text_v1"),
        ("legacy_sample.ppt", ".ppt", "python_sharepoint_text_v1"),
    ]
    .into_iter()
    .map(|(name, extension, _)| discovered_file_with_extension(&fixtures.join(name), extension))
    .collect::<Vec<_>>();
    let planned = plan_candidates(candidates, &profile);
    let scheduler = ParserScheduler::from_registry(
        &profile,
        WorkerRegistry {
            office: None,
            python_document: Some(worker),
        },
    );

    let parsed = scheduler
        .parse_planned_files(&planned)
        .expect("legacy files should run within the Rust scheduler deadline");

    assert_eq!(parsed.len(), 3);
    for result in &parsed {
        let backend = match result.file.extension.as_str() {
            ".xls" => "python_office_v1",
            ".doc" | ".ppt" => "python_sharepoint_text_v1",
            extension => panic!("unexpected legacy route: {extension}"),
        };
        assert_eq!(result.parser_backend.as_deref(), Some(backend));
        assert_eq!(
            result.worker_lane.as_deref(),
            Some("python_document_process")
        );
        assert!(!result.partial, "{backend}: {:?}", result.error);
        let Some(ParsedPayload::Worker(response)) = &result.payload else {
            panic!("{backend} must return a strict worker response");
        };
        assert!(response.content.contains("Legacy"), "{backend}");
    }
}

#[test]
fn missing_office_and_python_workers_are_environment_unavailable() {
    let directory = tempfile::tempdir().expect("temporary root should exist");
    for (kind, backend, extension) in [
        (WorkerKind::Office, "rust_office_oxide_v1", ".docx"),
        (WorkerKind::PythonDocument, "pdf_text_v1", ".pdf"),
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

#[test]
fn invalid_json_is_a_contract_failure() {
    let Some((directory, worker)) = fake_registered_worker("invalid_json", WorkerKind::Office)
    else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 1_000);

    let failure = execute_worker_request(&worker, &request)
        .expect_err("invalid JSON must fail the strict response contract");

    assert_eq!(failure.class, FailureClass::ContractFailure);
    assert_eq!(
        failure.diagnostic.error_code,
        ErrorCode::ParserInvalidPayload
    );
}

#[test]
fn wrong_path_or_backend_is_a_contract_failure() {
    for mode in ["wrong_path", "wrong_backend"] {
        let Some((directory, worker)) = fake_registered_worker(mode, WorkerKind::Office) else {
            return;
        };
        let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 1_000);

        let failure = execute_worker_request(&worker, &request)
            .expect_err("mismatched worker echo must fail the contract");

        assert_eq!(failure.class, FailureClass::ContractFailure, "{mode}");
    }
}

#[test]
fn changed_build_after_preflight_is_a_contract_failure() {
    let Some((directory, worker)) = fake_registered_worker("wrong_build", WorkerKind::Office)
    else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 1_000);

    let failure = execute_worker_request(&worker, &request)
        .expect_err("changed worker build must fail before caching");

    assert_eq!(failure.class, FailureClass::ContractFailure);
    assert_eq!(failure.diagnostic.error_code, ErrorCode::WorkerBuildChanged);
}

#[test]
fn sleep_past_deadline_is_deterministic_timeout() {
    let Some((directory, worker)) = fake_registered_worker("sleep", WorkerKind::Office) else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 100);
    let started = Instant::now();

    let failure = execute_worker_request(&worker, &request)
        .expect_err("sleeping worker must be terminated at the deadline");

    assert_eq!(failure.class, FailureClass::Deterministic);
    assert_eq!(failure.diagnostic.error_code, ErrorCode::ParserTimeout);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn worker_crash_is_recoverable_parser_failure() {
    let Some((directory, worker)) = fake_registered_worker("crash", WorkerKind::Office) else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 1_000);

    let failure = execute_worker_request(&worker, &request)
        .expect_err("crashed worker must produce a typed failure");

    assert_eq!(failure.class, FailureClass::RecoverableParserFailure);
    assert_eq!(failure.diagnostic.error_code, ErrorCode::ParserFailed);
}

#[test]
fn retryable_timeout_remains_deterministic_and_does_not_fallback() {
    let Some((directory, primary)) =
        fake_registered_worker("retryable_timeout", WorkerKind::Office)
    else {
        return;
    };
    let Some((_fallback_directory, fallback)) =
        fake_registered_worker("valid", WorkerKind::PythonDocument)
    else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 3_000);

    let execution =
        parse_with_fallback(&primary, Some(&fallback), &request, &office_profile(false));

    assert!(execution.response.is_none());
    assert_eq!(execution.fallback_backend, None);
    let failure = execution
        .final_failure
        .expect("timeout must be returned without fallback");
    assert_eq!(failure.class, FailureClass::Deterministic);
    assert_eq!(failure.diagnostic.error_code, ErrorCode::ParserTimeout);
}

#[test]
fn valid_response_above_four_mib_respects_the_contract_character_budget() {
    let Some((directory, worker)) = fake_registered_worker("large_response", WorkerKind::Office)
    else {
        return;
    };
    let mut request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 10_000);
    let WorkerParserLimits::Office {
        document_excerpt_max_chars,
        ..
    } = &mut request.parser_limits
    else {
        unreachable!("office request must use office limits");
    };
    *document_excerpt_max_chars = 5_500_000;

    let response = execute_worker_request(&worker, &request)
        .expect("a valid response above the old fixed cap must succeed");

    assert_eq!(response.content.len(), 5_000_000);
}

#[test]
fn response_content_over_the_request_budget_is_a_contract_failure() {
    let Some((directory, worker)) =
        fake_registered_worker("content_over_budget", WorkerKind::Office)
    else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 3_000);

    let failure = execute_worker_request(&worker, &request)
        .expect_err("worker content must not exceed the requested character budget");

    assert_eq!(failure.class, FailureClass::ContractFailure);
    assert_eq!(
        failure.diagnostic.error_code,
        ErrorCode::ParserInvalidPayload
    );
}

#[test]
fn fallback_uses_only_remaining_file_deadline_and_marks_partial() {
    let Some((directory, primary)) = fake_registered_worker("recoverable_slow", WorkerKind::Office)
    else {
        return;
    };
    let Some((_fallback_directory, fallback)) =
        fake_registered_worker("valid", WorkerKind::PythonDocument)
    else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 3_000);

    let execution =
        parse_with_fallback(&primary, Some(&fallback), &request, &office_profile(false));

    let response = execution
        .response
        .clone()
        .unwrap_or_else(|| panic!("fallback should succeed: {execution:?}"));
    assert!(execution.partial);
    assert!(execution.primary_failure.is_some());
    assert_eq!(
        execution.fallback_backend,
        Some(WorkerBackend::PythonOfficeV1)
    );
    assert_eq!(response.parser_backend, WorkerBackend::PythonOfficeV1);
    assert!(execution.primary_duration_ms >= 40);
    assert!(execution.fallback_duration_ms < 1_000);
}

#[test]
fn primary_and_fallback_share_one_wall_clock_deadline() {
    let Some((directory, primary)) = fake_registered_worker("recoverable_late", WorkerKind::Office)
    else {
        return;
    };
    let Some((_fallback_directory, fallback)) =
        fake_registered_worker("valid_slow", WorkerKind::PythonDocument)
    else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 2_000);
    let started = Instant::now();

    let execution =
        parse_with_fallback(&primary, Some(&fallback), &request, &office_profile(false));

    assert!(execution.response.is_none());
    assert_eq!(
        execution.fallback_backend,
        Some(WorkerBackend::PythonOfficeV1)
    );
    assert_eq!(
        execution
            .final_failure
            .expect("fallback must consume only the remaining deadline")
            .diagnostic
            .error_code,
        ErrorCode::ParserTimeout
    );
    assert!(started.elapsed() < Duration::from_millis(2_500));
}

#[test]
fn timeout_does_not_fallback_by_default() {
    let Some((directory, primary)) = fake_registered_worker("early_timeout", WorkerKind::Office)
    else {
        return;
    };
    let Some((_fallback_directory, fallback)) =
        fake_registered_worker("valid", WorkerKind::PythonDocument)
    else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 1_000);

    let execution =
        parse_with_fallback(&primary, Some(&fallback), &request, &office_profile(false));

    assert!(execution.response.is_none());
    assert_eq!(execution.fallback_backend, None);
    assert_eq!(
        execution
            .final_failure
            .expect("primary timeout should be returned")
            .diagnostic
            .error_code,
        ErrorCode::ParserTimeout
    );
}

#[test]
fn explicitly_enabled_early_timeout_can_use_remaining_time() {
    let Some((directory, primary)) = fake_registered_worker("early_timeout", WorkerKind::Office)
    else {
        return;
    };
    let Some((_fallback_directory, fallback)) =
        fake_registered_worker("valid", WorkerKind::PythonDocument)
    else {
        return;
    };
    let request = office_request(&directory, WorkerBackend::RustXlsxBoundedV1, 5_000);

    let execution = parse_with_fallback(&primary, Some(&fallback), &request, &office_profile(true));

    assert!(execution.response.is_some());
    assert!(execution.partial);
    assert_eq!(
        execution.fallback_backend,
        Some(WorkerBackend::PythonOfficeV1)
    );
}

fn fake_registered_worker(mode: &str, kind: WorkerKind) -> Option<(TempDir, RegisteredWorker)> {
    let python = python_executable()?;
    let directory = tempfile::tempdir().ok()?;
    let script = directory.path().join("fake_worker.py");
    fs::write(&script, FAKE_WORKER).ok()?;
    let identity = identity(kind);
    let command = WorkerCommand {
        program: python,
        base_args: vec![
            OsString::from(script.as_os_str()),
            OsString::from(mode),
            OsString::from(match kind {
                WorkerKind::Office => "office",
                WorkerKind::PythonDocument => "python",
            }),
        ],
        current_dir: Some(directory.path().to_path_buf()),
        expected_kind: kind,
        required_backends: identity.supported_backends.clone(),
        required_extensions: identity.supported_extensions.clone(),
    };
    Some((directory, RegisteredWorker { command, identity }))
}

fn identity(kind: WorkerKind) -> WorkerVersionResponse {
    let (backends, extensions) = match kind {
        WorkerKind::Office => (
            vec![
                "rust_office_oxide_v1".to_string(),
                "rust_xlsx_bounded_v1".to_string(),
            ],
            vec![
                ".docx".to_string(),
                ".pptx".to_string(),
                ".xlsx".to_string(),
            ],
        ),
        WorkerKind::PythonDocument => (
            vec![
                "pdf_text_v1".to_string(),
                "python_office_v1".to_string(),
                "python_sharepoint_text_v1".to_string(),
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
    };
    WorkerVersionResponse {
        contract: "ai_daily_worker".to_string(),
        protocol_version: 1,
        worker_kind: kind,
        worker_contract_version: "ai_daily_worker_v1".to_string(),
        worker_version: "0.1.0".to_string(),
        worker_build: "fake-build".to_string(),
        supported_backends: backends,
        supported_extensions: extensions,
    }
}

fn office_request(
    directory: &TempDir,
    backend: WorkerBackend,
    timeout_ms: u64,
) -> WorkerParseRequest {
    let path = directory.path().join("fixture.xlsx");
    fs::write(&path, b"fixture").expect("fixture should be writable");
    let metadata = fs::metadata(&path).expect("fixture metadata should exist");
    let modified = metadata
        .modified()
        .expect("modified time should exist")
        .duration_since(UNIX_EPOCH)
        .expect("modified time should follow epoch");
    WorkerParseRequest {
        contract: "ai_daily_worker".to_string(),
        protocol_version: 1,
        request_id: "123e4567-e89b-42d3-a456-426614174000".to_string(),
        file_path: path.to_string_lossy().into_owned(),
        file_type: ".xlsx".to_string(),
        backend,
        remaining_timeout_ms: timeout_ms,
        max_file_size_bytes: 1_000_000,
        parser_limits: WorkerParserLimits::Office {
            excel_max_sheets: 1,
            excel_max_rows: 1,
            excel_max_columns: 1,
            docx_max_paragraphs: 1,
            docx_max_tables: 1,
            docx_table_max_rows: 1,
            docx_table_max_cols: 1,
            pptx_max_slides: 1,
            pptx_include_notes: false,
            document_excerpt_max_chars: 100,
        },
        expected_source_version: ai_daily_scanner_core::discovery::build_source_version(
            modified.as_nanos(),
            metadata.len(),
        ),
    }
}

fn office_profile(fallback_after_timeout: bool) -> OfficeParseProfile {
    OfficeParseProfile {
        primary_backend: "rust_office_oxide_v1".to_string(),
        fallback_enabled: true,
        fallback_order: vec![FallbackBackend::PythonOfficeV1],
        fallback_after_timeout,
        fallback_policy_version: "hybrid_v1".to_string(),
        legacy_extensions_enabled: false,
        excel_max_sheets: 1,
        excel_max_rows: 1,
        excel_max_columns: 1,
        docx_max_paragraphs: 1,
        docx_max_tables: 1,
        docx_table_max_rows: 1,
        docx_table_max_cols: 1,
        pptx_max_slides: 1,
        pptx_include_notes: false,
        document_excerpt_max_chars: 100,
    }
}

fn pdf_only_profile() -> ai_daily_scanner_contract::NormalizedScannerProfileV1 {
    let raw: RawScannerProfileV1 = serde_json::from_value(serde_json::json!({
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".pdf"],
        "office_parser_fallback_enabled": false,
        "office_parser_fallback_order": []
    }))
    .expect("PDF-only profile should decode");
    normalize_scanner_profile(&raw, ReportMode::Daily).expect("PDF-only profile should normalize")
}

fn text_only_profile() -> ai_daily_scanner_contract::NormalizedScannerProfileV1 {
    let raw: RawScannerProfileV1 = serde_json::from_value(serde_json::json!({
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".txt"],
        "office_parser_fallback_enabled": false,
        "office_parser_fallback_order": []
    }))
    .expect("text-only profile should decode");
    normalize_scanner_profile(&raw, ReportMode::Daily).expect("text-only profile should normalize")
}

fn legacy_office_profile() -> ai_daily_scanner_contract::NormalizedScannerProfileV1 {
    let raw: RawScannerProfileV1 = serde_json::from_value(serde_json::json!({
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".xls", ".doc", ".ppt"],
        "office_legacy_extensions_enabled": true,
        "office_parser_fallback_enabled": false,
        "office_parser_fallback_order": [],
        "max_workers": 3,
        "file_timeout_seconds": 30
    }))
    .expect("legacy profile should decode");
    normalize_scanner_profile(&raw, ReportMode::Daily).expect("legacy profile should normalize")
}

fn discovered_file(
    path: &Path,
    source_version: String,
) -> ai_daily_scanner_core::discovery::DiscoveredFileOut {
    let metadata = fs::metadata(path).expect("fixture metadata should exist");
    ai_daily_scanner_core::discovery::DiscoveredFileOut {
        file_identity: path.to_string_lossy().into_owned(),
        path: path.to_string_lossy().into_owned(),
        extension: ".txt".to_string(),
        modified_at: "2026-07-16T00:00:00+08:00".to_string(),
        size_bytes: metadata.len(),
        source_version,
        source_guard_kind: None,
        source_guard_sha256: None,
    }
}

fn discovered_file_with_extension(
    path: &Path,
    extension: &str,
) -> ai_daily_scanner_core::discovery::DiscoveredFileOut {
    let metadata = fs::metadata(path).expect("fixture metadata should exist");
    ai_daily_scanner_core::discovery::DiscoveredFileOut {
        file_identity: path.to_string_lossy().into_owned(),
        path: path.to_string_lossy().into_owned(),
        extension: extension.to_string(),
        modified_at: "2026-07-16T00:00:00+08:00".to_string(),
        size_bytes: metadata.len(),
        source_version: source_version(path),
        source_guard_kind: None,
        source_guard_sha256: None,
    }
}

fn source_version(path: &Path) -> String {
    let metadata = fs::metadata(path).expect("fixture metadata should exist");
    let modified = metadata
        .modified()
        .expect("modified time should exist")
        .duration_since(UNIX_EPOCH)
        .expect("modified time should follow epoch");
    ai_daily_scanner_core::discovery::build_source_version(modified.as_nanos(), metadata.len())
}

fn write_module_worker(root: &Path, marker: &Path, valid: bool) {
    let identity = serde_json::to_string(&identity(WorkerKind::PythonDocument))
        .expect("identity should serialize");
    let marker_literal =
        serde_json::to_string(&marker.to_string_lossy()).expect("marker path should serialize");
    let output_literal = if valid {
        serde_json::to_string(&identity).expect("identity JSON should quote")
    } else {
        "'not-json'".to_string()
    };
    let source = format!(
        "from pathlib import Path\nimport sys\nPath({marker_literal}).write_text('version', encoding='utf-8')\nprint({output_literal})\n"
    );
    fs::write(root.join("fake_preflight.py"), source)
        .expect("fake preflight module should be writable");
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

const FAKE_WORKER: &str = r#"
import json
import os
import sys
import time

mode = sys.argv[1]
kind = sys.argv[2]
operation = sys.argv[-1]

if kind == "office":
    worker_kind = "office"
    backends = ["rust_office_oxide_v1", "rust_xlsx_bounded_v1"]
    extensions = [".docx", ".pptx", ".xlsx"]
else:
    worker_kind = "python_document"
    backends = ["pdf_text_v1", "python_office_v1", "python_sharepoint_text_v1"]
    extensions = [".doc", ".docx", ".pdf", ".ppt", ".pptx", ".xls", ".xlsx"]

if operation == "version":
    if len(sys.argv) > 4:
        with open(sys.argv[3], "a", encoding="ascii") as marker:
            marker.write(kind + "\n")
    if mode == "version_sleep":
        time.sleep(30)
    if mode == "rendezvous":
        deadline = time.monotonic() + 2
        while not os.path.exists(sys.argv[4]) and time.monotonic() < deadline:
            time.sleep(0.005)
        if not os.path.exists(sys.argv[4]):
            raise SystemExit(7)
    print(json.dumps({
        "contract": "ai_daily_worker",
        "protocol_version": 1,
        "worker_kind": worker_kind,
        "worker_contract_version": "ai_daily_worker_v1",
        "worker_version": "0.1.0",
        "worker_build": "fake-build",
        "supported_backends": backends,
        "supported_extensions": extensions,
    }))
    raise SystemExit(0)

request = json.load(sys.stdin)
if mode == "sleep":
    time.sleep(30)
if mode == "crash":
    os._exit(9)
if mode == "invalid_json":
    print("not-json")
    raise SystemExit(0)
if mode == "recoverable_slow":
    time.sleep(0.05)
if mode == "recoverable_late":
    time.sleep(0.9)
if mode == "valid_slow":
    time.sleep(1.3)

backend = request["backend"]
lane = "rust_office_process" if backend.startswith("rust_") else "python_document_process"
error = None
status = "ok"
content = "parsed"
exit_code = 0
if mode in {"recoverable_slow", "recoverable_late", "early_timeout", "retryable_timeout"}:
    status = "error"
    content = ""
    exit_code = 1
    error = {
        "error_code": "PARSER_TIMEOUT" if mode in {"early_timeout", "retryable_timeout"} else "PARSER_FAILED",
        "message": "synthetic worker failure",
        "retryable": mode in {"recoverable_slow", "recoverable_late", "retryable_timeout"},
        "stage": "parse",
        "file_path": request["file_path"],
        "backend": backend,
    }

response = {
    "contract": "ai_daily_worker",
    "protocol_version": 1,
    "request_id": request["request_id"],
    "status": status,
    "file_path": request["file_path"],
    "file_type": request["file_type"],
    "content": content,
    "parser_backend": backend,
    "worker_lane": lane,
    "truncated": False,
    "warnings": [],
    "error": error,
    "duration_ms": 1,
    "worker_contract_version": "ai_daily_worker_v1",
    "worker_version": "0.1.0",
    "worker_build": "changed-build" if mode == "wrong_build" else "fake-build",
    "observed_source_version": request["expected_source_version"],
}
if mode == "large_response":
    response["content"] = "x" * 5_000_000
if mode == "content_over_budget":
    response["content"] = "x" * 101
if mode == "wrong_path":
    response["file_path"] = request["file_path"] + ".other"
if mode == "wrong_backend":
    response["parser_backend"] = "rust_office_oxide_v1"
print(json.dumps(response))
raise SystemExit(exit_code)
"#;
