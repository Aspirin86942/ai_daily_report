use ai_daily_discovery::{
    discover_files_with_diagnostics, normalize_contract_path_text, DiscoveredFileOut,
    DiscoveryIssue, DiscoveryReport, DiscoveryRequest,
};
use ai_daily_scanner_contract::{
    AuditWorkerLane, BuildContextRequest, CacheMissReason, CacheStatus, ContextAction,
    ContextDecision, ContextEnvelope, ContextSummary, Diagnostic, DiagnosticStage, DoctorCheck,
    DoctorCheckStatus, DoctorRequest, DoctorResponse, EngineStatus, ErrorCode, Nullable,
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
        "python_worker_handshake",
        document::worker_command(&request.adapters),
        "python_office_v2",
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
    let version = engine_identity();
    let work_dir = match validate_build_work_dir(&request.work_dir) {
        Ok(path) => path,
        Err(error) => {
            return build_error_output(request, &version, error, Vec::new(), empty_summary(), None);
        }
    };
    let profile = match normalize_scanner_settings(&request.scanner_settings, request.report_mode) {
        Ok(profile) => profile,
        Err(message) => {
            return build_error_output(
                request,
                &version,
                diagnostic(
                    ErrorCode::InvalidRequest,
                    message,
                    false,
                    DiagnosticStage::Request,
                ),
                Vec::new(),
                empty_summary(),
                None,
            );
        }
    };
    let normalized_settings =
        match normalize_scanner_settings(&request.scanner_settings, request.report_mode) {
            Ok(profile) => profile,
            Err(message) => {
                return build_error_output(
                    request,
                    &version,
                    diagnostic(
                        ErrorCode::InvalidRequest,
                        message,
                        false,
                        DiagnosticStage::Request,
                    ),
                    Vec::new(),
                    empty_summary(),
                    None,
                );
            }
        };
    let canonical = match ScannerStore::canonicalize_request(request, &profile) {
        Ok(canonical) => canonical,
        Err(error) => {
            return build_error_output(
                request,
                &version,
                error.diagnostic(DiagnosticStage::Request),
                Vec::new(),
                empty_summary(),
                None,
            );
        }
    };
    let runtime = match AttemptRuntime::from_request(request) {
        Ok(runtime) => runtime,
        Err(error) => {
            return build_error_output(
                request,
                &version,
                error.diagnostic(DiagnosticStage::Request),
                Vec::new(),
                empty_summary(),
                None,
            );
        }
    };
    let ScannerResources {
        store,
        worker_pools,
    } = resources;
    let now_ms = match current_time_millis() {
        Ok(value) => value,
        Err(error) => {
            return build_error_output(
                request,
                &version,
                error.diagnostic(DiagnosticStage::Internal),
                Vec::new(),
                empty_summary(),
                None,
            );
        }
    };
    let active = match store.begin_run(&request.request_id, &canonical, &runtime, now_ms) {
        Ok(BeginRunOutcome::Stored(stored)) => {
            let exit_code = i32::from(stored.envelope.status == EngineStatus::Error);
            return ScannerOperation {
                value: stored.envelope,
                exit_code,
            };
        }
        Ok(BeginRunOutcome::Started(active)) => active,
        Err(error) => {
            return build_error_output(
                request,
                &version,
                error.diagnostic(DiagnosticStage::Cache),
                Vec::new(),
                empty_summary(),
                None,
            );
        }
    };
    // The only monotonic origin for this run is created immediately after the
    // begin_run COMMIT. Handshake/discovery time must consume this same budget.
    let timing = match ActiveRunTiming::start(normalized_settings.total_deadline_ms) {
        Ok(timing) => timing,
        Err(message) => {
            let abandon_error = current_time_millis()
                .and_then(|abandon_now| store.abandon_active_run(&active, abandon_now));
            let error = match abandon_error {
                Ok(()) => diagnostic(
                    ErrorCode::InternalError,
                    message,
                    false,
                    DiagnosticStage::Internal,
                ),
                Err(abandon) => abandon.diagnostic(DiagnosticStage::Cache),
            };
            return build_error_output(
                request,
                &version,
                error,
                Vec::new(),
                empty_summary(),
                Some(active.scan_run_id()),
            );
        }
    };
    let mut heartbeat = LeaseHeartbeat::start(PathBuf::from(&request.scan_db_path), active.clone());
    execute_active_build(
        request,
        &version,
        &profile,
        &normalized_settings,
        &work_dir,
        store,
        worker_pools,
        &active,
        &mut heartbeat,
        started_at,
        &timing,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_active_build(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    profile: &ai_daily_scanner_contract::NormalizedScannerSettings,
    normalized_settings: &ai_daily_scanner_contract::NormalizedScannerSettings,
    work_dir: &Path,
    store: &mut ScannerStore,
    worker_pools: &mut ScannerWorkerPools,
    active: &ActiveRun,
    heartbeat: &mut LeaseHeartbeat,
    started_at: Instant,
    timing: &ActiveRunTiming,
) -> ScannerOperation<ContextEnvelope> {
    let rss_tracker = WorkerRssTracker::default();
    // ---- bounded parallel worker handshakes (spec Solution run.rs order) ----
    // A Scanner owns the verified worker identities with its long-lived pools.
    // The first run performs the parallel preflight; later runs reuse those
    // identities and each session still validates its own hello on startup.
    let office_command = office::worker_command(&request.adapters);
    let python_command = document::worker_command(&request.adapters);
    let (office_python_pair, worker_handshake_ms) = if let Some((office, python)) =
        worker_pools.registered_workers(&office_command, &python_command)
    {
        ((Ok(office), Ok(python)), 0)
    } else {
        // One bounded parallel hello batch validates both worker-v2
        // implementations. Pools start lazily on the first operation.
        let remaining_handshake_ms = timing.remaining_work_ms();
        if remaining_handshake_ms == 0 {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                Vec::new(),
                stage_deadline_exhausted("worker handshake"),
                elapsed_summary(started_at),
                0,
                &rss_tracker,
                timing,
            );
        }
        let handshake_timeout =
            WORKER_HANDSHAKE_TIMEOUT.min(Duration::from_millis(remaining_handshake_ms.max(1)));
        let handshake_started = Instant::now();
        let pair = register_worker_pair_observed(
            &office_command,
            &python_command,
            handshake_timeout,
            &rss_tracker,
        );
        (pair, elapsed_ms(handshake_started))
    };
    let (office_result, python_result) = office_python_pair;
    let (office_worker, office_error) = split_handshake(office_result, "rust_office_oxide_v2");
    let (python_worker, python_error) = split_handshake(python_result, "python_office_v2");
    let office_fingerprint = office_worker.as_ref().map(worker_fingerprint);
    let python_fingerprint = python_worker.as_ref().map(worker_fingerprint);
    let fingerprint_now_ms = match current_time_millis() {
        Ok(value) => value,
        Err(error) => {
            heartbeat.stop();
            return build_error_output(
                request,
                version,
                error.diagnostic(DiagnosticStage::Internal),
                Vec::new(),
                elapsed_summary(started_at),
                Some(active.scan_run_id()),
            );
        }
    };
    if let Err(error) = store.record_worker_fingerprints(
        active,
        office_fingerprint.as_ref(),
        python_fingerprint.as_ref(),
        fingerprint_now_ms,
    ) {
        heartbeat.stop();
        return build_error_output(
            request,
            version,
            error.diagnostic(DiagnosticStage::Cache),
            Vec::new(),
            elapsed_summary(started_at),
            Some(active.scan_run_id()),
        );
    }
    let mut handshake_errors: Vec<Diagnostic> =
        [office_error, python_error].into_iter().flatten().collect();
    if !handshake_errors.is_empty() {
        let error = handshake_errors.remove(0);
        return finish_active_error(
            request,
            version,
            store,
            active,
            heartbeat,
            handshake_errors,
            error,
            elapsed_summary(started_at),
            worker_handshake_ms,
            &rss_tracker,
            timing,
        );
    }
    let (Some(office_worker), Some(python_worker)) = (office_worker, python_worker) else {
        return finish_active_error(
            request,
            version,
            store,
            active,
            heartbeat,
            Vec::new(),
            diagnostic(
                ErrorCode::InternalError,
                "worker preflight completed without both identities".to_string(),
                false,
                DiagnosticStage::Process,
            ),
            elapsed_summary(started_at),
            worker_handshake_ms,
            &rss_tracker,
            timing,
        );
    };
    let route_stacks =
        match route_stack_fingerprints(version, profile, &office_worker, &python_worker) {
            Ok(value) => value,
            Err(message) => {
                return finish_active_error(
                    request,
                    version,
                    store,
                    active,
                    heartbeat,
                    Vec::new(),
                    diagnostic(
                        ErrorCode::InternalError,
                        message,
                        false,
                        DiagnosticStage::Cache,
                    ),
                    elapsed_summary(started_at),
                    worker_handshake_ms,
                    &rss_tracker,
                    timing,
                );
            }
        };

    // ---- discovery (with engine-owned SourceGuardV2) ----
    let remaining_discovery_ms = timing.remaining_work_ms();
    if remaining_discovery_ms == 0 {
        return finish_active_error(
            request,
            version,
            store,
            active,
            heartbeat,
            Vec::new(),
            stage_deadline_exhausted("discovery"),
            elapsed_summary(started_at),
            worker_handshake_ms,
            &rss_tracker,
            timing,
        );
    }
    let discovery_started = Instant::now();
    let discovery_timeout = Duration::from_millis(
        profile
            .execution
            .discovery_timeout_ms
            .min(remaining_discovery_ms)
            .max(1),
    );
    let mut discovery = match discover_with_timeout(work_dir, request, profile, discovery_timeout) {
        Ok(report) => report,
        Err(error) => {
            let mut summary = elapsed_summary(started_at);
            summary.discovery_duration_ms = elapsed_ms(discovery_started);
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                Vec::new(),
                error,
                summary,
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let mut warnings: Vec<Diagnostic> = discovery
        .issues
        .iter()
        .map(discovery_issue_diagnostic)
        .collect();
    attach_source_guards(&mut discovery.files, &timing.source_guards);
    let discovery_duration_ms = elapsed_ms(discovery_started);

    // ---- assemble + execute the deep-module scheduler ----
    let params = crate::session::SessionParams::from_settings(normalized_settings);
    let (office_pool, python_pool) = worker_pools.resolve(
        office_command,
        office_worker.clone(),
        python_command.clone(),
        python_worker.clone(),
        params,
    );
    let classifier_port = ClassifierPort::new(python_pool.clone());
    let parser_port = ProductionParser::new(profile, python_pool.clone(), office_pool.clone());
    let cache_port = StoreCachePort::new(
        PathBuf::from(&request.scan_db_path),
        route_stacks,
        profile.clone(),
        timing.deadlines,
        timing.clock.clone(),
    );
    let context_profile_hash = match context_profile_hash(1, &version.engine_build, profile) {
        Ok(value) => value,
        Err(message) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                diagnostic(
                    ErrorCode::InternalError,
                    message,
                    false,
                    DiagnosticStage::Context,
                ),
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let rejected_profile_hash = match rejected_profile_hash(1, &version.engine_build, profile) {
        Ok(value) => value,
        Err(message) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                diagnostic(
                    ErrorCode::InternalError,
                    message,
                    false,
                    DiagnosticStage::Cache,
                ),
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let started_at_ms = match current_time_millis() {
        Ok(value) => value,
        Err(error) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                error.diagnostic(DiagnosticStage::Internal),
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let worker_identities = WorkerIdentities {
        office_contract: Some(office_worker.identity.worker_contract_version.clone()),
        office_version: Some(office_worker.identity.worker_version.clone()),
        office_build: Some(office_worker.identity.worker_build.clone()),
        python_contract: Some(python_worker.identity.worker_contract_version.clone()),
        python_version: Some(python_worker.identity.worker_version.clone()),
        python_build: Some(python_worker.identity.worker_build.clone()),
        // carry：worker-v2 hello 的真实 build identity。
        // PDF 被 profile 允许时 hello 已由 preflight fail-closed
        // 保证存在；未允许时没有分类动作，classifier build 保持 None。
        classifier_build: Some(python_worker.identity.worker_build.clone()),
    };

    // Persistence boundary 2: the complete discovery inventory is committed in
    // fixed short transactions before either snapshot or parse/classification
    // cache lookup. Only the completed receipt opens those lookup paths.
    let inventory = match snapshot_inventory(&discovery.files, &request.work_dir) {
        Ok(records) => records,
        Err(message) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                diagnostic(
                    ErrorCode::InternalError,
                    message,
                    false,
                    DiagnosticStage::Cache,
                ),
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let inventory_prepare_started = timing.clock.now_ms();
    let inventory_now_ms = match current_time_millis() {
        Ok(value) => value,
        Err(error) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                error.diagnostic(DiagnosticStage::Internal),
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let scan_run_id = match i64::try_from(active.scan_run_id()) {
        Ok(value) => value,
        Err(_) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                diagnostic(
                    ErrorCode::InvalidRequest,
                    "scan_run_id exceeds SQLite integer range".to_string(),
                    false,
                    DiagnosticStage::Cache,
                ),
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let inventory_existed_before = match store.prepare_inventory_with_deadline(
        &inventory,
        scan_run_id,
        inventory_now_ms,
        timing.deadlines,
        &timing.clock,
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                error.diagnostic(DiagnosticStage::Cache),
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let inventory_prepare_duration_ms = timing
        .clock
        .now_ms()
        .saturating_sub(inventory_prepare_started);

    // ---- snapshot fast path (spec Part 5.4) ----
    // The live worker handshake is kept; a snapshot skips classification/parse
    // lookup + execution, context recompute, and new artifact payload writes.
    // A hit requires a byte-exact `snapshot_key_json` AND at least one committed
    // Success source run referencing the artifact (orphan artifacts never hit).
    let snapshot_lookup_started = Instant::now();
    let classifier_identity = ClassifierIdentity {
        contract: CLASSIFIER_CONTRACT_VERSION.to_string(),
        build: worker_identities
            .classifier_build
            .clone()
            .unwrap_or_else(|| "0".repeat(64)),
        profile_hash: match crate::store::classifier_profile_hash(normalized_settings) {
            Ok(hash) => hash,
            Err(message) => {
                return finish_active_error(
                    request,
                    version,
                    store,
                    active,
                    heartbeat,
                    warnings,
                    diagnostic(
                        ErrorCode::InternalError,
                        message,
                        false,
                        DiagnosticStage::Cache,
                    ),
                    elapsed_summary(started_at),
                    worker_handshake_ms,
                    &rss_tracker,
                    timing,
                );
            }
        },
    };
    let key_parts = match snapshot_key_parts(
        request,
        &discovery.files,
        &discovery.issues,
        normalized_settings,
        &version.engine_build,
        &worker_identities,
        &classifier_identity,
    ) {
        Ok(parts) => parts,
        Err(message) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                diagnostic(
                    ErrorCode::InternalError,
                    message,
                    false,
                    DiagnosticStage::Cache,
                ),
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    // The strict guard span covers the pre-lookup discovery verification and,
    // for a SQL hit, loading + verifying every artifact row before reuse.
    let verified_hit = if discovery_guards_are_current(&discovery.files, &timing.source_guards) {
        let hit = match store.snapshot_lookup(&key_parts) {
            Ok(hit) => hit,
            Err(error) => {
                return finish_active_error(
                    request,
                    version,
                    store,
                    active,
                    heartbeat,
                    warnings,
                    error.diagnostic(DiagnosticStage::Cache),
                    elapsed_summary(started_at),
                    worker_handshake_ms,
                    &rss_tracker,
                    timing,
                );
            }
        };
        match hit {
            Some(hit) => {
                let artifact = match store.load_artifact(hit.artifact_id) {
                    Ok(artifact) => artifact,
                    Err(error) => {
                        return finish_active_error(
                            request,
                            version,
                            store,
                            active,
                            heartbeat,
                            warnings,
                            error.diagnostic(DiagnosticStage::Cache),
                            elapsed_summary(started_at),
                            worker_handshake_ms,
                            &rss_tracker,
                            timing,
                        );
                    }
                };
                artifact_guards_are_current(&artifact, &discovery.files, &timing.source_guards)
                    .then_some((hit, artifact))
            }
            None => None,
        }
    } else {
        None
    };
    let snapshot_lookup_ms = elapsed_ms(snapshot_lookup_started);
    if let Some((hit, artifact)) = verified_hit {
        return finalize_snapshot_hit(
            request,
            version,
            store,
            active,
            heartbeat,
            &discovery,
            inventory,
            &context_profile_hash,
            &classifier_identity,
            hit,
            artifact,
            discovery_duration_ms,
            inventory_prepare_duration_ms,
            snapshot_lookup_ms,
            worker_handshake_ms,
            &rss_tracker,
            started_at,
            timing,
        );
    }

    let input = match ScheduledRunInput::new_with_deadlines(
        active.scan_run_id(),
        started_at_ms,
        work_dir.to_string_lossy().into_owned(),
        discovery.files,
        discovery.issues,
        normalized_settings.clone(),
        worker_identities.clone(),
        version.engine_version.clone(),
        version.engine_build.clone(),
        context_profile_hash.clone(),
        rejected_profile_hash,
        discovery_duration_ms,
        timing.deadlines,
    )
    .and_then(|input| {
        input.with_prepared_inventory(
            inventory,
            inventory_existed_before,
            inventory_prepare_duration_ms,
        )
    }) {
        Ok(input) => input,
        Err(failure) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                failure.diagnostic,
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let scheduler = BudgetedContextScheduler::new(
        Box::new(classifier_port),
        Box::new(parser_port),
        Box::new(cache_port),
        Box::new(timing.clock.clone()),
        Box::new(RealGuardVerifier::new(timing.source_guards.clone())),
    );
    let mut outcome = match scheduler.execute(input) {
        Ok(outcome) => outcome,
        Err(failure) => {
            return finish_active_error(
                request,
                version,
                store,
                active,
                heartbeat,
                warnings,
                failure.diagnostic,
                elapsed_summary(started_at),
                worker_handshake_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let session_stats =
        crate::session::SessionPoolStats::combine(office_pool.stats(), python_pool.stats());
    let peak_worker_rss_bytes = combine_peak_rss(
        rss_tracker.peak_worker_rss_bytes(),
        session_stats.peak_worker_rss_bytes,
    );
    apply_worker_rss_observation(&mut outcome, peak_worker_rss_bytes);

    // ---- terminal finalization (the ONLY linearization point) ----
    // spec Part 2.3: a committed Error run MUST carry a context_runs row
    // (artifact_id=NULL). The scheduler outcome has no context for Error, so a
    // reconciling context is derived from the persisted file/decision rows.
    let derived_error_context = if matches!(outcome.terminal_intent, TerminalIntent::Error) {
        Some(build_error_context_record(
            &outcome.file_results,
            &outcome.inventory,
            &outcome.stage_metrics,
            &context_profile_hash,
        ))
    } else {
        None
    };
    let (envelope, run_status) = scheduler_outcome_envelope(
        request,
        version,
        active,
        &outcome,
        derived_error_context.as_ref(),
    );
    let envelope_json = match canonical_envelope_json(&envelope) {
        Ok(value) => value,
        Err(error) => {
            return persist_active_error_without_heartbeat(
                request,
                version,
                store,
                active,
                warnings,
                error.diagnostic(DiagnosticStage::Internal),
                outcome
                    .context
                    .as_ref()
                    .map(|context| context.summary.clone())
                    .unwrap_or_else(empty_summary),
                worker_handshake_ms,
                discovery_duration_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    // spec Part 5.1/5.4: every Success/Partial run persists an artifact
    // (eligible → snapshot key + per-file semantic rows; otherwise a payload
    // artifact with no rows). Built before the outcome is consumed by the batch.
    let artifact_draft = match build_batch_artifact(
        &outcome,
        normalized_settings,
        &worker_identities,
        &classifier_identity.profile_hash,
    ) {
        Ok(draft) => draft,
        Err(message) => {
            return persist_active_error_without_heartbeat(
                request,
                version,
                store,
                active,
                warnings,
                diagnostic(
                    ErrorCode::InternalError,
                    message,
                    false,
                    DiagnosticStage::Internal,
                ),
                outcome
                    .context
                    .as_ref()
                    .map(|context| context.summary.clone())
                    .unwrap_or_else(empty_summary),
                worker_handshake_ms,
                discovery_duration_ms,
                &rss_tracker,
                timing,
            );
        }
    };
    let snapshot_key_for_batch = artifact_draft
        .as_ref()
        .filter(|draft| draft.snapshot_eligible)
        .map(|_| key_parts.clone());
    let batch_context = match derived_error_context {
        Some(record) => Some(record),
        None => outcome.context,
    };
    let batch = FinalizationBatch {
        status: run_status,
        envelope_json: envelope_json.clone(),
        inventory: outcome.inventory,
        file_results: outcome.file_results,
        diagnostics: outcome.diagnostics,
        stage_metrics: outcome.stage_metrics,
        extension_metrics: outcome.extension_metrics,
        context: batch_context,
        artifact: artifact_draft,
        snapshot_key: snapshot_key_for_batch,
        snapshot_hit: None,
        execution_metrics: Some(assemble_scheduler_execution_metrics(
            &outcome.execution_metrics,
            Some(&session_stats),
            peak_worker_rss_bytes,
            worker_handshake_ms,
            discovery_duration_ms,
            snapshot_lookup_ms,
        )),
    };
    heartbeat.stop();
    if let Some(error) = heartbeat.take_background_error() {
        warnings.push(diagnostic(
            ErrorCode::CacheWriteFailed,
            format!("lease heartbeat recovered after a transient failure: {error}"),
            true,
            DiagnosticStage::Cache,
        ));
    }
    let finalize_now_ms = match current_time_millis() {
        Ok(value) => value,
        Err(error) => {
            return build_error_output(
                request,
                version,
                error.diagnostic(DiagnosticStage::Internal),
                warnings,
                batch
                    .context
                    .as_ref()
                    .map(|context| context.summary.clone())
                    .unwrap_or_else(empty_summary),
                Some(active.scan_run_id()),
            );
        }
    };
    let exit_code = i32::from(run_status == RunStatus::Error);
    if let Some(output) = persist_post_outcome_failure_if_invalid(
        request,
        version,
        store,
        active,
        &batch,
        worker_handshake_ms,
        discovery_duration_ms,
        &rss_tracker,
        timing,
    ) {
        return output;
    }
    match store.finalize(
        active,
        &batch,
        finalize_now_ms,
        timing.deadlines,
        &timing.clock,
    ) {
        Ok(timings) => complete_committed_terminal(store, timing, timings, envelope, exit_code),
        Err(error) => abandon_after_finalization_failure(
            request,
            version,
            store,
            active,
            error,
            warnings,
            empty_summary(),
        ),
    }
}

fn apply_worker_rss_observation(
    outcome: &mut BudgetedScanOutcome,
    peak_worker_rss_bytes: Option<u64>,
) {
    if peak_worker_rss_bytes.is_some() {
        return;
    }
    outcome.diagnostics.push(RunDiagnosticRecord {
        severity: DiagnosticSeverity::Warning,
        diagnostic: worker_rss_unavailable_diagnostic(),
    });
}

fn combine_peak_rss(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        _ => None,
    }
}

fn worker_rss_unavailable_diagnostic() -> Diagnostic {
    diagnostic(
        ErrorCode::WorkerRssUnavailable,
        "worker peak RSS could not be observed".to_string(),
        false,
        DiagnosticStage::Process,
    )
}

/// Rebuilds the frozen `ContextEnvelope` from a scheduler outcome. The caller
/// must not re-decide actions/counts/admission; this only projects the outcome
/// onto the envelope shape and the terminal record.
fn scheduler_outcome_envelope(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    active: &ActiveRun,
    outcome: &crate::scheduler::BudgetedScanOutcome,
    error_context: Option<&ContextRunRecord>,
) -> (ContextEnvelope, RunStatus) {
    let run_status = match outcome.terminal_intent {
        crate::scheduler::TerminalIntent::Success => RunStatus::Success,
        crate::scheduler::TerminalIntent::Partial => RunStatus::Partial,
        crate::scheduler::TerminalIntent::Error => RunStatus::Error,
    };
    let engine_status = match run_status {
        RunStatus::Success => EngineStatus::Ok,
        RunStatus::Partial => EngineStatus::Partial,
        RunStatus::Error => EngineStatus::Error,
        RunStatus::Running | RunStatus::Abandoned => EngineStatus::Error,
    };
    let warnings: Vec<Diagnostic> = outcome
        .diagnostics
        .iter()
        .filter(|record| record.severity == DiagnosticSeverity::Warning)
        .map(|record| record.diagnostic.clone())
        .take(100_000)
        .collect();
    let error: Option<Diagnostic> = outcome
        .diagnostics
        .iter()
        .filter(|record| record.severity == DiagnosticSeverity::Error)
        .map(|record| record.diagnostic.clone())
        .next();
    // spec Part 2.3: every committed terminal run (Success/Partial/Error) returns
    // `context_run_id = scan_run_id`. An Error run's summary is the derived
    // reconciling summary (empty file_context), never a fabricated zero object.
    let (file_context, summary, context_run_id) = match &outcome.context {
        Some(context) => (
            context.final_context.clone(),
            context.summary.clone(),
            Some(active.context_run_id()),
        ),
        None if run_status == RunStatus::Error => (
            String::new(),
            error_context
                .map(|context| context.summary.clone())
                .unwrap_or_else(empty_summary),
            Some(active.context_run_id()),
        ),
        None => (String::new(), empty_summary(), None),
    };
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        engine_version: version.engine_version.clone(),
        engine_build: version.engine_build.clone(),
        status: engine_status,
        file_context,
        summary,
        scan_run_id: Nullable(Some(active.scan_run_id())),
        context_run_id: Nullable(context_run_id),
        warnings,
        error: Nullable(error),
    };
    (envelope, run_status)
}

/// Derives the minimal `context_runs` record for an Error outcome (spec
/// Part 2.3 + Part 2.2 count equations): one decision per file result
/// (Error/Timeout → Error, NotParsed → Omit, Success → Keep), an empty
/// `file_context`, and a summary that reconciles with the persisted
/// file/decision/stage rows. Durations come from the stage metrics so the
/// relational summary validation passes; never copies a Success/Partial render.
fn build_error_context_record(
    file_results: &[FileResultRecord],
    inventory: &[InventoryRecord],
    stage_metrics: &[StageMetric],
    context_profile_hash: &str,
) -> ContextRunRecord {
    let size_by_identity: std::collections::HashMap<&str, u64> = inventory
        .iter()
        .map(|item| (item.file_identity.as_str(), item.size_bytes))
        .collect();
    let decisions: Vec<ContextDecisionRecord> = file_results
        .iter()
        .map(|result| {
            let action = match result.parse_status {
                ParseStatus::Success => ContextAction::Keep,
                ParseStatus::Error | ParseStatus::Timeout => ContextAction::Error,
                ParseStatus::NotParsed => ContextAction::Omit,
            };
            let reason = match action {
                ContextAction::Error => "parse_error".to_string(),
                ContextAction::Omit => "not_parsed".to_string(),
                _ => "small_file_keep".to_string(),
            };
            ContextDecisionRecord {
                file_identity: result.file_identity.clone(),
                decision: ContextDecision {
                    relative_path: result.relative_path.clone(),
                    action,
                    reason,
                    priority: 0,
                    input_chars: size_by_identity
                        .get(result.file_identity.as_str())
                        .copied()
                        .unwrap_or(0),
                    output_chars: 0,
                    truncated: result.truncated,
                    error_code: result
                        .error
                        .as_ref()
                        .map(|diagnostic| {
                            crate::store::inventory::enum_text(&diagnostic.error_code)
                        })
                        .unwrap_or_default(),
                },
            }
        })
        .collect();
    let success_count = file_results
        .iter()
        .filter(|result| result.parse_status == ParseStatus::Success)
        .count() as u64;
    let timeout_count = file_results
        .iter()
        .filter(|result| result.parse_status == ParseStatus::Timeout)
        .count() as u64;
    let error_count = file_results
        .iter()
        .filter(|result| result.parse_status == ParseStatus::Error)
        .count() as u64;
    let source_file_count = file_results.len() as u64;
    let not_parsed_count = source_file_count
        .saturating_sub(success_count)
        .saturating_sub(timeout_count)
        .saturating_sub(error_count);
    let stage_by_name: std::collections::HashMap<StageName, &StageMetric> = stage_metrics
        .iter()
        .map(|metric| (metric.stage, metric))
        .collect();
    let summary = ContextSummary {
        source_file_count,
        success_count,
        timeout_count,
        included_file_count: success_count,
        omitted_file_count: not_parsed_count,
        error_file_count: error_count,
        input_chars: decisions
            .iter()
            .map(|record| record.decision.input_chars)
            .sum(),
        output_chars: 0,
        total_duration_ms: stage_metrics
            .iter()
            .fold(0_u64, |acc, metric| acc + metric.duration_ms),
        discovery_duration_ms: stage_by_name
            .get(&StageName::Discovery)
            .map(|metric| metric.duration_ms)
            .unwrap_or(0),
        parse_duration_ms: stage_by_name
            .get(&StageName::Parse)
            .map(|metric| metric.duration_ms)
            .unwrap_or(0),
        compression_duration_ms: stage_by_name
            .get(&StageName::Context)
            .map(|metric| metric.duration_ms)
            .unwrap_or(0),
    };
    ContextRunRecord {
        context_profile_hash: context_profile_hash.to_string(),
        status: RunStatus::Error,
        final_context: String::new(),
        context_sha256: crate::store::sha256_hex(b""),
        summary,
        decisions,
    }
}

/// Assembles the strict `execution_metrics` for the scheduler path (spec
/// Part 5.3). The scheduler owns the plan/execution counts; the run shell owns
/// the wall timings. `current_run_audit_write_ms`/`terminal_precommit_ms`/
/// `envelope_rebuild_ms`/`terminal_rows_written` are filled by `store.finalize`.
fn assemble_scheduler_execution_metrics(
    metrics: &crate::scheduler::ExecutionMetrics,
    session: Option<&crate::session::SessionPoolStats>,
    peak_worker_rss_bytes: Option<u64>,
    worker_handshake_ms: u64,
    discovery_ms: u64,
    snapshot_lookup_ms: u64,
) -> ai_daily_scanner_contract::ExecutionMetricsV2 {
    ai_daily_scanner_contract::ExecutionMetricsV2 {
        discovery_observed_file_count: metrics.discovery_observed_file_count,
        source_guard_content_hash_file_count: metrics.source_guard_content_hash_file_count,
        source_guard_unavailable_count: metrics.source_guard_unavailable_count,
        source_guard_bytes_read: metrics.source_guard_bytes_read,
        candidate_file_count: metrics.candidate_file_count,
        admitted_file_count: metrics.admitted_file_count,
        classification_slot_count: metrics.classification_slot_count,
        confirmed_run_inspected_pages_total: metrics.confirmed_run_inspected_pages_total,
        unobserved_classification_attempt_count: metrics.unobserved_classification_attempt_count,
        nominal_charged_pages_total: metrics.nominal_charged_pages_total,
        extraction_slot_count: metrics.extraction_slot_count,
        pdfplumber_invocations: metrics.pdfplumber_invocations,
        snapshot_hit: false,
        parse_cache_lookup_count: metrics.parse_cache_lookup_count,
        classification_cache_lookup_count: metrics.classification_cache_lookup_count,
        parse_cache_all_hit: Nullable(metrics.parse_cache_all_hit),
        classification_cache_all_hit: Nullable(metrics.classification_cache_all_hit),
        stage_deadline_exhausted_count: metrics.stage_deadline_exhausted_count,
        session_restart_count: session.map_or(0, |stats| stats.session_restart_count),
        session_fallback_count: 0,
        classify_attempt_count: metrics.classify_attempt_count,
        parse_attempt_count: metrics.parse_attempt_count,
        reserved_chars: metrics.reserved_chars,
        rendered_chars: metrics.rendered_chars,
        worker_handshake_ms,
        discovery_ms,
        snapshot_lookup_ms,
        current_run_audit_write_ms: 0,
        terminal_precommit_ms: 0,
        deadline_precommit_elapsed_ms: metrics.deadline_precommit_elapsed_ms,
        envelope_rebuild_ms: 0,
        terminal_rows_written: 0,
        peak_worker_rss_bytes: Nullable(peak_worker_rss_bytes),
    }
}

/// Assembles the strict `execution_metrics` for a snapshot-hit current run
/// (spec Part 5.4): the scheduler did not run, so every plan/execution count is
/// 0; discovery/source-guard counts and the live-handshake/lookup/terminal wall
/// spans are this run's real measured values.
#[allow(clippy::too_many_arguments)]
fn assemble_snapshot_execution_metrics(
    discovery_observed_file_count: u64,
    guards: SourceGuardObservationMetrics,
    reserved_chars: u64,
    rendered_chars: u64,
    worker_handshake_ms: u64,
    discovery_ms: u64,
    snapshot_lookup_ms: u64,
    deadline_precommit_elapsed_ms: u64,
    peak_worker_rss_bytes: Option<u64>,
) -> ai_daily_scanner_contract::ExecutionMetricsV2 {
    ai_daily_scanner_contract::ExecutionMetricsV2 {
        discovery_observed_file_count,
        source_guard_content_hash_file_count: guards.content_hash_file_count,
        source_guard_unavailable_count: guards.unavailable_file_count,
        source_guard_bytes_read: guards.bytes_read,
        candidate_file_count: 0,
        admitted_file_count: 0,
        classification_slot_count: 0,
        confirmed_run_inspected_pages_total: 0,
        unobserved_classification_attempt_count: 0,
        nominal_charged_pages_total: 0,
        extraction_slot_count: 0,
        pdfplumber_invocations: 0,
        snapshot_hit: true,
        parse_cache_lookup_count: 0,
        classification_cache_lookup_count: 0,
        parse_cache_all_hit: Nullable(None),
        classification_cache_all_hit: Nullable(None),
        stage_deadline_exhausted_count: 0,
        session_restart_count: 0,
        session_fallback_count: 0,
        classify_attempt_count: 0,
        parse_attempt_count: 0,
        reserved_chars,
        rendered_chars,
        worker_handshake_ms,
        discovery_ms,
        snapshot_lookup_ms,
        current_run_audit_write_ms: 0,
        terminal_precommit_ms: 0,
        deadline_precommit_elapsed_ms,
        envelope_rebuild_ms: 0,
        terminal_rows_written: 0,
        peak_worker_rss_bytes: Nullable(peak_worker_rss_bytes),
    }
}

/// Assembles the strict `execution_metrics` for the engine-error path (spec
/// Part 5.3): the scheduler did not complete, so every plan/execution count is
/// 0; the worker handshake and discovery wall spans that genuinely ran are the
/// measured values (never fabricated zeros).
fn assemble_engine_error_execution_metrics(
    worker_handshake_ms: u64,
    discovery_ms: u64,
    discovery_observed_file_count: u64,
    guards: SourceGuardObservationMetrics,
    peak_worker_rss_bytes: Option<u64>,
) -> ai_daily_scanner_contract::ExecutionMetricsV2 {
    ai_daily_scanner_contract::ExecutionMetricsV2 {
        discovery_observed_file_count,
        source_guard_content_hash_file_count: guards.content_hash_file_count,
        source_guard_unavailable_count: guards.unavailable_file_count,
        source_guard_bytes_read: guards.bytes_read,
        candidate_file_count: 0,
        admitted_file_count: 0,
        classification_slot_count: 0,
        confirmed_run_inspected_pages_total: 0,
        unobserved_classification_attempt_count: 0,
        nominal_charged_pages_total: 0,
        extraction_slot_count: 0,
        pdfplumber_invocations: 0,
        snapshot_hit: false,
        parse_cache_lookup_count: 0,
        classification_cache_lookup_count: 0,
        parse_cache_all_hit: Nullable(None),
        classification_cache_all_hit: Nullable(None),
        stage_deadline_exhausted_count: 0,
        session_restart_count: 0,
        session_fallback_count: 0,
        classify_attempt_count: 0,
        parse_attempt_count: 0,
        reserved_chars: 0,
        rendered_chars: 0,
        worker_handshake_ms,
        discovery_ms,
        snapshot_lookup_ms: 0,
        current_run_audit_write_ms: 0,
        terminal_precommit_ms: 0,
        deadline_precommit_elapsed_ms: 0,
        envelope_rebuild_ms: 0,
        terminal_rows_written: 0,
        peak_worker_rss_bytes: Nullable(peak_worker_rss_bytes),
    }
}

/// Spec Part 5.4 snapshot-hit finalization: the current run reuses the selected
/// artifact (reference + `snapshot_hit`/`reused_from` written by the store),
/// current rows are rebuilt from artifact rows with `snapshot` semantics
/// (`parse_cache_status=snapshot`, `cache_miss_reason=''`, `parse_duration_ms=0`),
/// and the current summary durations are THIS run's measured values — never the
/// source run's old timings.
#[allow(clippy::too_many_arguments)]
fn finalize_snapshot_hit(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    store: &mut ScannerStore,
    active: &ActiveRun,
    heartbeat: &mut LeaseHeartbeat,
    discovery: &DiscoveryReport,
    inventory: Vec<InventoryRecord>,
    context_profile_hash: &str,
    classifier_identity: &ClassifierIdentity,
    hit: SnapshotHit,
    artifact: ArtifactDraft,
    discovery_duration_ms: u64,
    inventory_prepare_duration_ms: u64,
    snapshot_lookup_ms: u64,
    worker_handshake_ms: u64,
    rss_tracker: &WorkerRssTracker,
    started_at: Instant,
    timing: &ActiveRunTiming,
) -> ScannerOperation<ContextEnvelope> {
    let file_results = snapshot_file_results(&artifact, Some(classifier_identity));
    let decisions: Vec<ContextDecisionRecord> = artifact
        .decision_rows
        .iter()
        .map(|row| ContextDecisionRecord {
            file_identity: row.file_identity.clone(),
            decision: ContextDecision {
                relative_path: row.relative_path.clone(),
                action: row.action,
                reason: row.reason.clone(),
                priority: row.priority,
                input_chars: row.input_chars,
                output_chars: row.output_chars,
                truncated: row.truncated,
                error_code: row.error_code.clone(),
            },
        })
        .collect();
    let compression_duration_ms = snapshot_lookup_ms;
    let summary = ContextSummary {
        source_file_count: artifact.semantic_summary.source_file_count,
        success_count: artifact.semantic_summary.success_count,
        timeout_count: artifact.semantic_summary.timeout_count,
        included_file_count: artifact.semantic_summary.included_file_count,
        omitted_file_count: artifact.semantic_summary.omitted_file_count,
        error_file_count: artifact.semantic_summary.error_file_count,
        input_chars: artifact.semantic_summary.input_chars,
        output_chars: artifact.semantic_summary.output_chars,
        total_duration_ms: elapsed_ms(started_at),
        discovery_duration_ms,
        parse_duration_ms: 0,
        compression_duration_ms,
    };
    let stage_metrics = vec![
        StageMetric {
            stage: StageName::Discovery,
            item_count: discovery.files.len() as u64,
            duration_ms: discovery_duration_ms,
        },
        StageMetric {
            stage: StageName::Cache,
            item_count: discovery.files.len() as u64,
            duration_ms: inventory_prepare_duration_ms.saturating_add(snapshot_lookup_ms),
        },
        StageMetric {
            stage: StageName::Parse,
            item_count: 0,
            duration_ms: 0,
        },
        StageMetric {
            stage: StageName::Context,
            item_count: decisions.len() as u64,
            duration_ms: compression_duration_ms,
        },
    ];
    let extension_metrics = match crate::context_audit::extension_metrics(&inventory, &file_results)
    {
        Ok(metrics) => metrics,
        Err(message) => {
            heartbeat.stop();
            return build_error_output(
                request,
                version,
                diagnostic(
                    ErrorCode::InternalError,
                    message,
                    false,
                    DiagnosticStage::Internal,
                ),
                Vec::new(),
                empty_summary(),
                Some(active.scan_run_id()),
            );
        }
    };
    let peak_worker_rss_bytes = rss_tracker.peak_worker_rss_bytes();
    let warnings: Vec<Diagnostic> = peak_worker_rss_bytes
        .is_none()
        .then(worker_rss_unavailable_diagnostic)
        .into_iter()
        .collect();
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        engine_version: version.engine_version.clone(),
        engine_build: version.engine_build.clone(),
        status: EngineStatus::Ok,
        file_context: artifact.final_context.clone(),
        summary: summary.clone(),
        scan_run_id: Nullable(Some(active.scan_run_id())),
        context_run_id: Nullable(Some(active.context_run_id())),
        warnings: warnings.clone(),
        error: Nullable(None),
    };
    let envelope_json = match canonical_envelope_json(&envelope) {
        Ok(json) => json,
        Err(error) => {
            heartbeat.stop();
            return build_error_output(
                request,
                version,
                error.diagnostic(DiagnosticStage::Internal),
                Vec::new(),
                summary,
                Some(active.scan_run_id()),
            );
        }
    };
    let context = ContextRunRecord {
        context_profile_hash: context_profile_hash.to_string(),
        status: RunStatus::Success,
        final_context: artifact.final_context.clone(),
        context_sha256: artifact.context_sha256.clone(),
        summary,
        decisions,
    };
    let batch = FinalizationBatch {
        status: RunStatus::Success,
        envelope_json: envelope_json.clone(),
        inventory,
        file_results,
        diagnostics: warnings
            .into_iter()
            .map(|diagnostic| RunDiagnosticRecord {
                severity: DiagnosticSeverity::Warning,
                diagnostic,
            })
            .collect(),
        stage_metrics,
        extension_metrics,
        context: Some(context),
        artifact: None,
        snapshot_key: None,
        snapshot_hit: Some(SnapshotHitRef {
            artifact_id: hit.artifact_id,
            reused_from_context_run_id: hit.source_context_run_id,
        }),
        execution_metrics: Some(assemble_snapshot_execution_metrics(
            discovery.files.len() as u64,
            timing.source_guards.metrics(),
            artifact.semantic_summary.reserved_chars,
            artifact.semantic_summary.rendered_chars,
            worker_handshake_ms,
            discovery_duration_ms,
            snapshot_lookup_ms,
            elapsed_ms(started_at),
            peak_worker_rss_bytes,
        )),
    };
    heartbeat.stop();
    let finalize_now_ms = match current_time_millis() {
        Ok(value) => value,
        Err(error) => {
            return build_error_output(
                request,
                version,
                error.diagnostic(DiagnosticStage::Internal),
                Vec::new(),
                batch
                    .context
                    .as_ref()
                    .map(|context| context.summary.clone())
                    .unwrap_or_else(empty_summary),
                Some(active.scan_run_id()),
            );
        }
    };
    if let Some(output) = persist_post_outcome_failure_if_invalid(
        request,
        version,
        store,
        active,
        &batch,
        worker_handshake_ms,
        discovery_duration_ms,
        rss_tracker,
        timing,
    ) {
        return output;
    }
    match store.finalize(
        active,
        &batch,
        finalize_now_ms,
        timing.deadlines,
        &timing.clock,
    ) {
        Ok(timings) => complete_committed_terminal(store, timing, timings, envelope, 0),
        Err(error) => abandon_after_finalization_failure(
            request,
            version,
            store,
            active,
            error,
            Vec::new(),
            empty_summary(),
        ),
    }
}

/// Builds the `ArtifactDraft` for a scheduler outcome (spec Part 5.1).
/// `None` for Error runs; `Some` for Success/Partial. An eligible artifact
/// (Ok + warnings-empty + no Error/Timeout) carries the snapshot key plus
/// per-source-file rows; ineligible runs persist a payload artifact with no rows.
#[allow(clippy::too_many_arguments)]
fn build_batch_artifact(
    outcome: &BudgetedScanOutcome,
    normalized_settings: &ai_daily_scanner_contract::NormalizedScannerSettings,
    worker_identities: &WorkerIdentities,
    classifier_profile_hash: &str,
) -> Result<Option<ArtifactDraft>, String> {
    let Some(context) = &outcome.context else {
        return Ok(None);
    };
    let eligible = matches!(outcome.terminal_intent, TerminalIntent::Success)
        && !outcome
            .diagnostics
            .iter()
            .any(|record| record.severity == DiagnosticSeverity::Warning)
        && outcome.file_results.iter().all(|record| {
            !matches!(
                record.parse_status,
                ParseStatus::Error | ParseStatus::Timeout
            )
        });
    let semantic = SemanticSummary {
        source_file_count: context.summary.source_file_count,
        success_count: context.summary.success_count,
        timeout_count: context.summary.timeout_count,
        included_file_count: context.summary.included_file_count,
        omitted_file_count: context.summary.omitted_file_count,
        error_file_count: context.summary.error_file_count,
        input_chars: context.summary.input_chars,
        output_chars: context.summary.output_chars,
        reserved_chars: outcome.execution_metrics.reserved_chars,
        rendered_chars: outcome.execution_metrics.rendered_chars,
    };
    if eligible {
        let file_rows = artifact_file_rows(
            outcome,
            normalized_settings,
            worker_identities,
            classifier_profile_hash,
        );
        let decision_rows = artifact_decision_rows(&context.decisions);
        ArtifactDraft::new(
            true,
            context.final_context.clone(),
            semantic,
            file_rows,
            decision_rows,
        )
        .map(Some)
    } else {
        ArtifactDraft::new(
            false,
            context.final_context.clone(),
            semantic,
            Vec::new(),
            Vec::new(),
        )
        .map(Some)
    }
}

#[allow(clippy::too_many_arguments)]
fn artifact_file_rows(
    outcome: &BudgetedScanOutcome,
    normalized_settings: &ai_daily_scanner_contract::NormalizedScannerSettings,
    worker_identities: &WorkerIdentities,
    classifier_profile_hash: &str,
) -> Vec<ArtifactFileRow> {
    let result_by_identity: std::collections::HashMap<&str, &FileResultRecord> = outcome
        .file_results
        .iter()
        .map(|record| (record.file_identity.as_str(), record))
        .collect();
    let classifier_build = worker_identities
        .classifier_build
        .clone()
        .unwrap_or_else(|| "0".repeat(64));
    outcome
        .inventory
        .iter()
        .map(|item| {
            let result = result_by_identity.get(item.file_identity.as_str());
            let empty_hash = crate::store::sha256_hex(b"");
            // spec Part 3.2: the artifact stores only the immutable classifier
            // provenance subset (status/page/result/nominal/build/profile-hash);
            // cache status / miss reason / run pages / duration / transport /
            // attempt are current-run execution fields and never reused.
            let classifier =
                outcome
                    .classifications
                    .get(&item.file_identity)
                    .map(|classification| {
                        PdfClassificationProvenanceV1 {
                    status: classification.status,
                    page_count: classification.page_count,
                    result_examined_pages: classification.result_examined_pages,
                    nominal_charged_pages: if classification.status
                        == ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
                    {
                        0
                    } else {
                        normalized_settings.parse.pdf.max_pages
                    },
                    classifier_build: classifier_build.clone(),
                    classifier_profile_hash: classifier_profile_hash.to_string(),
                }
                    });
            ArtifactFileRow {
                file_identity: item.file_identity.clone(),
                relative_path: item.relative_path.clone(),
                legacy_source_version: item.source_version.clone(),
                source_guard_kind: item.source_guard_kind.clone(),
                source_guard_sha256: item.source_guard_sha256.clone(),
                parse_profile_hash: result
                    .map(|record| record.parse_profile_hash.clone())
                    .unwrap_or_else(|| "0".repeat(64)),
                parse_status: result
                    .map(|record| record.parse_status)
                    .unwrap_or(ParseStatus::NotParsed),
                parser_backend: result
                    .map(|record| record.parser_backend.clone())
                    .unwrap_or_else(|| "not_parsed".to_string()),
                worker_lane: result
                    .map(|record| {
                        crate::store::inventory::worker_lane_text(record.worker_lane).to_string()
                    })
                    .unwrap_or_else(|| "not_parsed".to_string()),
                truncated: result.map(|record| record.truncated).unwrap_or(false),
                content_sha256: result
                    .map(|record| record.content_sha256.clone())
                    .unwrap_or(empty_hash),
                classifier,
            }
        })
        .collect()
}

fn artifact_decision_rows(decisions: &[ContextDecisionRecord]) -> Vec<ArtifactDecisionRow> {
    decisions
        .iter()
        .map(|record| ArtifactDecisionRow {
            file_identity: record.file_identity.clone(),
            relative_path: record.decision.relative_path.clone(),
            action: record.decision.action,
            reason: record.decision.reason.clone(),
            priority: record.decision.priority,
            input_chars: record.decision.input_chars,
            output_chars: record.decision.output_chars,
            truncated: record.decision.truncated,
            error_code: record.decision.error_code.clone(),
        })
        .collect()
}

fn snapshot_inventory(
    files: &[DiscoveredFileOut],
    work_dir: &str,
) -> Result<Vec<InventoryRecord>, String> {
    files
        .iter()
        .map(|file| {
            let relative_path =
                crate::context_audit::relative_contract_path(Path::new(work_dir), &file.path)?;
            InventoryRecord::from_discovered(file, relative_path)
        })
        .collect()
}

/// Current-run rows for a snapshot hit (spec Part 5.2): `parse_cache_status`
/// becomes `snapshot`, `cache_miss_reason=''`, durations/attempts 0 — never the
/// source run's miss/hit or old timings.
fn snapshot_file_results(
    artifact: &ArtifactDraft,
    classifier_identity: Option<&ClassifierIdentity>,
) -> Vec<FileResultRecord> {
    let decision_reason_by_identity: std::collections::HashMap<&str, &str> = artifact
        .decision_rows
        .iter()
        .map(|row| (row.file_identity.as_str(), row.reason.as_str()))
        .collect();
    artifact
        .file_rows
        .iter()
        .map(|row| FileResultRecord {
            file_identity: row.file_identity.clone(),
            relative_path: row.relative_path.clone(),
            source_version: row.legacy_source_version.clone(),
            parse_profile_hash: row.parse_profile_hash.clone(),
            cache_status: CacheStatus::Fresh,
            cache_miss_reason: CacheMissReason::None,
            parse_status: row.parse_status,
            parser_backend: row.parser_backend.clone(),
            worker_lane: snapshot_worker_lane(&row.worker_lane),
            truncated: row.truncated,
            content_sha256: row.content_sha256.clone(),
            primary_duration_ms: 0,
            fallback_duration_ms: 0,
            parse_duration_ms: 0,
            failure_class: String::new(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ai_daily_scanner_contract::ParseTransport::Snapshot,
            parse_attempt_count: 0,
            pdf_classification: snapshot_classification_audit(
                row.classifier.as_ref(),
                decision_reason_by_identity
                    .get(row.file_identity.as_str())
                    .copied(),
                classifier_identity,
            ),
            error: None,
        })
        .collect()
}

fn snapshot_classification_audit(
    provenance: Option<&PdfClassificationProvenanceV1>,
    decision_reason: Option<&str>,
    classifier_identity: Option<&ClassifierIdentity>,
) -> Option<ai_daily_scanner_contract::PdfClassificationAuditV1> {
    let reconstructed;
    let provenance = match provenance {
        Some(value) => value,
        None if decision_reason == Some("pdf_classification_page_quota_exhausted") => {
            let identity = classifier_identity?;
            reconstructed = PdfClassificationProvenanceV1 {
                status: ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget,
                page_count: None,
                result_examined_pages: Some(0),
                nominal_charged_pages: 0,
                classifier_build: identity.build.clone(),
                classifier_profile_hash: identity.profile_hash.clone(),
            };
            &reconstructed
        }
        None => return None,
    };
    let not_eligible = provenance.status
        == ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget;
    Some(ai_daily_scanner_contract::PdfClassificationAuditV1 {
        status: provenance.status,
        page_count: Nullable(provenance.page_count),
        classification_cache_status: if not_eligible {
            ai_daily_scanner_contract::ClassificationCacheStatus::NotEligible
        } else {
            ai_daily_scanner_contract::ClassificationCacheStatus::Snapshot
        },
        classification_cache_miss_reason: String::new(),
        result_examined_pages: Nullable(provenance.result_examined_pages),
        run_inspected_pages: Nullable(Some(0)),
        nominal_charged_pages: provenance.nominal_charged_pages,
        duration_ms: 0,
        transport: if not_eligible {
            ai_daily_scanner_contract::ClassificationTransport::NotApplicable
        } else {
            ai_daily_scanner_contract::ClassificationTransport::Snapshot
        },
        attempt_count: 0,
        classifier_build: provenance.classifier_build.clone(),
        classifier_profile_hash: provenance.classifier_profile_hash.clone(),
    })
}

fn snapshot_worker_lane(lane: &str) -> AuditWorkerLane {
    match lane {
        "rust_core" => AuditWorkerLane::RustCore,
        "rust_office_process_v2" => AuditWorkerLane::RustOfficeProcessV2,
        "python_document_process_v2" => AuditWorkerLane::PythonDocumentProcessV2,
        _ => AuditWorkerLane::NotParsed,
    }
}

fn validate_build_work_dir(work_dir: &str) -> Result<PathBuf, Diagnostic> {
    let path = Path::new(work_dir);
    let metadata = fs::metadata(path).map_err(|error| {
        let error_code = if error.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::WorkDirNotFound
        } else {
            ErrorCode::DiscoveryEntryUnreadable
        };
        diagnostic(
            error_code,
            "work_dir is unavailable".to_string(),
            error.kind() != std::io::ErrorKind::NotFound,
            DiagnosticStage::Request,
        )
    })?;
    if !metadata.is_dir() {
        return Err(diagnostic(
            ErrorCode::WorkDirNotDirectory,
            "work_dir is not a directory".to_string(),
            false,
            DiagnosticStage::Request,
        ));
    }
    fs::canonicalize(path)
        .map(|canonical| PathBuf::from(normalize_contract_path_text(&canonical.to_string_lossy())))
        .map_err(|_| {
            diagnostic(
                ErrorCode::DiscoveryEntryUnreadable,
                "work_dir could not be canonicalized".to_string(),
                true,
                DiagnosticStage::Request,
            )
        })
}

fn split_handshake(
    result: Result<RegisteredWorker, crate::fallback::ParseFailure>,
    backend: &str,
) -> (Option<RegisteredWorker>, Option<Diagnostic>) {
    match result {
        Ok(worker) => (Some(worker), None),
        Err(failure) => {
            let mut diagnostic = failure.diagnostic;
            if diagnostic.backend.0.is_none() {
                diagnostic.backend = Nullable(Some(backend.to_string()));
            }
            (None, Some(diagnostic))
        }
    }
}

fn worker_fingerprint(worker: &RegisteredWorker) -> WorkerFingerprint {
    WorkerFingerprint {
        contract: worker.identity.worker_contract_version.clone(),
        version: worker.identity.worker_version.clone(),
        build: worker.identity.worker_build.clone(),
    }
}

fn route_stack_fingerprints(
    version: &EngineIdentity,
    profile: &ai_daily_scanner_contract::NormalizedScannerSettings,
    office_worker: &RegisteredWorker,
    python_worker: &RegisteredWorker,
) -> Result<RouteStackFingerprints, String> {
    let python_fallback = profile.parse.office.fallback_enabled.then_some((
        python_worker.identity.worker_contract_version.as_str(),
        python_worker.identity.worker_build.as_str(),
    ));
    Ok(RouteStackFingerprints {
        text_like: RouteStackFingerprint::text(&version.engine_build)?,
        modern_office: RouteStackFingerprint::modern_office(
            &version.engine_build,
            &office_worker.identity.worker_contract_version,
            &office_worker.identity.worker_build,
            python_fallback,
        )?,
        python_document: RouteStackFingerprint::python_document(
            &version.engine_build,
            &python_worker.identity.worker_contract_version,
            &python_worker.identity.worker_build,
        )?,
    })
}

fn source_guard_from_wire(kind: Option<&str>, hash: Option<&str>) -> Option<SourceGuardV2> {
    let guard = SourceGuardV2 {
        kind: source_guard_kind_from_text(kind?)?,
        guard_sha256: hash.map(str::to_string),
    };
    guard.validate().ok()?;
    Some(guard)
}

fn discovery_guards_are_current(
    files: &[DiscoveredFileOut],
    observer: &SourceGuardObserver,
) -> bool {
    let mut all_current = true;
    for file in files {
        let Some(expected) = source_guard_from_wire(
            file.source_guard_kind.as_deref(),
            file.source_guard_sha256.as_deref(),
        ) else {
            all_current = false;
            continue;
        };
        if expected.kind == SourceGuardKind::Unavailable
            || !observer.verify(Path::new(&file.path), &expected)
        {
            all_current = false;
        }
    }
    all_current
}

fn artifact_guards_are_current(
    artifact: &ArtifactDraft,
    files: &[DiscoveredFileOut],
    observer: &SourceGuardObserver,
) -> bool {
    let mut all_current = artifact.snapshot_eligible && artifact.file_rows.len() == files.len();
    let mut rows = std::collections::HashMap::with_capacity(artifact.file_rows.len());
    for row in &artifact.file_rows {
        if rows.insert(row.file_identity.as_str(), row).is_some() {
            all_current = false;
        }
    }
    for file in files {
        let discovery_guard = source_guard_from_wire(
            file.source_guard_kind.as_deref(),
            file.source_guard_sha256.as_deref(),
        );
        if let Some(expected) = discovery_guard.as_ref() {
            if expected.kind == SourceGuardKind::Unavailable
                || !observer.verify(Path::new(&file.path), expected)
            {
                all_current = false;
            }
        } else {
            all_current = false;
        }

        let Some(row) = rows.remove(file.file_identity.as_str()) else {
            all_current = false;
            continue;
        };
        let artifact_guard = source_guard_from_wire(
            row.source_guard_kind.as_deref(),
            row.source_guard_sha256.as_deref(),
        );
        if row.legacy_source_version != file.source_version
            || artifact_guard.is_none()
            || artifact_guard != discovery_guard
        {
            all_current = false;
        }
    }
    all_current && rows.is_empty()
}

/// Computes the engine-owned SourceGuardV2 for every discovered file and
/// carries it on the discovery output so cache/snapshot identity can consume
/// it. A hard guard I/O error leaves the file unavailable (fail closed), never
/// an invented metadata identity.
fn attach_source_guards(files: &mut [DiscoveredFileOut], observer: &SourceGuardObserver) {
    for file in files {
        let guard = observer
            .compute(Path::new(&file.path))
            .unwrap_or(SourceGuardV2 {
                kind: SourceGuardKind::Unavailable,
                guard_sha256: None,
            });
        file.source_guard_kind = Some(source_guard_kind_text(guard.kind).to_string());
        file.source_guard_sha256 = guard.guard_sha256;
    }
}

fn discover_with_timeout(
    work_dir: &Path,
    request: &BuildContextRequest,
    profile: &ai_daily_scanner_contract::NormalizedScannerSettings,
    timeout: Duration,
) -> Result<DiscoveryReport, Diagnostic> {
    let start_date = NaiveDate::parse_from_str(&request.start_date, "%Y-%m-%d").map_err(|_| {
        diagnostic(
            ErrorCode::InvalidRequest,
            "start_date is invalid".to_string(),
            false,
            DiagnosticStage::Request,
        )
    })?;
    let end_date = NaiveDate::parse_from_str(&request.end_date, "%Y-%m-%d").map_err(|_| {
        diagnostic(
            ErrorCode::InvalidRequest,
            "end_date is invalid".to_string(),
            false,
            DiagnosticStage::Request,
        )
    })?;
    let discovery_request = DiscoveryRequest {
        work_dir: work_dir.to_path_buf(),
        start_date,
        end_date,
        allowed_extensions: profile.discovery.allowed_extensions.clone(),
        ignored_patterns: profile.discovery.ignored_patterns.clone(),
        excluded_dirs: profile
            .discovery
            .excluded_dirs
            .iter()
            .map(PathBuf::from)
            .collect(),
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let _ = sender.send(discover_files_with_diagnostics(&discovery_request));
    });
    match receiver.recv_timeout(timeout) {
        Ok(Ok(report)) => {
            if handle.join().is_err() {
                Err(diagnostic(
                    ErrorCode::InternalError,
                    "discovery worker panicked".to_string(),
                    false,
                    DiagnosticStage::Discovery,
                ))
            } else {
                Ok(report)
            }
        }
        Ok(Err(error)) => {
            let error_code = match error.kind() {
                std::io::ErrorKind::NotFound => ErrorCode::WorkDirNotFound,
                std::io::ErrorKind::InvalidInput => ErrorCode::WorkDirNotDirectory,
                _ => ErrorCode::DiscoveryEntryUnreadable,
            };
            Err(diagnostic(
                error_code,
                "file discovery failed".to_string(),
                error.kind() != std::io::ErrorKind::InvalidInput,
                DiagnosticStage::Discovery,
            ))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => Err(diagnostic(
            ErrorCode::InternalError,
            "file discovery exceeded its configured deadline".to_string(),
            true,
            DiagnosticStage::Discovery,
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(diagnostic(
            ErrorCode::InternalError,
            "file discovery worker exited without a result".to_string(),
            true,
            DiagnosticStage::Discovery,
        )),
    }
}

fn discovery_issue_diagnostic(issue: &DiscoveryIssue) -> Diagnostic {
    let file_path = issue
        .path
        .as_ref()
        .filter(|path| Path::new(path).is_absolute());
    Diagnostic {
        error_code: ErrorCode::DiscoveryEntryUnreadable,
        message: truncate_chars(&issue.message, 4_096),
        retryable: true,
        stage: DiagnosticStage::Discovery,
        file_path: Nullable(file_path.cloned()),
        backend: Nullable(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_active_error(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    store: &mut ScannerStore,
    active: &ActiveRun,
    heartbeat: &mut LeaseHeartbeat,
    mut warnings: Vec<Diagnostic>,
    error: Diagnostic,
    summary: ContextSummary,
    worker_handshake_ms: u64,
    rss_tracker: &WorkerRssTracker,
    timing: &ActiveRunTiming,
) -> ScannerOperation<ContextEnvelope> {
    heartbeat.stop();
    if let Some(background_error) = heartbeat.take_background_error() {
        warnings.push(diagnostic(
            ErrorCode::CacheWriteFailed,
            format!("lease heartbeat recovered after a transient failure: {background_error}"),
            true,
            DiagnosticStage::Cache,
        ));
    }
    let discovery_ms = summary.discovery_duration_ms;
    persist_active_error_without_heartbeat(
        request,
        version,
        store,
        active,
        warnings,
        error,
        summary,
        worker_handshake_ms,
        discovery_ms,
        rss_tracker,
        timing,
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_active_error_without_heartbeat(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    store: &mut ScannerStore,
    active: &ActiveRun,
    mut warnings: Vec<Diagnostic>,
    error: Diagnostic,
    summary: ContextSummary,
    worker_handshake_ms: u64,
    discovery_ms: u64,
    rss_tracker: &WorkerRssTracker,
    timing: &ActiveRunTiming,
) -> ScannerOperation<ContextEnvelope> {
    let discovery_observed_file_count = summary.source_file_count;
    let summary = empty_summary();
    let peak_worker_rss_bytes = rss_tracker.peak_worker_rss_bytes();
    if peak_worker_rss_bytes.is_none()
        && !warnings
            .iter()
            .any(|warning| warning.error_code == ErrorCode::WorkerRssUnavailable)
    {
        warnings.push(worker_rss_unavailable_diagnostic());
    }
    warnings.truncate(100_000);
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        engine_version: version.engine_version.clone(),
        engine_build: version.engine_build.clone(),
        status: EngineStatus::Error,
        file_context: String::new(),
        summary: summary.clone(),
        scan_run_id: Nullable(Some(active.scan_run_id())),
        context_run_id: Nullable(Some(active.context_run_id())),
        warnings: warnings.clone(),
        error: Nullable(Some(error.clone())),
    };
    let envelope_json = match canonical_envelope_json(&envelope) {
        Ok(value) => value,
        Err(serialization_error) => {
            return build_error_output(
                request,
                version,
                serialization_error.diagnostic(DiagnosticStage::Internal),
                warnings,
                summary.clone(),
                Some(active.scan_run_id()),
            );
        }
    };
    let mut diagnostics: Vec<RunDiagnosticRecord> = warnings
        .iter()
        .cloned()
        .map(|diagnostic| RunDiagnosticRecord {
            severity: DiagnosticSeverity::Warning,
            diagnostic,
        })
        .collect();
    diagnostics.push(RunDiagnosticRecord {
        severity: DiagnosticSeverity::Error,
        diagnostic: error,
    });
    let batch = FinalizationBatch {
        status: RunStatus::Error,
        envelope_json: envelope_json.clone(),
        inventory: Vec::new(),
        file_results: Vec::new(),
        diagnostics,
        stage_metrics: crate::scheduler::zero_stage_metrics(),
        extension_metrics: Vec::new(),
        context: Some(ContextRunRecord {
            context_profile_hash: crate::store::sha256_hex(b"terminal_failure_v1"),
            status: RunStatus::Error,
            final_context: String::new(),
            context_sha256: crate::store::sha256_hex(b""),
            summary: summary.clone(),
            decisions: Vec::new(),
        }),
        artifact: None,
        snapshot_key: None,
        snapshot_hit: None,
        // spec Part 5.3: engine-error runs persist an authoritative metrics row
        // with the wall spans that genuinely ran (handshake + discovery), never
        // a fabricated-zero derive.
        execution_metrics: Some(assemble_engine_error_execution_metrics(
            worker_handshake_ms,
            discovery_ms,
            discovery_observed_file_count,
            timing.source_guards.metrics(),
            peak_worker_rss_bytes,
        )),
    };
    let now_ms = match current_time_millis() {
        Ok(value) => value,
        Err(time_error) => {
            return build_error_output(
                request,
                version,
                time_error.diagnostic(DiagnosticStage::Internal),
                warnings,
                summary,
                Some(active.scan_run_id()),
            );
        }
    };
    match store.finalize(active, &batch, now_ms, timing.deadlines, &timing.clock) {
        Ok(timings) => complete_committed_terminal(store, timing, timings, envelope, 1),
        Err(write_error) => abandon_after_finalization_failure(
            request,
            version,
            store,
            active,
            write_error,
            warnings,
            summary,
        ),
    }
}

fn complete_committed_terminal(
    store: &mut ScannerStore,
    timing: &ActiveRunTiming,
    timings: TerminalAuditTimings,
    envelope: ContextEnvelope,
    exit_code: i32,
) -> ScannerOperation<ContextEnvelope> {
    if timings.busy_timeout_restore_failed {
        eprintln!("scanner warning: SQLite busy timeout restore failed after terminal commit");
        return ScannerOperation {
            value: envelope,
            exit_code,
        };
    }

    // Optional GC is admitted only after the authoritative terminal COMMIT.
    // Busy, deadline overshoot, time-read, or restore failures remain redacted
    // runtime warnings and can never rewrite that committed result.
    if timing.remaining_absolute_ms() >= crate::store::cache::OPPORTUNISTIC_GC_BUDGET_MS {
        match current_time_millis() {
            Ok(now_ms) => {
                if store
                    .run_opportunistic_gc(
                        now_ms,
                        crate::store::cache::OPPORTUNISTIC_GC_BUDGET_MS,
                        &timing.clock,
                    )
                    .is_err()
                {
                    eprintln!("scanner warning: opportunistic SQLite GC was skipped");
                }
            }
            Err(_) => {
                eprintln!("scanner warning: opportunistic SQLite GC was skipped");
            }
        }
    }
    ScannerOperation {
        value: envelope,
        exit_code,
    }
}

fn abandon_after_finalization_failure(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    store: &mut ScannerStore,
    active: &ActiveRun,
    error: StoreError,
    mut warnings: Vec<Diagnostic>,
    summary: ContextSummary,
) -> ScannerOperation<ContextEnvelope> {
    let cleanup = current_time_millis().and_then(|now_ms| store.abandon_active_run(active, now_ms));
    if let Err(cleanup_error) = cleanup {
        warnings.push(cleanup_error.diagnostic(DiagnosticStage::Cache));
    }
    build_error_output(
        request,
        version,
        error.diagnostic(DiagnosticStage::Cache),
        warnings,
        summary,
        Some(active.scan_run_id()),
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_post_outcome_failure_if_invalid(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    store: &mut ScannerStore,
    active: &ActiveRun,
    batch: &FinalizationBatch,
    worker_handshake_ms: u64,
    discovery_ms: u64,
    rss_tracker: &WorkerRssTracker,
    timing: &ActiveRunTiming,
) -> Option<ScannerOperation<ContextEnvelope>> {
    let error = match store.validate_finalization_batch(active, batch) {
        Ok(()) => return None,
        Err(error) => error,
    };
    let observed_summary = batch
        .context
        .as_ref()
        .map(|context| context.summary.clone())
        .unwrap_or_else(empty_summary);
    Some(persist_active_error_without_heartbeat(
        request,
        version,
        store,
        active,
        Vec::new(),
        error.diagnostic(DiagnosticStage::Cache),
        observed_summary,
        worker_handshake_ms,
        discovery_ms,
        rss_tracker,
        timing,
    ))
}

fn build_error_output(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    error: Diagnostic,
    mut warnings: Vec<Diagnostic>,
    summary: ContextSummary,
    scan_run_id: Option<u64>,
) -> ScannerOperation<ContextEnvelope> {
    warnings.truncate(100_000);
    let response = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        engine_version: version.engine_version.clone(),
        engine_build: version.engine_build.clone(),
        status: EngineStatus::Error,
        file_context: String::new(),
        summary,
        scan_run_id: Nullable(scan_run_id),
        context_run_id: Nullable(None),
        warnings,
        error: Nullable(Some(error)),
    };
    ScannerOperation {
        value: response,
        exit_code: 1,
    }
}

struct LeaseHeartbeat {
    stop_sender: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<Option<StoreError>>>,
    background_error: Option<StoreError>,
}

impl LeaseHeartbeat {
    fn start(database_path: PathBuf, active: ActiveRun) -> Self {
        let (stop_sender, stop_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut store = None;
            let mut last_error = None;
            loop {
                match stop_receiver.recv_timeout(Duration::from_millis(HEARTBEAT_INTERVAL_MS)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return last_error,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if store.is_none() {
                            match ScannerStore::open_existing(&database_path) {
                                Ok(opened) => store = Some(opened),
                                Err(error) => {
                                    last_error = Some(error);
                                    continue;
                                }
                            }
                        }
                        let Some(store) = store.as_mut() else {
                            continue;
                        };
                        match current_time_millis()
                            .and_then(|now_ms| store.heartbeat(&active, now_ms))
                        {
                            Ok(()) => last_error = None,
                            Err(error) => last_error = Some(error),
                        }
                    }
                }
            }
        });
        Self {
            stop_sender: Some(stop_sender),
            handle: Some(handle),
            background_error: None,
        }
    }

    fn stop(&mut self) {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.handle.take() {
            self.background_error = match handle.join() {
                Ok(error) => error,
                Err(_) => Some(StoreError::CacheWrite {
                    detail: "lease heartbeat thread panicked".to_string(),
                }),
            };
        }
    }

    fn take_background_error(&mut self) -> Option<StoreError> {
        self.background_error.take()
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

fn stage_deadline_exhausted(operation: &str) -> Diagnostic {
    diagnostic(
        ErrorCode::StageDeadlineExhausted,
        format!("work deadline exhausted before {operation}"),
        true,
        DiagnosticStage::Process,
    )
}

fn elapsed_summary(started_at: Instant) -> ContextSummary {
    let mut summary = empty_summary();
    summary.total_duration_ms = elapsed_ms(started_at);
    summary
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn committed_terminal_output_survives_restore_and_opportunistic_gc_failures() {
        let directory = tempdir().unwrap();
        let db_path = directory.path().join(crate::store::SCAN_DB_FILENAME);
        let mut store = ScannerStore::open(&db_path).unwrap();
        let timing = ActiveRunTiming::start(5_000).unwrap();
        let _ = crate::store::take_test_opportunistic_gc_calls();
        let envelope = ContextEnvelope {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: "00000000-0000-4000-8000-000000000001".to_string(),
            engine_version: "test".to_string(),
            engine_build: "test".to_string(),
            status: EngineStatus::Ok,
            file_context: "committed".to_string(),
            summary: empty_summary(),
            scan_run_id: Nullable(None),
            context_run_id: Nullable(None),
            warnings: Vec::new(),
            error: Nullable(None),
        };

        let output = complete_committed_terminal(
            &mut store,
            &timing,
            TerminalAuditTimings {
                busy_timeout_restore_failed: true,
                ..TerminalAuditTimings::default()
            },
            envelope.clone(),
            0,
        );
        assert_eq!(output.value.file_context, "committed");
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            crate::store::take_test_opportunistic_gc_calls(),
            0,
            "restore failure must skip every post-commit store use"
        );

        let locker = rusqlite::Connection::open(&db_path).unwrap();
        locker.busy_timeout(Duration::from_millis(0)).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let output = complete_committed_terminal(
            &mut store,
            &timing,
            TerminalAuditTimings::default(),
            envelope,
            7,
        );
        locker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(output.value.file_context, "committed");
        assert_eq!(output.exit_code, 7);
        assert_eq!(crate::store::take_test_opportunistic_gc_calls(), 1);
    }

    #[test]
    fn local_scanner_build_uses_a_deterministic_source_fingerprint() {
        let build = engine_identity().engine_build;
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

    #[test]
    fn post_outcome_rejection_commits_a_replayable_minimal_error() {
        let directory = tempdir().expect("temporary directory should be created");
        let scan_db = directory.path().join(crate::store::SCAN_DB_FILENAME);
        let mut request: BuildContextRequest = serde_json::from_str(include_str!(
            "../../../tests/fixtures/scanner_contract/v1/request.json"
        ))
        .expect("request fixture");
        request.request_id = "00000000-0000-4000-8000-000000000001".to_string();
        request.work_dir = directory.path().to_string_lossy().into_owned();
        request.scan_db_path = scan_db.to_string_lossy().into_owned();
        request.adapters.office_worker_path = directory
            .path()
            .join("office-worker.exe")
            .to_string_lossy()
            .into_owned();
        request.adapters.python_executable = directory
            .path()
            .join("python.exe")
            .to_string_lossy()
            .into_owned();
        request.adapters.python_module_root = directory.path().to_string_lossy().into_owned();

        let profile = normalize_scanner_settings(&request.scanner_settings, request.report_mode)
            .expect("normalized scanner profile");
        let canonical =
            ScannerStore::canonicalize_request(&request, &profile).expect("canonical request");
        let runtime = AttemptRuntime::from_request(&request).expect("attempt runtime");
        let mut store = ScannerStore::open(&scan_db).expect("scanner store");
        let now_ms = current_time_millis().expect("current time");
        let active = match store
            .begin_run(&request.request_id, &canonical, &runtime, now_ms)
            .expect("begin run")
        {
            BeginRunOutcome::Started(active) => active,
            BeginRunOutcome::Stored(_) => panic!("expected a new active run"),
        };
        let office = WorkerFingerprint {
            contract: "ai_daily_worker_v2".to_string(),
            version: "0.1.0".to_string(),
            build: "office-build".to_string(),
        };
        let python = WorkerFingerprint {
            contract: "ai_daily_worker_v2".to_string(),
            version: "0.1.0".to_string(),
            build: "python-build".to_string(),
        };
        store
            .record_worker_fingerprints(&active, Some(&office), Some(&python), now_ms)
            .expect("worker fingerprints");
        let invalid_outcome = FinalizationBatch {
            status: RunStatus::Success,
            envelope_json: "{}".to_string(),
            inventory: Vec::new(),
            file_results: Vec::new(),
            diagnostics: Vec::new(),
            stage_metrics: Vec::new(),
            extension_metrics: Vec::new(),
            context: None,
            artifact: None,
            snapshot_key: None,
            snapshot_hit: None,
            execution_metrics: None,
        };

        let timing = ActiveRunTiming::start(10_000).expect("test deadline");
        let output = persist_post_outcome_failure_if_invalid(
            &request,
            &engine_identity(),
            &mut store,
            &active,
            &invalid_outcome,
            0,
            0,
            &WorkerRssTracker::default(),
            &timing,
        )
        .expect("invalid outcome must be converted");
        assert_eq!(output.exit_code, 1);

        let stored = store
            .load_terminal_envelope(active.scan_run_id())
            .unwrap_or_else(|error| {
                panic!(
                    "committed terminal failure must be replayable: {error}; output={:?}",
                    output.value.status
                )
            });
        assert_eq!(stored.envelope.scan_run_id.0, Some(active.scan_run_id()));
        assert_eq!(
            stored.envelope.context_run_id.0,
            Some(active.context_run_id())
        );
    }

    #[test]
    fn discovery_produces_a_source_guard_for_every_file() {
        let directory = tempdir().expect("temporary directory should be created");
        let path = directory.path().join("evidence.txt");
        std::fs::write(&path, "AAAA").expect("fixture file should be written");
        let mut files = vec![DiscoveredFileOut {
            file_identity: "bootstrap:evidence".to_string(),
            path: path.to_string_lossy().into_owned(),
            extension: ".txt".to_string(),
            modified_at: "2026-07-16T00:00:00+08:00".to_string(),
            size_bytes: 4,
            source_version: "mtime_ns=1:size=4".to_string(),
            source_guard_kind: None,
            source_guard_sha256: None,
        }];

        let observer = crate::source_guard::SourceGuardObserver::default();
        attach_source_guards(&mut files, &observer);

        // Every discovered file must carry a guard. Unavailable stays fail-closed
        // with a null hash; any other kind must carry a 64-char sha256.
        let kind = files[0]
            .source_guard_kind
            .as_deref()
            .expect("discovery must produce a guard kind");
        match kind {
            "unavailable" => {
                assert!(files[0].source_guard_sha256.is_none());
            }
            _ => {
                let hash = files[0]
                    .source_guard_sha256
                    .as_deref()
                    .expect("guard kind requires a hash");
                assert_eq!(hash.len(), 64);
                assert!(hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
            }
        }
    }

    #[test]
    fn guard_attachment_metrics_flow_into_engine_error_metrics() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("missing.txt");
        let observer = crate::source_guard::SourceGuardObserver::default();
        let mut files = vec![guarded_discovery_file(&missing)];

        attach_source_guards(&mut files, &observer);
        let observations = observer.metrics();
        assert_eq!(observations.content_hash_file_count, 1);
        assert_eq!(observations.unavailable_file_count, 1);
        assert_eq!(observations.bytes_read, 0);

        let metrics = assemble_engine_error_execution_metrics(1, 2, 1, observations, Some(0));
        assert_eq!(metrics.source_guard_content_hash_file_count, 1);
        assert_eq!(metrics.source_guard_unavailable_count, 1);
        assert_eq!(metrics.source_guard_bytes_read, 0);
    }

    fn guarded_discovery_file(path: &Path) -> DiscoveredFileOut {
        DiscoveredFileOut {
            file_identity: "fixture:evidence".to_string(),
            path: path.to_string_lossy().into_owned(),
            extension: ".txt".to_string(),
            modified_at: "2026-07-16T00:00:00+08:00".to_string(),
            size_bytes: 4,
            source_version: "mtime_ns=1:size=4".to_string(),
            source_guard_kind: None,
            source_guard_sha256: None,
        }
    }

    fn artifact_for_discovery(file: &DiscoveredFileOut) -> ArtifactDraft {
        ArtifactDraft {
            snapshot_eligible: true,
            final_context: "fixture".to_string(),
            context_sha256: crate::artifact::sha256_hex(b"fixture"),
            semantic_summary: SemanticSummary {
                source_file_count: 1,
                success_count: 1,
                timeout_count: 0,
                included_file_count: 1,
                omitted_file_count: 0,
                error_file_count: 0,
                input_chars: 7,
                output_chars: 7,
                reserved_chars: 7,
                rendered_chars: 7,
            },
            file_rows: vec![ArtifactFileRow {
                file_identity: file.file_identity.clone(),
                relative_path: "evidence.txt".to_string(),
                legacy_source_version: file.source_version.clone(),
                source_guard_kind: file.source_guard_kind.clone(),
                source_guard_sha256: file.source_guard_sha256.clone(),
                parse_profile_hash: "a".repeat(64),
                parse_status: ParseStatus::Success,
                parser_backend: "light_text_v2".to_string(),
                worker_lane: "rust_core".to_string(),
                truncated: false,
                content_sha256: crate::artifact::sha256_hex(b"fixture"),
                classifier: None,
            }],
            decision_rows: Vec::new(),
        }
    }

    #[test]
    fn snapshot_precheck_taints_a_source_changed_after_discovery() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("precheck.txt");
        std::fs::write(&path, "AAAA").unwrap();
        let observer = crate::source_guard::SourceGuardObserver::default();
        let mut files = vec![guarded_discovery_file(&path)];
        attach_source_guards(&mut files, &observer);

        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&path, "BBBB").unwrap();
        std::thread::sleep(Duration::from_millis(50));

        assert!(!discovery_guards_are_current(&files, &observer));
        let current = crate::source_guard::SourceGuardObserver::default()
            .compute(&path)
            .unwrap();
        assert!(
            !observer.verify(&path, &current),
            "the scheduler must inherit the precheck mismatch taint"
        );
    }

    #[test]
    fn snapshot_postcheck_rejects_a_source_changed_after_artifact_load() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("postcheck.txt");
        std::fs::write(&path, "AAAA").unwrap();
        let observer = crate::source_guard::SourceGuardObserver::default();
        let mut files = vec![guarded_discovery_file(&path)];
        attach_source_guards(&mut files, &observer);
        let artifact = artifact_for_discovery(&files[0]);

        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&path, "BBBB").unwrap();
        std::thread::sleep(Duration::from_millis(50));

        assert!(!artifact_guards_are_current(&artifact, &files, &observer));
    }

    #[test]
    fn snapshot_postcheck_rejects_artifact_guard_row_disagreement() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("artifact-row.txt");
        std::fs::write(&path, "AAAA").unwrap();
        let observer = crate::source_guard::SourceGuardObserver::default();
        let mut files = vec![guarded_discovery_file(&path)];
        attach_source_guards(&mut files, &observer);
        let mut artifact = artifact_for_discovery(&files[0]);
        artifact.file_rows[0].source_guard_kind = Some("unknown_guard".to_string());
        assert!(!artifact_guards_are_current(&artifact, &files, &observer));
        artifact.file_rows[0].source_guard_kind = files[0].source_guard_kind.clone();
        artifact.file_rows[0].source_guard_sha256 = Some("f".repeat(64));

        assert!(!artifact_guards_are_current(&artifact, &files, &observer));
        artifact.file_rows.clear();
        assert!(!artifact_guards_are_current(&artifact, &files, &observer));
    }

    #[test]
    fn snapshot_reports_the_live_handshake_peak_rss() {
        let observations = crate::source_guard::SourceGuardObservationMetrics {
            content_hash_file_count: 1,
            unavailable_file_count: 2,
            bytes_read: 3,
        };
        let metrics =
            assemble_snapshot_execution_metrics(4, observations, 0, 0, 1, 2, 3, 4, Some(64 * 1024));
        assert_eq!(metrics.discovery_observed_file_count, 4);
        assert_eq!(metrics.source_guard_content_hash_file_count, 1);
        assert_eq!(metrics.source_guard_unavailable_count, 2);
        assert_eq!(metrics.source_guard_bytes_read, 3);
        assert_eq!(metrics.peak_worker_rss_bytes.0, Some(64 * 1024));
    }

    fn successful_outcome_for_rss_test() -> BudgetedScanOutcome {
        BudgetedScanOutcome {
            scan_run_id: 1,
            terminal_intent: TerminalIntent::Success,
            inventory: Vec::new(),
            file_results: Vec::new(),
            parse_cache_receipts: Vec::new(),
            classification_cache_receipts: Vec::new(),
            classifications: std::collections::BTreeMap::new(),
            diagnostics: Vec::new(),
            stage_metrics: Vec::new(),
            extension_metrics: Vec::new(),
            context: Some(ContextRunRecord {
                context_profile_hash: "a".repeat(64),
                status: RunStatus::Success,
                final_context: String::new(),
                context_sha256: crate::store::sha256_hex(b""),
                summary: empty_summary(),
                decisions: Vec::new(),
            }),
            execution_metrics: crate::scheduler::ExecutionMetrics::default(),
        }
    }

    #[test]
    fn unavailable_worker_rss_adds_warning_without_changing_success() {
        let mut outcome = successful_outcome_for_rss_test();

        apply_worker_rss_observation(&mut outcome, None);

        assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
        assert_eq!(
            outcome.context.as_ref().map(|context| context.status),
            Some(RunStatus::Success)
        );
        assert_eq!(outcome.diagnostics.len(), 1);
        assert_eq!(outcome.diagnostics[0].severity, DiagnosticSeverity::Warning);
        assert_eq!(
            outcome.diagnostics[0].diagnostic.error_code,
            ErrorCode::WorkerRssUnavailable
        );
        assert_eq!(
            outcome.diagnostics[0].diagnostic.stage,
            DiagnosticStage::Process
        );
    }

    #[test]
    fn observed_worker_rss_keeps_success_without_warning() {
        for peak_worker_rss_bytes in [0, 64 * 1024 * 1024] {
            let mut outcome = successful_outcome_for_rss_test();

            apply_worker_rss_observation(&mut outcome, Some(peak_worker_rss_bytes));

            assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
            assert_eq!(
                outcome.context.as_ref().map(|context| context.status),
                Some(RunStatus::Success)
            );
            assert!(outcome.diagnostics.is_empty());
        }
    }

    #[test]
    fn snapshot_not_classified_by_budget_stays_not_eligible() {
        let classifier = PdfClassificationProvenanceV1 {
            status: ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget,
            page_count: None,
            result_examined_pages: Some(0),
            nominal_charged_pages: 0,
            classifier_build: "a".repeat(64),
            classifier_profile_hash: "b".repeat(64),
        };
        let artifact = ArtifactDraft::new(
            true,
            String::new(),
            SemanticSummary {
                source_file_count: 1,
                success_count: 0,
                timeout_count: 0,
                included_file_count: 0,
                omitted_file_count: 1,
                error_file_count: 0,
                input_chars: 0,
                output_chars: 0,
                reserved_chars: 0,
                rendered_chars: 0,
            },
            vec![ArtifactFileRow {
                file_identity: "fixture:budget.pdf".to_string(),
                relative_path: "budget.pdf".to_string(),
                legacy_source_version: "mtime_ns=1:size=1".to_string(),
                source_guard_kind: Some("content_sha256_v1".to_string()),
                source_guard_sha256: Some("c".repeat(64)),
                parse_profile_hash: "d".repeat(64),
                parse_status: ParseStatus::NotParsed,
                parser_backend: "not_parsed".to_string(),
                worker_lane: "not_parsed".to_string(),
                truncated: false,
                content_sha256: crate::store::sha256_hex(b""),
                classifier: Some(classifier),
            }],
            vec![ArtifactDecisionRow {
                file_identity: "fixture:budget.pdf".to_string(),
                relative_path: "budget.pdf".to_string(),
                action: ai_daily_scanner_contract::ContextAction::Omit,
                reason: "pdf_classification_page_quota_exhausted".to_string(),
                priority: 0,
                input_chars: 0,
                output_chars: 0,
                truncated: false,
                error_code: String::new(),
            }],
        )
        .expect("eligible artifact");

        let rows = snapshot_file_results(&artifact, None);
        let audit = rows[0]
            .pdf_classification
            .as_ref()
            .expect("snapshot classification audit");
        assert_eq!(
            audit.classification_cache_status,
            ai_daily_scanner_contract::ClassificationCacheStatus::NotEligible
        );
        assert_eq!(
            audit.transport,
            ai_daily_scanner_contract::ClassificationTransport::NotApplicable
        );
        assert_eq!(audit.attempt_count, 0);

        // Historical eligible artifacts created before zero-execution
        // provenance was stored can be reconstructed only from the frozen
        // quota decision and the current preflight classifier identity.
        let mut historical = artifact.clone();
        historical.file_rows[0].classifier = None;
        let identity = ClassifierIdentity {
            contract: CLASSIFIER_CONTRACT_VERSION.to_string(),
            build: "a".repeat(64),
            profile_hash: "b".repeat(64),
        };
        let rows = snapshot_file_results(&historical, Some(&identity));
        let audit = rows[0]
            .pdf_classification
            .as_ref()
            .expect("quota decision permits controlled reconstruction");
        assert_eq!(
            audit.status,
            ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
        );

        historical.decision_rows[0].reason = "semantic_file_quota_exhausted".to_string();
        let rows = snapshot_file_results(&historical, Some(&identity));
        assert!(
            rows[0].pdf_classification.is_none(),
            "a generic unclassified PDF must never be guessed as budget-excluded"
        );
    }
}
