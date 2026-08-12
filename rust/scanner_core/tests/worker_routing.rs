use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use ai_daily_scanner_contract::{ErrorCode, ReportMode, ScannerSettings};
use ai_daily_scanner_core::classifier::ParserRoute;
use ai_daily_scanner_core::config::normalize_scanner_settings;
use ai_daily_scanner_core::fallback::FailureClass;
use ai_daily_scanner_core::parsers::{register_worker, register_worker_pair, WorkerCommand};
use ai_daily_scanner_core::planner::{plan_candidates, PlanAction};
use ai_daily_worker_contract::WorkerKind;

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
        let (required_backends, required_extensions) = capabilities(kind);
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
            required_backends,
            required_extensions,
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
fn explicitly_enabled_legacy_office_routes_are_planned_for_python_worker() {
    let Some(python) = python_executable() else {
        return;
    };
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("project root should exist");
    let _ = python;
    let profile = legacy_office_profile();
    let fixtures = project_root.join("tests/fixtures/worker_documents");
    let candidates = [
        ("legacy_sample.xls", ".xls", "python_office_v2"),
        ("legacy_sample.doc", ".doc", "python_sharepoint_text_v2"),
        ("legacy_sample.ppt", ".ppt", "python_sharepoint_text_v2"),
    ]
    .into_iter()
    .map(|(name, extension, _)| discovered_file_with_extension(&fixtures.join(name), extension))
    .collect::<Vec<_>>();
    let planned = plan_candidates(candidates, &profile);
    assert_eq!(planned.len(), 3);
    let action_by_extension = planned
        .iter()
        .map(|planned| (planned.file.extension.as_str(), &planned.action))
        .collect::<std::collections::HashMap<_, _>>();
    assert!(matches!(
        action_by_extension[".xls"],
        PlanAction::Parse(ParserRoute::PythonOffice)
    ));
    for extension in [".doc", ".ppt"] {
        assert!(matches!(
            action_by_extension[extension],
            PlanAction::Parse(ParserRoute::PythonSharepointText)
        ));
    }
}

#[test]
fn missing_office_and_python_workers_are_environment_unavailable() {
    let directory = tempfile::tempdir().expect("temporary root should exist");
    for (kind, backend, extension) in [
        (WorkerKind::Office, "rust_office_oxide_v2", ".docx"),
        (WorkerKind::PythonDocument, "python_pdf_text_v2", ".pdf"),
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
        WorkerKind::PythonDocument => (
            vec![
                "python_pdf_text_v2".to_string(),
                "python_office_v2".to_string(),
                "python_sharepoint_text_v2".to_string(),
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
    backends = ["rust_office_oxide_v2", "rust_xlsx_bounded_v2"]
    extensions = [".docx", ".pptx", ".xlsx"]
else:
    worker_kind = "python_document"
    backends = ["python_pdf_text_v2", "python_office_v2", "python_sharepoint_text_v2"]
    extensions = [".doc", ".docx", ".pdf", ".ppt", ".pptx", ".xls", ".xlsx"]

if operation == "hello":
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
    operations = ["office_parse"] if kind == "office" else [
        "pdf_classify", "pdf_parse", "python_office_parse", "python_sharepoint_parse"
    ]
    print(json.dumps({
        "contract": "ai_daily_worker",
        "protocol_version": 2,
        "frame": "hello",
        "worker_contract_version": "ai_daily_worker_v2",
        "worker_kind": worker_kind,
        "worker_version": "0.1.0",
        "worker_build": "fake-build",
        "supported_operations": operations,
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
lane = "rust_office_process_v2" if backend.startswith("rust_") else "python_document_process_v2"
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
    "worker_contract_version": "ai_daily_worker_v2",
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
    response["parser_backend"] = "rust_office_oxide_v2"
print(json.dumps(response))
raise SystemExit(exit_code)
"#;
