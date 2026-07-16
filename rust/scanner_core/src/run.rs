use ai_daily_scanner_contract::{
    BuildContextRequest, ContextEnvelope, ContextSummary, Diagnostic, DiagnosticStage, DoctorCheck,
    DoctorCheckStatus, DoctorRequest, DoctorResponse, EngineStatus, ErrorCode, InspectRunRequest,
    InspectRunResponse, InspectStatus, Nullable, TransportErrorResponse, Validate, VersionResponse,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::parsers::{
    document, office, register_worker, WorkerCommand, WORKER_CONTRACT_VERSION,
    WORKER_HANDSHAKE_TIMEOUT,
};

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

    record_handshake(
        &mut checks,
        &mut first_error,
        "office_worker_handshake",
        office::worker_command(&request.adapters),
        "rust_office_oxide_v1",
    );

    record_handshake(
        &mut checks,
        &mut first_error,
        "python_worker_handshake",
        document::worker_command(&request.adapters),
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
    command: WorkerCommand,
    diagnostic_backend: &str,
) {
    match register_worker(&command, WORKER_HANDSHAKE_TIMEOUT) {
        Ok(_) => checks.push(DoctorCheck {
            name: check_name.to_string(),
            status: DoctorCheckStatus::Ok,
            message: "worker contract accepted".to_string(),
        }),
        Err(failure) => {
            let message = failure.diagnostic.message;
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
        engine_build: env!("AI_DAILY_ENGINE_BUILD").to_string(),
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
    fn local_scanner_build_uses_a_deterministic_source_fingerprint() {
        let build = version_response().engine_build;
        if let Some(ci_build) =
            option_env!("AI_DAILY_BUILD_ID").filter(|value| !value.trim().is_empty())
        {
            assert_eq!(build, ci_build);
        } else {
            let digest = build
                .strip_prefix("sha256-source-v1:")
                .expect("local build must use the source hash prefix");
            assert_eq!(digest.len(), 64);
            assert!(digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        }
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
}
