//! In-process scanner interface.
//!
//! The command-line process is a temporary adapter around this module.  New
//! callers should use [`Scanner`] and receive typed results instead of learning
//! the JSON/stdin process protocol.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ai_daily_scanner_contract::{
    AdapterPaths, BuildContextRequest, CompressionProfile, ContextEnvelope, DoctorRequest,
    DoctorResponse, InspectRunRequest, InspectRunResponseV2, Nullable, ReportMode, ScannerProfile,
    Validate,
};
use serde::Serialize;
use thiserror::Error;

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

use crate::inspect::assemble_inspect_v2;
use crate::run::{build_context_command, doctor_command, CommandOutput, EngineShellError};
use crate::store::ScannerStore;

/// One context build and the complete evidence committed for that run.
#[derive(Debug, Serialize)]
pub struct ContextResult {
    pub envelope: ContextEnvelope,
    pub evidence: Option<InspectRunResponseV2>,
}

/// Stable configuration owned by a scanner instance rather than every run.
#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub work_dir: String,
    pub scan_db_path: String,
    pub scanner_profile: ScannerProfile,
    pub adapters: AdapterPaths,
}

/// The only per-run input accepted by the in-process scanner interface.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub report_mode: ReportMode,
    pub start_date: String,
    pub end_date: String,
    pub compression_profile: Option<CompressionProfile>,
}

#[derive(Debug, Error)]
pub enum ScannerError {
    #[error("invalid scanner configuration: {0}")]
    InvalidConfiguration(String),
    #[error("scanner operation failed: {0}")]
    Operation(#[from] EngineShellError),
    #[error("another build_context call is already active")]
    Busy,
}

/// A completed scanner operation and its legacy command exit semantics.
#[derive(Debug)]
pub struct ScannerOperation<T> {
    pub value: T,
    pub exit_code: i32,
}

impl<T> ScannerOperation<T> {
    fn from_command(output: CommandOutput) -> Result<Self, EngineShellError>
    where
        T: serde::de::DeserializeOwned,
    {
        Ok(Self {
            value: serde_json::from_str(&output.json)?,
            exit_code: output.exit_code,
        })
    }
}

/// The scanner's sole in-process interface.
///
/// Configuration is still carried by the frozen request while the legacy CLI
/// adapter exists.  A later native-only step moves stable runtime configuration
/// into `Scanner::open` without changing this seam again for callers.
#[derive(Debug)]
pub struct Scanner {
    config: ScannerConfig,
    build_lock: Mutex<()>,
}

impl Scanner {
    pub fn open(config: ScannerConfig) -> Result<Self, ScannerError> {
        validate_config(&config)?;
        Ok(Self {
            config,
            build_lock: Mutex::new(()),
        })
    }

    pub fn build_context(
        &self,
        request: &ScanRequest,
    ) -> Result<ScannerOperation<ContextResult>, ScannerError> {
        self.build_context_with_request_id(request, new_request_id())
    }

    pub fn build_context_with_request_id(
        &self,
        request: &ScanRequest,
        request_id: String,
    ) -> Result<ScannerOperation<ContextResult>, ScannerError> {
        let _guard = self.build_lock.try_lock().map_err(|_| ScannerError::Busy)?;
        let wire_request = self.build_request(request, request_id)?;
        let operation: ScannerOperation<ContextEnvelope> =
            ScannerOperation::from_command(build_context_command(&wire_request)?)?;
        let evidence = match operation.value.scan_run_id.0 {
            Some(scan_run_id) => Some(load_run_evidence(&wire_request, scan_run_id)?),
            None => None,
        };
        Ok(ScannerOperation {
            value: ContextResult {
                envelope: operation.value,
                evidence,
            },
            exit_code: operation.exit_code,
        })
    }

    pub fn doctor(&self) -> Result<ScannerOperation<DoctorResponse>, ScannerError> {
        self.doctor_with_request_id(new_request_id())
    }

    pub fn doctor_with_request_id(
        &self,
        request_id: String,
    ) -> Result<ScannerOperation<DoctorResponse>, ScannerError> {
        let request = DoctorRequest {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id,
            scan_db_path: self.config.scan_db_path.clone(),
            adapters: self.config.adapters.clone(),
        };
        Ok(ScannerOperation::from_command(doctor_command(&request)?)?)
    }

    fn build_request(
        &self,
        request: &ScanRequest,
        request_id: String,
    ) -> Result<BuildContextRequest, ScannerError> {
        let wire_request = BuildContextRequest {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id,
            work_dir: self.config.work_dir.clone(),
            start_date: request.start_date.clone(),
            end_date: request.end_date.clone(),
            report_mode: request.report_mode,
            compression_profile: Nullable(request.compression_profile),
            scan_db_path: self.config.scan_db_path.clone(),
            scanner_profile: self.config.scanner_profile.clone(),
            adapters: self.config.adapters.clone(),
        };
        wire_request
            .validate()
            .map_err(ScannerError::InvalidConfiguration)?;
        Ok(wire_request)
    }
}

impl ScannerConfig {
    pub fn from_build_request(request: &BuildContextRequest) -> Self {
        Self {
            work_dir: request.work_dir.clone(),
            scan_db_path: request.scan_db_path.clone(),
            scanner_profile: request.scanner_profile.clone(),
            adapters: request.adapters.clone(),
        }
    }
}

impl ScanRequest {
    pub fn from_build_request(request: &BuildContextRequest) -> Self {
        Self {
            report_mode: request.report_mode,
            start_date: request.start_date.clone(),
            end_date: request.end_date.clone(),
            compression_profile: request.compression_profile.0,
        }
    }
}

fn validate_config(config: &ScannerConfig) -> Result<(), ScannerError> {
    config
        .scanner_profile
        .validate()
        .map_err(ScannerError::InvalidConfiguration)?;
    config
        .adapters
        .validate()
        .map_err(ScannerError::InvalidConfiguration)?;
    if !Path::new(&config.work_dir).is_absolute() {
        return Err(ScannerError::InvalidConfiguration(
            "work_dir must be absolute".to_string(),
        ));
    }
    if !Path::new(&config.scan_db_path).is_absolute() {
        return Err(ScannerError::InvalidConfiguration(
            "scan_db_path must be absolute".to_string(),
        ));
    }
    Ok(())
}

fn new_request_id() -> String {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed) as u128;
    let process = std::process::id() as u128;
    let mut value = time ^ (process << 64) ^ sequence;
    // RFC 4122 variant plus version 4 are sufficient for the internal
    // idempotency key; unpredictability is not a security property here.
    value = (value & !(0xf_u128 << 76)) | (4_u128 << 76);
    value = (value & !(0x3_u128 << 62)) | (0x2_u128 << 62);
    let hex = format!("{value:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn load_run_evidence(
    request: &BuildContextRequest,
    scan_run_id: u64,
) -> Result<InspectRunResponseV2, EngineShellError> {
    let inspect_request = InspectRunRequest {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        scan_db_path: request.scan_db_path.clone(),
        scan_run_id,
        include_content: false,
    };
    let mut store = ScannerStore::open_existing(Path::new(&request.scan_db_path))
        .map_err(|error| EngineShellError::Evidence(error.to_string()))?;
    let snapshot = store
        .inspect_run(scan_run_id, false)
        .map_err(|error| EngineShellError::Evidence(error.error.to_string()))?;
    assemble_inspect_v2(&inspect_request, &snapshot).map_err(EngineShellError::Evidence)
}
