use ai_daily_scanner_contract::{
    BuildContextRequest, ContextEnvelope, ContextSummary, Diagnostic, DiagnosticStage, DoctorCheck,
    DoctorCheckStatus, DoctorRequest, DoctorResponse, EngineStatus, ErrorCode, InspectRunRequest,
    InspectRunResponse, InspectStatus, Nullable, TransportErrorResponse, Validate, VersionResponse,
    WorkerKind, WorkerVersionResponse,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const WORKER_CONTRACT_VERSION: &str = "ai_daily_worker_v1";
const WORKER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_STREAM_CAPTURE_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct CapturedStream {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

#[derive(Debug, Error)]
pub enum EngineShellError {
    #[error("failed to serialize scanner response: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug)]
pub struct CommandOutput {
    pub payload: Value,
    pub exit_code: i32,
}

impl CommandOutput {
    fn success<T: Serialize>(payload: &T) -> Result<Self, EngineShellError> {
        Ok(Self {
            payload: serde_json::to_value(payload)?,
            exit_code: 0,
        })
    }

    fn with_exit<T: Serialize>(payload: &T, exit_code: i32) -> Result<Self, EngineShellError> {
        Ok(Self {
            payload: serde_json::to_value(payload)?,
            exit_code,
        })
    }
}

pub fn dispatch(command: &str, input: &[u8]) -> Result<CommandOutput, EngineShellError> {
    match command {
        "version" => CommandOutput::success(&version_response()),
        "build-context" => {
            let request = match decode_request::<BuildContextRequest>(input) {
                Ok(request) => request,
                Err(_) => return invalid_request_output(),
            };
            build_context_placeholder(&request)
        }
        "doctor" => {
            let request = match decode_request::<DoctorRequest>(input) {
                Ok(request) => request,
                Err(_) => return invalid_request_output(),
            };
            doctor(&request)
        }
        "inspect-run" => {
            let request = match decode_request::<InspectRunRequest>(input) {
                Ok(request) => request,
                Err(_) => return invalid_request_output(),
            };
            inspect_run_placeholder(&request)
        }
        _ => invalid_request_output(),
    }
}

fn doctor(request: &DoctorRequest) -> Result<CommandOutput, EngineShellError> {
    let mut checks = Vec::new();
    let mut first_error = None;

    match probe_scan_db_parent(&request.scan_db_path) {
        Ok(message) => checks.push(DoctorCheck {
            name: "scan_db_parent".to_string(),
            status: DoctorCheckStatus::Ok,
            message,
        }),
        Err(message) => {
            checks.push(DoctorCheck {
                name: "scan_db_parent".to_string(),
                status: DoctorCheckStatus::Error,
                message: message.clone(),
            });
            first_error = Some(Diagnostic {
                error_code: ErrorCode::CacheOpenFailed,
                message,
                retryable: false,
                stage: DiagnosticStage::Doctor,
                file_path: Nullable(None),
                backend: Nullable(None),
            });
        }
    }

    let mut office = Command::new(&request.adapters.office_worker_path);
    office.arg("version");
    record_handshake(
        &mut checks,
        &mut first_error,
        "office_worker_handshake",
        office,
        WorkerKind::Office,
        &["rust_office_oxide_v1", "rust_xlsx_bounded_v1"],
        &[".docx", ".pptx", ".xlsx"],
        "rust_office_oxide_v1",
    );

    let mut python = Command::new(&request.adapters.python_executable);
    python
        .arg("-m")
        .arg(&request.adapters.python_document_worker_module)
        .arg("version")
        .current_dir(&request.adapters.python_module_root);
    record_handshake(
        &mut checks,
        &mut first_error,
        "python_worker_handshake",
        python,
        WorkerKind::PythonDocument,
        &[
            "pdf_text_v1",
            "python_office_v1",
            "python_sharepoint_text_v1",
        ],
        &[".doc", ".docx", ".pdf", ".ppt", ".pptx", ".xls", ".xlsx"],
        "python_office_v1",
    );

    let version = version_response();
    let status = if first_error.is_some() {
        EngineStatus::Error
    } else {
        EngineStatus::Ok
    };
    let exit_code = if first_error.is_some() { 1 } else { 0 };
    let response = DoctorResponse {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        status,
        engine_version: version.engine_version,
        engine_build: version.engine_build,
        checks,
        warnings: Vec::new(),
        error: Nullable(first_error),
    };
    CommandOutput::with_exit(&response, exit_code)
}

fn probe_scan_db_parent(scan_db_path: &str) -> Result<String, String> {
    let parent = Path::new(scan_db_path)
        .parent()
        .ok_or_else(|| "scan database parent is invalid".to_string())?;
    if !parent.is_dir() {
        return Err("scan database parent is not an accessible directory".to_string());
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is invalid".to_string())?
        .as_nanos();
    let probe_path = parent.join(format!(
        ".ai-daily-scanner-doctor-{}-{nonce}.tmp",
        std::process::id()
    ));
    let mut probe = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)
        .map_err(|_| "scan database parent is not writable".to_string())?;
    let write_result = probe.write_all(b"doctor");
    drop(probe);
    let remove_result = fs::remove_file(&probe_path);
    if write_result.is_err() || remove_result.is_err() {
        return Err("scan database parent capability probe failed".to_string());
    }
    Ok("scan database parent is writable".to_string())
}

#[allow(clippy::too_many_arguments)]
fn record_handshake(
    checks: &mut Vec<DoctorCheck>,
    first_error: &mut Option<Diagnostic>,
    check_name: &str,
    command: Command,
    expected_kind: WorkerKind,
    expected_backends: &[&str],
    expected_extensions: &[&str],
    diagnostic_backend: &str,
) {
    match run_worker_handshake(
        command,
        expected_kind,
        expected_backends,
        expected_extensions,
    ) {
        Ok(()) => checks.push(DoctorCheck {
            name: check_name.to_string(),
            status: DoctorCheckStatus::Ok,
            message: "worker contract accepted".to_string(),
        }),
        Err(message) => {
            checks.push(DoctorCheck {
                name: check_name.to_string(),
                status: DoctorCheckStatus::Error,
                message: message.clone(),
            });
            if first_error.is_none() {
                *first_error = Some(Diagnostic {
                    error_code: ErrorCode::WorkerHandshakeFailed,
                    message,
                    retryable: false,
                    stage: DiagnosticStage::Doctor,
                    file_path: Nullable(None),
                    backend: Nullable(Some(diagnostic_backend.to_string())),
                });
            }
        }
    }
}

fn run_worker_handshake(
    command: Command,
    expected_kind: WorkerKind,
    expected_backends: &[&str],
    expected_extensions: &[&str],
) -> Result<(), String> {
    run_worker_handshake_with_timeout(
        command,
        expected_kind,
        expected_backends,
        expected_extensions,
        WORKER_HANDSHAKE_TIMEOUT,
    )
}

fn run_worker_handshake_with_timeout(
    mut command: Command,
    expected_kind: WorkerKind,
    expected_backends: &[&str],
    expected_extensions: &[&str],
    timeout: Duration,
) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|_| "worker version command could not start".to_string())?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("worker version pipes could not be opened".to_string());
    };
    let stdout_receiver = match spawn_bounded_reader(stdout, "stdout") {
        Ok(receiver) => receiver,
        Err(message) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(message);
        }
    };
    let stderr_receiver = match spawn_bounded_reader(stderr, "stderr") {
        Ok(receiver) => receiver,
        Err(message) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(message);
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = receive_bounded_stream(stdout_receiver, started, timeout, "stdout")?;
                let stderr = receive_bounded_stream(stderr_receiver, started, timeout, "stderr")?;
                if stdout.exceeded_limit {
                    return Err("worker version stdout exceeded the size limit".to_string());
                }
                let output = Output {
                    status,
                    stdout: stdout.bytes,
                    stderr: stderr.bytes,
                };
                return validate_worker_handshake_output(
                    output,
                    expected_kind,
                    expected_backends,
                    expected_extensions,
                );
            }
            Ok(None) if started.elapsed() < timeout => {
                let remaining = timeout.saturating_sub(started.elapsed());
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("worker version command timed out".to_string());
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("worker version command status could not be read".to_string());
            }
        }
    }
}

