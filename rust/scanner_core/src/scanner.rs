//! In-process scanner interface.
//!
//! Callers use [`Scanner`] and receive typed results without a transport layer.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ai_daily_scanner_contract::{
    AdapterPaths, BuildContextRequest, CompressionProfile, ContextEnvelope, DoctorRequest,
    DoctorResponse, Nullable, ReportMode, ScannerEvidence, ScannerSettings, Validate,
};
use serde::Serialize;
use thiserror::Error;

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

use crate::evidence::assemble_scanner_evidence;
use crate::parsers::{RegisteredWorker, WorkerCommand};
use crate::run::{build_context_command, doctor_command, EngineShellError};
use crate::session::{SessionParams, WorkerPool};
use crate::store::ScannerStore;

/// One context build and the complete evidence committed for that run.
#[derive(Debug, Serialize)]
pub struct ContextResult {
    pub envelope: ContextEnvelope,
    pub evidence: Option<ScannerEvidence>,
}

/// Stable configuration owned by a scanner instance rather than every run.
#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub work_dir: String,
    pub scan_db_path: String,
    pub scanner_settings: ScannerSettings,
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
    #[error("native scanner initialization failed: {0}")]
    Initialization(crate::store::StoreError),
    #[error("another build_context call is already active")]
    Busy,
}

/// A completed scanner operation and its stable success/error status.
#[derive(Debug)]
pub struct ScannerOperation<T> {
    pub value: T,
    pub exit_code: i32,
}

/// The scanner's sole in-process interface.
///
pub struct Scanner {
    config: ScannerConfig,
    build_lock: Mutex<()>,
    resources: Mutex<ScannerResources>,
}

pub(crate) struct ScannerResources {
    pub(crate) store: ScannerStore,
    pub(crate) worker_pools: ScannerWorkerPools,
}

#[derive(Default)]
pub(crate) struct ScannerWorkerPools {
    office: Option<Arc<WorkerPool>>,
    python_document: Option<Arc<WorkerPool>>,
}

impl ScannerWorkerPools {
    pub(crate) fn registered_workers(
        &self,
        office_command: &WorkerCommand,
        python_command: &WorkerCommand,
    ) -> Option<(RegisteredWorker, RegisteredWorker)> {
        Some((
            self.office.as_ref()?.registered_worker(office_command)?,
            self.python_document
                .as_ref()?
                .registered_worker(python_command)?,
        ))
    }

    pub(crate) fn resolve(
        &mut self,
        office_command: WorkerCommand,
        office_worker: RegisteredWorker,
        python_command: WorkerCommand,
        python_worker: RegisteredWorker,
        params: SessionParams,
    ) -> (Arc<WorkerPool>, Arc<WorkerPool>) {
        let office_hello = crate::parsers::worker_hello_from_registered(&office_worker);
        let python_hello = crate::parsers::worker_hello_from_registered(&python_worker);
        let office = resolve_pool(
            &mut self.office,
            office_command,
            office_hello,
            office_worker,
            params,
        );
        let python_document = resolve_pool(
            &mut self.python_document,
            python_command,
            python_hello,
            python_worker,
            params,
        );
        (office, python_document)
    }
}

fn resolve_pool(
    slot: &mut Option<Arc<WorkerPool>>,
    command: WorkerCommand,
    hello: ai_daily_worker_contract::WorkerHello,
    worker: RegisteredWorker,
    params: SessionParams,
) -> Arc<WorkerPool> {
    if let Some(pool) = slot {
        if pool.matches(&command, &hello, &worker, params) {
            return Arc::clone(pool);
        }
    }
    let pool = WorkerPool::new(
        command,
        hello,
        worker,
        params,
        crate::process::WorkerRssTracker::default(),
    );
    *slot = Some(Arc::clone(&pool));
    pool
}

impl Scanner {
    pub fn open(config: ScannerConfig) -> Result<Self, ScannerError> {
        validate_config(&config)?;
        let store = ScannerStore::open(Path::new(&config.scan_db_path))
            .map_err(ScannerError::Initialization)?;
        Ok(Self {
            config,
            build_lock: Mutex::new(()),
            resources: Mutex::new(ScannerResources {
                store,
                worker_pools: ScannerWorkerPools::default(),
            }),
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
        let mut resources = self
            .resources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let operation = build_context_command(&wire_request, &mut resources);
        let evidence = match operation.value.scan_run_id.0 {
            Some(scan_run_id) => Some(load_run_evidence(
                &mut resources.store,
                &wire_request,
                scan_run_id,
            )?),
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
        Ok(doctor_command(&request))
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
            scanner_settings: self.config.scanner_settings.clone(),
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
            scanner_settings: request.scanner_settings.clone(),
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
        .scanner_settings
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
    store: &mut ScannerStore,
    request: &BuildContextRequest,
    scan_run_id: u64,
) -> Result<ScannerEvidence, EngineShellError> {
    let snapshot = store
        .load_evidence(scan_run_id)
        .map_err(|error| EngineShellError::Evidence(error.error.to_string()))?;
    assemble_scanner_evidence(&request.request_id, scan_run_id, &snapshot)
        .map_err(EngineShellError::Evidence)
}
