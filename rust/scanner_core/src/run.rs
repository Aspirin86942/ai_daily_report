use ai_daily_discovery::{
    discover_files_until, normalize_contract_path_text, DiscoveredFileOut, DiscoveryIssue,
    DiscoveryReport, DiscoveryRequest,
};
use ai_daily_scanner_contract::{
    BuildContextRequest, CacheMissReason, CacheStatus, ContextAction, ContextDecision,
    ContextEnvelope, ContextSummary, Diagnostic, DiagnosticStage, DoctorCheck, DoctorCheckStatus,
    DoctorRequest, DoctorResponse, EngineStatus, ErrorCode, NormalizedScannerSettings, Nullable,
    ParseStatus, RunStatus, StageMetric, StageName,
};
use chrono::NaiveDate;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::artifact::{
    snapshot_key_parts, ArtifactDecisionRow, ArtifactDraft, ArtifactFileRow, ClassifierIdentity,
    PdfClassificationProvenanceV1, SemanticSummary, CLASSIFIER_CONTRACT_VERSION,
};
use crate::config::normalize_scanner_settings;
use crate::context_audit::{context_profile_hash, rejected_profile_hash};
use crate::identity::{engine_identity, EngineIdentity};
use crate::parsers::classifier::ClassifierPort;
use crate::parsers::{
    document, office, register_worker, register_worker_pair_observed, RegisteredWorker,
    WorkerCommand, WORKER_HANDSHAKE_TIMEOUT,
};
use crate::process::WorkerRssTracker;
use crate::scanner::{ScannerOperation, ScannerResources, ScannerWorkerPools};
use crate::scheduler::{
    BudgetedContextScheduler, BudgetedScanOutcome, Clock, RealClock, RealGuardVerifier,
    RunDeadlines, ScheduledRunInput, TerminalIntent, WorkerIdentities,
};
use crate::scheduler_adapter::{ProductionParser, StoreCachePort};
use crate::source_guard::{
    source_guard_kind_from_text, source_guard_kind_text, SourceGuardKind,
    SourceGuardObservationMetrics, SourceGuardObserver, SourceGuardV2,
};
use crate::store::{
    canonical_envelope_json, current_time_millis, ActiveRun, AttemptRuntime, BeginRunOutcome,
    ContextDecisionRecord, ContextRunRecord, DiagnosticSeverity, FileResultRecord,
    FinalizationBatch, InventoryRecord, RouteStackFingerprint, RouteStackFingerprints,
    RunDiagnosticRecord, ScannerStore, SnapshotHit, SnapshotHitRef, StoreError,
    TerminalAuditTimings, WorkerFingerprint, HEARTBEAT_INTERVAL_MS,
};

#[derive(Debug, Error)]
pub enum EngineShellError {
    #[error("failed to assemble scanner evidence: {0}")]
    Evidence(String),
}

#[derive(Debug, Clone)]
struct ActiveRunTiming {
    clock: RealClock,
    deadlines: RunDeadlines,
    source_guards: SourceGuardObserver,
}

impl ActiveRunTiming {
    fn start(total_deadline_ms: u64) -> Result<Self, String> {
        let clock = RealClock::new();
        let deadlines = RunDeadlines::derive(total_deadline_ms, &clock)?;
        Ok(Self {
            clock,
            deadlines,
            source_guards: SourceGuardObserver::default(),
        })
    }

    fn remaining_work_ms(&self) -> u64 {
        self.deadlines.remaining_to_work_deadline(&self.clock)
    }

    fn remaining_absolute_ms(&self) -> u64 {
        self.deadlines.remaining_to_absolute_deadline(&self.clock)
    }
}

pub(crate) fn doctor_command(request: &DoctorRequest) -> ScannerOperation<DoctorResponse> {
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
        "rust_office_oxide_v2",
    );

    record_handshake(
        &mut checks,
        &mut first_error,
        "pdf_worker_handshake",
        document::worker_command(&request.adapters),
        "rust_pdfium_v3",
    );

    let identity = engine_identity();
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
        engine_version: identity.engine_version,
        engine_build: identity.engine_build,
        checks,
        warnings: Vec::new(),
        error: Nullable(first_error),
    };
    ScannerOperation {
        value: response,
        exit_code,
    }
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

pub(crate) fn build_context_command(
    request: &BuildContextRequest,
    resources: &mut ScannerResources,
) -> ScannerOperation<ContextEnvelope> {
    let started_at = Instant::now();
    resources.sources.clear();
    let version = engine_identity();
    let ScannerResources {
        store,
        worker_pools,
        sources,
    } = resources;
    let early_error = |error, scan_run_id| {
        build_error_output(
            request,
            &version,
            error,
            Vec::new(),
            ContextSummary::default(),
            scan_run_id,
        )
    };
    let prepared = match preparation::prepare_build(request) {
        Ok(prepared) => prepared,
        Err(error) => return early_error(error, None),
    };
    let profile = &prepared.profile;
    let active = match prepared.begin(store, &request.request_id) {
        Ok(BeginRunOutcome::Stored(stored)) => {
            let exit_code = i32::from(stored.envelope.status == EngineStatus::Error);
            return ScannerOperation {
                value: stored.envelope,
                exit_code,
            };
        }
        Ok(BeginRunOutcome::Started(active)) => active,
        Err(error) => return early_error(error, None),
    };
    // The only monotonic origin for this run is created immediately after the
    // begin_run COMMIT. Handshake/discovery time must consume this same budget.
    let mut timing = match ActiveRunTiming::start(profile.total_deadline_ms) {
        Ok(timing) => timing,
        Err(message) => {
            let error = match abandon_run(store, &active) {
                Ok(()) => internal_error(message, DiagnosticStage::Internal),
                Err(abandon) => abandon.diagnostic(DiagnosticStage::Cache),
            };
            return early_error(error, Some(active.scan_run_id()));
        }
    };
    let heartbeat = LeaseHeartbeat::start(PathBuf::from(&request.scan_db_path), active.clone());
    if request.window_start_utc.is_some() {
        timing.source_guards = SourceGuardObserver::strict_with_limits(
            profile.execution.max_file_size_bytes,
            Instant::now() + Duration::from_millis(timing.remaining_work_ms()),
        );
    }
    ActiveBuild {
        run: RunAttempt {
            request,
            version: &version,
            store,
            active: &active,
            timing: &timing,
        },
        heartbeat,
        started_at,
        rss_tracker: WorkerRssTracker::default(),
        handshake_ms: 0,
    }
    .execute(profile, &prepared.work_dir, worker_pools, sources)
}