fn spawn_bounded_reader<R>(
    mut stream: R,
    stream_name: &'static str,
) -> Result<Receiver<Result<CapturedStream, String>>, String>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name(format!("worker-{stream_name}-reader"))
        .spawn(move || {
            let mut bytes = Vec::new();
            let mut exceeded_limit = false;
            let mut buffer = [0_u8; 8192];
            loop {
                let read = match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(_) => {
                        let _ = sender.send(Err(format!(
                            "worker version {stream_name} could not be read"
                        )));
                        return;
                    }
                };
                let remaining = WORKER_STREAM_CAPTURE_LIMIT.saturating_sub(bytes.len());
                let captured = remaining.min(read);
                bytes.extend_from_slice(&buffer[..captured]);
                exceeded_limit |= captured < read;
            }
            let _ = sender.send(Ok(CapturedStream {
                bytes,
                exceeded_limit,
            }));
        })
        .map_err(|_| format!("worker version {stream_name} reader could not start"))?;
    Ok(receiver)
}

fn receive_bounded_stream(
    receiver: Receiver<Result<CapturedStream, String>>,
    started: Instant,
    timeout: Duration,
    stream_name: &str,
) -> Result<CapturedStream, String> {
    let remaining = timeout.saturating_sub(started.elapsed());
    match receiver.recv_timeout(remaining) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err("worker version command timed out".to_string()),
        Err(RecvTimeoutError::Disconnected) => Err(format!(
            "worker version {stream_name} reader stopped unexpectedly"
        )),
    }
}

fn validate_worker_handshake_output(
    output: Output,
    expected_kind: WorkerKind,
    expected_backends: &[&str],
    expected_extensions: &[&str],
) -> Result<(), String> {
    if !output.status.success() {
        return Err("worker version command returned nonzero".to_string());
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| "worker version stdout is not UTF-8".to_string())?;
    let response: WorkerVersionResponse = serde_json::from_str(stdout)
        .map_err(|_| "worker version stdout is not one strict JSON response".to_string())?;
    response.validate()?;
    if response.worker_kind != expected_kind
        || response.worker_contract_version != WORKER_CONTRACT_VERSION
        || response.worker_version != env!("CARGO_PKG_VERSION")
        || !contains_every(&response.supported_backends, expected_backends)
        || !contains_every(&response.supported_extensions, expected_extensions)
    {
        return Err("worker version identity or capabilities mismatch".to_string());
    }
    Ok(())
}

fn contains_every(actual: &[String], required: &[&str]) -> bool {
    required
        .iter()
        .all(|required_item| actual.iter().any(|item| item == required_item))
}

fn inspect_run_placeholder(request: &InspectRunRequest) -> Result<CommandOutput, EngineShellError> {
    let response = InspectRunResponse {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        scan_run_id: request.scan_run_id,
        context_run_id: Nullable(None),
        status: InspectStatus::Error,
        run_status: Nullable(None),
        summary: empty_summary(),
        stage_metrics: Vec::new(),
        extension_metrics: Vec::new(),
        files: Vec::new(),
        decisions: Vec::new(),
        warnings: Vec::new(),
        error: Nullable(Some(Diagnostic {
            error_code: ErrorCode::NotImplemented,
            message: "inspect-run is not implemented in Task 4".to_string(),
            retryable: false,
            stage: DiagnosticStage::Inspect,
            file_path: Nullable(None),
            backend: Nullable(None),
        })),
    };
    CommandOutput::with_exit(&response, 1)
}

fn build_context_placeholder(
    request: &BuildContextRequest,
) -> Result<CommandOutput, EngineShellError> {
    let version = version_response();
    let response = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        engine_version: version.engine_version,
        engine_build: version.engine_build,
        status: EngineStatus::Error,
        file_context: String::new(),
        summary: empty_summary(),
        scan_run_id: Nullable(None),
        context_run_id: Nullable(None),
        warnings: Vec::new(),
        error: Nullable(Some(Diagnostic {
            error_code: ErrorCode::NotImplemented,
            message: "build-context is not implemented in Task 4".to_string(),
            retryable: false,
            stage: DiagnosticStage::Context,
            file_path: Nullable(None),
            backend: Nullable(None),
        })),
    };
    CommandOutput::with_exit(&response, 1)
}

fn empty_summary() -> ContextSummary {
    ContextSummary {
        source_file_count: 0,
        success_count: 0,
        timeout_count: 0,
        included_file_count: 0,
        omitted_file_count: 0,
        error_file_count: 0,
        input_chars: 0,
        output_chars: 0,
        total_duration_ms: 0,
        discovery_duration_ms: 0,
        parse_duration_ms: 0,
        compression_duration_ms: 0,
    }
}

fn decode_request<T>(input: &[u8]) -> Result<T, String>
where
    T: DeserializeOwned + Validate,
{
    let request: T = serde_json::from_slice(input).map_err(|error| error.to_string())?;
    request.validate()?;
    Ok(request)
}