struct RunAttempt<'a> {
    request: &'a BuildContextRequest,
    version: &'a EngineIdentity,
    store: &'a mut ScannerStore,
    active: &'a ActiveRun,
    timing: &'a ActiveRunTiming,
}

struct ActiveBuild<'a> {
    run: RunAttempt<'a>,
    heartbeat: LeaseHeartbeat,
    started_at: Instant,
    rss_tracker: WorkerRssTracker,
    handshake_ms: u64,
}

fn abandon_run(store: &mut ScannerStore, active: &ActiveRun) -> Result<(), StoreError> {
    current_time_millis().and_then(|now| store.abandon_active_run(active, now))
}

struct LeaseHeartbeat {
    stop_sender: mpsc::Sender<()>,
    handle: Option<thread::JoinHandle<Option<StoreError>>>,
}

impl LeaseHeartbeat {
    fn start(database_path: PathBuf, active: ActiveRun) -> Self {
        let (stop_sender, stop_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut store = None;
            let mut renew = || {
                let store = match &mut store {
                    Some(opened) => opened,
                    None => store.insert(ScannerStore::open_existing(&database_path)?),
                };
                store.heartbeat(&active, current_time_millis()?)
            };
            let mut last_error = None;
            while let Err(mpsc::RecvTimeoutError::Timeout) =
                stop_receiver.recv_timeout(Duration::from_millis(HEARTBEAT_INTERVAL_MS))
            {
                last_error = renew().err();
            }
            last_error
        });
        Self {
            stop_sender,
            handle: Some(handle),
        }
    }

    fn stop(&mut self) -> Option<StoreError> {
        let handle = self.handle.take()?;
        let _ = self.stop_sender.send(());
        handle.join().unwrap_or_else(|_| {
            Some(StoreError::CacheWrite {
                detail: "lease heartbeat thread panicked".to_string(),
            })
        })
    }

    fn stop_with_warning(&mut self, warnings: &mut Vec<Diagnostic>) {
        if let Some(error) = self.stop() {
            warnings.push(diagnostic(
                ErrorCode::CacheWriteFailed,
                format!("lease heartbeat recovered after a transient failure: {error}"),
                true,
                DiagnosticStage::Cache,
            ));
        }
    }
}

impl Drop for LeaseHeartbeat {
    fn drop(&mut self) {
        self.stop();
    }
}

fn diagnostic(
    error_code: ErrorCode,
    message: String,
    retryable: bool,
    stage: DiagnosticStage,
) -> Diagnostic {
    Diagnostic {
        error_code,
        message: truncate_chars(&message, 4_096),
        retryable,
        stage,
        file_path: Nullable(None),
        backend: Nullable(None),
    }
}

fn internal_error(message: String, stage: DiagnosticStage) -> Diagnostic {
    diagnostic(ErrorCode::InternalError, message, false, stage)
}

fn run_timestamp() -> Result<u64, Diagnostic> {
    current_time_millis().map_err(|error| error.diagnostic(DiagnosticStage::Internal))
}

fn stage_deadline_exhausted(operation: &str) -> Diagnostic {
    diagnostic(
        ErrorCode::StageDeadlineExhausted,
        format!("work deadline exhausted before {operation}"),
        true,
        DiagnosticStage::Process,
    )
}

fn elapsed_summary(started_at: Instant) -> ContextSummary {
    ContextSummary {
        total_duration_ms: elapsed_ms(started_at),
        ..Default::default()
    }
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests;

mod outcome;
mod stages;
#[cfg(test)]
use outcome::artifact_file_rows;
use outcome::{
    apply_worker_rss_observation, assemble_scheduler_execution_metrics,
    assemble_snapshot_execution_metrics, base_execution_metrics, build_batch_artifact,
    build_error_context_record, combine_peak_rss, scheduler_outcome_envelope,
    snapshot_file_results, snapshot_inventory, worker_rss_unavailable_diagnostic,
};
mod preparation;
#[cfg(test)]
use preparation::discovery_timeout_diagnostic;
use preparation::{
    artifact_guards_are_current, attach_source_guards, discover_with_timeout,
    discovery_guards_are_current, discovery_issue_diagnostic, route_stack_fingerprints,
    split_handshake, worker_fingerprint,
};
mod persistence;
mod snapshot;
use persistence::build_error_output;
#[cfg(test)]
use persistence::{complete_committed_terminal, persist_post_outcome_failure_if_invalid};