pub fn invalid_request_output() -> Result<CommandOutput, EngineShellError> {
    let response = TransportErrorResponse {
        contract: "ai_daily_transport".to_string(),
        protocol_version: 1,
        status: "error".to_string(),
        error: Diagnostic {
            error_code: ErrorCode::InvalidRequest,
            message: "command request could not be decoded".to_string(),
            retryable: false,
            stage: DiagnosticStage::Request,
            file_path: Nullable(None),
            backend: Nullable(None),
        },
    };
    CommandOutput::with_exit(&response, 2)
}

pub fn version_response() -> VersionResponse {
    VersionResponse {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        binary_name: "ai-daily-scanner".to_string(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        engine_build: option_env!("AI_DAILY_ENGINE_BUILD")
            .unwrap_or("dev-scanner-engine")
            .to_string(),
        target_triple: target_triple(),
        supported_commands: vec![
            "version".to_string(),
            "doctor".to_string(),
            "build-context".to_string(),
            "inspect-run".to_string(),
        ],
        office_worker_contract_version: WORKER_CONTRACT_VERSION.to_string(),
        python_worker_contract_version: WORKER_CONTRACT_VERSION.to_string(),
    }
}

fn target_triple() -> String {
    let arch = std::env::consts::ARCH;
    if cfg!(all(target_os = "windows", target_env = "msvc")) {
        format!("{arch}-pc-windows-msvc")
    } else if cfg!(target_os = "windows") {
        format!("{arch}-pc-windows-gnu")
    } else if cfg!(target_os = "macos") {
        format!("{arch}-apple-darwin")
    } else {
        format!("{arch}-unknown-linux-gnu")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_daily_scanner_contract::Validate;
    use tempfile::tempdir;

    #[test]
    fn scanner_version_uses_the_frozen_command_order() {
        let response = version_response();

        response.validate().expect("version response must be valid");
        assert_eq!(response.binary_name, "ai-daily-scanner");
        assert_eq!(response.supported_commands[0], "version");
    }

    #[test]
    fn db_parent_probe_does_not_create_the_scan_database() {
        let directory = tempdir().expect("temporary directory should be created");
        let scan_db = directory.path().join("scan-index-v2.sqlite3");

        probe_scan_db_parent(scan_db.to_str().expect("path should be UTF-8"))
            .expect("temporary directory should be writable");

        assert!(!scan_db.exists());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("temporary directory should remain readable")
                .count(),
            0
        );
    }

    #[test]
    fn worker_handshake_has_a_bounded_deadline() {
        let mut command =
            Command::new(std::env::current_exe().expect("test executable should exist"));
        command
            .args([
                "--exact",
                "run::tests::handshake_sleep_helper",
                "--nocapture",
            ])
            .env("AI_DAILY_HANDSHAKE_SLEEP_HELPER", "1");
        let started = std::time::Instant::now();

        let error = run_worker_handshake_with_timeout(
            command,
            WorkerKind::PythonDocument,
            &["pdf_text_v1"],
            &[".pdf"],
            std::time::Duration::from_millis(50),
        )
        .expect_err("sleeping worker must hit the deadline");

        assert_eq!(error, "worker version command timed out");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn worker_handshake_drains_large_output_before_exit() {
        let mut command =
            Command::new(std::env::current_exe().expect("test executable should exist"));
        command
            .args([
                "--exact",
                "run::tests::handshake_large_output_helper",
                "--nocapture",
            ])
            .env("AI_DAILY_HANDSHAKE_LARGE_OUTPUT_HELPER", "1");
        let started = std::time::Instant::now();

        let error = run_worker_handshake_with_timeout(
            command,
            WorkerKind::PythonDocument,
            &["pdf_text_v1"],
            &[".pdf"],
            std::time::Duration::from_millis(500),
        )
        .expect_err("synthetic output is intentionally not valid JSON");

        assert_ne!(error, "worker version command timed out");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn handshake_sleep_helper() {
        if std::env::var_os("AI_DAILY_HANDSHAKE_SLEEP_HELPER").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    #[test]
    fn handshake_large_output_helper() {
        if std::env::var_os("AI_DAILY_HANDSHAKE_LARGE_OUTPUT_HELPER").is_some() {
            let chunk = vec![b'x'; 2 * 1024 * 1024];
            std::io::stdout()
                .write_all(&chunk)
                .expect("stdout write should succeed");
            std::io::stderr()
                .write_all(&chunk)
                .expect("stderr write should succeed");
        }
    }
}
