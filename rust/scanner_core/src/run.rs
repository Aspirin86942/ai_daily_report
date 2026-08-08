use ai_daily_discovery::{
    discover_files_with_diagnostics, normalize_contract_path_text, DiscoveredFileOut,
    DiscoveryIssue, DiscoveryReport, DiscoveryRequest,
};
use ai_daily_scanner_contract::{
    BuildContextRequest, ContextEnvelope, ContextSummary, Diagnostic, DiagnosticStage, DoctorCheck,
    DoctorCheckStatus, DoctorRequest, DoctorResponse, EngineStatus, ErrorCode, InspectRunRequest,
    InspectRunResponse, InspectStatus, Nullable, RunStatus, TransportErrorResponse,
    UpgradeDatabaseRequestV1, UpgradeStatus, Validate, VersionResponse, WorkerDiagnosticV1,
    WorkerDiagnosticV1ErrorCode, WorkerDiagnosticV1Stage,
};
use chrono::NaiveDate;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::config::{normalize_scanner_profile_for_request, normalize_scanner_profile_v2};
use crate::context_audit::{context_profile_hash, rejected_profile_hash, InspectAuditError};
use crate::parsers::{
    document, office, register_worker, register_worker_pair, RegisteredWorker, WorkerCommand,
    WorkerRegistry, WORKER_CONTRACT_VERSION, WORKER_HANDSHAKE_TIMEOUT,
};
use crate::parsers::classifier::ClassifierPort;
use crate::scheduler::{
    BudgetedContextScheduler, RealClock, RealGuardVerifier, ScheduledRunInput, WorkerIdentities,
};
use crate::scheduler_adapter::{ProductionParser, StoreCachePort};
use crate::source_guard::{
    compute_source_guard, source_guard_kind_text, SourceGuardKind, SourceGuardV2,
};
use crate::store::{
    canonical_envelope_json, current_time_millis, ActiveRun, AttemptRuntime, BeginRunOutcome,
    DiagnosticSeverity, FinalizationBatch, RouteStackFingerprint, RouteStackFingerprints,
    RunDiagnosticRecord, ScannerStore, StoreError, WorkerFingerprint, HEARTBEAT_INTERVAL_MS,
};

#[derive(Debug, Error)]
pub enum EngineShellError {
    #[error("failed to serialize scanner response: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug)]
pub struct CommandOutput {
    pub json: String,
    pub exit_code: i32,
}

impl CommandOutput {
    fn success<T: Serialize>(payload: &T) -> Result<Self, EngineShellError> {
        Ok(Self {
            json: serde_json::to_string(payload)?,
            exit_code: 0,
        })
    }

    fn with_exit<T: Serialize>(payload: &T, exit_code: i32) -> Result<Self, EngineShellError> {
        Ok(Self {
            json: serde_json::to_string(payload)?,
            exit_code,
        })
    }

    fn canonical_json(json: String, exit_code: i32) -> Result<Self, EngineShellError> {
        let _: serde_json::Value = serde_json::from_str(&json)?;
        Ok(Self { json, exit_code })
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
            build_context_command(&request)
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
            inspect_run_command(&request)
        }
        "upgrade-db" => {
            let request = match decode_request::<UpgradeDatabaseRequestV1>(input) {
                Ok(request) => request,
                Err(_) => return invalid_request_output(),
            };
            upgrade_database_command(&request)
        }
        _ => invalid_request_output(),
    }
}

fn upgrade_database_command(
    request: &UpgradeDatabaseRequestV1,
) -> Result<CommandOutput, EngineShellError> {
    let response = ScannerStore::upgrade_database(request);
    debug_assert!(
        response.validate().is_ok(),
        "upgrade-db response violates the wire contract"
    );
    let exit_code = if response.status == UpgradeStatus::Error {
        1
    } else {
        0
    };
    CommandOutput::with_exit(&response, exit_code)
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

fn inspect_run_command(request: &InspectRunRequest) -> Result<CommandOutput, EngineShellError> {
    let mut store = match ScannerStore::open_existing(Path::new(&request.scan_db_path)) {
        Ok(store) => store,
        Err(error) => {
            return inspect_error_output(
                request,
                error.error_code(),
                error.to_string(),
                error.retryable(),
                None,
            );
        }
    };
    match store.inspect_run(request.scan_run_id, request.include_content) {
        Ok(snapshot) => {
            let response = InspectRunResponse {
                contract: "ai_daily_context".to_string(),
                protocol_version: 1,
                request_id: request.request_id.clone(),
                scan_run_id: request.scan_run_id,
                context_run_id: Nullable(snapshot.context_run_id),
                status: InspectStatus::Ok,
                run_status: Nullable(Some(snapshot.run_status)),
                summary: snapshot.summary,
                stage_metrics: snapshot.stage_metrics,
                extension_metrics: snapshot.extension_metrics,
                files: snapshot.files,
                decisions: snapshot.decisions,
                warnings: snapshot.warnings,
                error: Nullable(None),
            };
            CommandOutput::success(&response)
        }
        Err(error) => {
            let (error_code, retryable) = match &error.error {
                InspectAuditError::RunNotFound => (ErrorCode::RunNotFound, false),
                InspectAuditError::RunCorrupt(_) => (ErrorCode::RunCorrupt, false),
                InspectAuditError::ContentForbidden => (ErrorCode::InvalidRequest, false),
                InspectAuditError::Sql(_) => (ErrorCode::CacheOpenFailed, true),
            };
            inspect_error_output(
                request,
                error_code,
                error.error.to_string(),
                retryable,
                error.run_status,
            )
        }
    }
}

fn inspect_error_output(
    request: &InspectRunRequest,
    error_code: ErrorCode,
    message: String,
    retryable: bool,
    run_status: Option<RunStatus>,
) -> Result<CommandOutput, EngineShellError> {
    let response = InspectRunResponse {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        scan_run_id: request.scan_run_id,
        context_run_id: Nullable(None),
        status: InspectStatus::Error,
        run_status: Nullable(run_status),
        summary: empty_summary(),
        stage_metrics: Vec::new(),
        extension_metrics: Vec::new(),
        files: Vec::new(),
        decisions: Vec::new(),
        warnings: Vec::new(),
        error: Nullable(Some(Diagnostic {
            error_code,
            message: truncate_chars(&message, 4_096),
            retryable,
            stage: DiagnosticStage::Inspect,
            file_path: Nullable(None),
            backend: Nullable(None),
        })),
    };
    CommandOutput::with_exit(&response, 1)
}

fn build_context_command(request: &BuildContextRequest) -> Result<CommandOutput, EngineShellError> {
    let started_at = Instant::now();
    let version = version_response();
    let work_dir = match validate_build_work_dir(&request.work_dir) {
        Ok(path) => path,
        Err(error) => {
            return build_error_output(request, &version, error, Vec::new(), empty_summary(), None);
        }
    };
    let profile = match normalize_scanner_profile_for_request(&request.scanner_profile, request.report_mode) {
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
    let v2_profile = match normalize_scanner_profile_v2(&request.scanner_profile, request.report_mode) {
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
    let runtime = match AttemptRuntime::from_request(request, &version) {
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
    let mut store = match ScannerStore::open(Path::new(&request.scan_db_path)) {
        Ok(store) => store,
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
            return CommandOutput::canonical_json(stored.envelope_json, exit_code);
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
    let mut heartbeat = LeaseHeartbeat::start(PathBuf::from(&request.scan_db_path), active.clone());
    execute_active_build(
        request,
        &version,
        &profile,
        &v2_profile,
        &work_dir,
        &mut store,
        &active,
        &mut heartbeat,
        started_at,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_active_build(
    request: &BuildContextRequest,
    version: &VersionResponse,
    profile: &ai_daily_scanner_contract::NormalizedScannerProfileV1,
    v2_profile: &ai_daily_scanner_contract::NormalizedScannerProfileV2,
    work_dir: &Path,
    store: &mut ScannerStore,
    active: &ActiveRun,
    heartbeat: &mut LeaseHeartbeat,
    started_at: Instant,
) -> Result<CommandOutput, EngineShellError> {
    // ---- bounded parallel worker handshakes (spec Solution run.rs order) ----
    let office_command = office::worker_command(&request.adapters);
    let python_command = document::worker_command(&request.adapters);
    let (office_result, python_result) =
        register_worker_pair(&office_command, &python_command, WORKER_HANDSHAKE_TIMEOUT);
    let (office_worker, office_error) = split_handshake(office_result, "rust_office_oxide_v1");
    let (python_worker, python_error) = split_handshake(python_result, "python_office_v1");
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
        );
    };
    let registry = WorkerRegistry {
        office: Some(office_worker.clone()),
        python_document: Some(python_worker.clone()),
    };
    let route_stacks = match route_stack_fingerprints(version, profile, &office_worker, &python_worker)
    {
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
            );
        }
    };

    // ---- discovery (with engine-owned SourceGuardV2) ----
    let discovery_started = Instant::now();
    let mut discovery = match discover_with_timeout(work_dir, request, profile) {
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
            );
        }
    };
    let discovery_duration_ms = elapsed_ms(discovery_started);
    let mut warnings: Vec<Diagnostic> = discovery
        .issues
        .iter()
        .map(discovery_issue_diagnostic)
        .collect();
    attach_source_guards(&mut discovery.files);

    // ---- assemble + execute the deep-module scheduler ----
    let classifier_command = document::worker_command(&request.adapters);
    let classifier_port = ClassifierPort::new(classifier_command);
    let parser_port = ProductionParser::new(profile, registry);
    let cache_port = StoreCachePort::new(
        PathBuf::from(&request.scan_db_path),
        route_stacks,
        profile.clone(),
    );
    let clock = RealClock::new();
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
        classifier_build: Some(python_worker.identity.worker_build.clone()),
    };
    let input = match ScheduledRunInput::new(
        active.scan_run_id(),
        started_at_ms,
        work_dir.to_string_lossy().into_owned(),
        discovery.files,
        discovery.issues,
        v2_profile.clone(),
        worker_identities,
        version.engine_version.clone(),
        version.engine_build.clone(),
        context_profile_hash,
        rejected_profile_hash,
        discovery_duration_ms,
        &clock,
    ) {
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
            );
        }
    };
    let scheduler = BudgetedContextScheduler::new(
        Box::new(classifier_port),
        Box::new(parser_port),
        Box::new(cache_port),
        Box::new(clock),
        Box::new(RealGuardVerifier),
    );
    let outcome = match scheduler.execute(input) {
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
            );
        }
    };

    // ---- terminal finalization (the ONLY linearization point) ----
    let (envelope, run_status) = scheduler_outcome_envelope(request, version, active, &outcome);
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
            );
        }
    };
    let batch = FinalizationBatch {
        status: run_status,
        envelope_json: envelope_json.clone(),
        inventory: outcome.inventory,
        cache_writes: outcome.parse_cache_receipts,
        file_results: outcome.file_results,
        diagnostics: outcome.diagnostics,
        stage_metrics: outcome.stage_metrics,
        extension_metrics: outcome.extension_metrics,
        context: outcome.context,
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
    match store.finalize(active, &batch, finalize_now_ms) {
        Ok(()) => CommandOutput::canonical_json(envelope_json, exit_code),
        Err(error) => build_error_output(
            request,
            version,
            error.diagnostic(DiagnosticStage::Cache),
            warnings,
            batch
                .context
                .as_ref()
                .map(|context| context.summary.clone())
                .unwrap_or_else(empty_summary),
            Some(active.scan_run_id()),
        ),
    }
}

/// Rebuilds the frozen `ContextEnvelope` from a scheduler outcome. The caller
/// must not re-decide actions/counts/admission; this only projects the outcome
/// onto the envelope shape and the terminal record.
fn scheduler_outcome_envelope(
    request: &BuildContextRequest,
    version: &VersionResponse,
    active: &ActiveRun,
    outcome: &crate::scheduler::BudgetedScanOutcome,
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
    let (file_context, summary, context_run_id) = match &outcome.context {
        Some(context) => (
            context.final_context.clone(),
            context.summary.clone(),
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
    version: &VersionResponse,
    profile: &ai_daily_scanner_contract::NormalizedScannerProfileV1,
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

/// Computes the engine-owned SourceGuardV2 for every discovered file and
/// carries it on the discovery output so cache/snapshot identity can consume
/// it. A hard guard I/O error leaves the file unavailable (fail closed), never
/// an invented metadata identity.
fn attach_source_guards(files: &mut [DiscoveredFileOut]) {
    for file in files {
        let guard = compute_source_guard(Path::new(&file.path)).unwrap_or(SourceGuardV2 {
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
    profile: &ai_daily_scanner_contract::NormalizedScannerProfileV1,
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
    match receiver.recv_timeout(Duration::from_millis(
        profile.execution.discovery_timeout_ms,
    )) {
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
    version: &VersionResponse,
    store: &mut ScannerStore,
    active: &ActiveRun,
    heartbeat: &mut LeaseHeartbeat,
    mut warnings: Vec<Diagnostic>,
    error: Diagnostic,
    summary: ContextSummary,
) -> Result<CommandOutput, EngineShellError> {
    heartbeat.stop();
    if let Some(background_error) = heartbeat.take_background_error() {
        warnings.push(diagnostic(
            ErrorCode::CacheWriteFailed,
            format!("lease heartbeat recovered after a transient failure: {background_error}"),
            true,
            DiagnosticStage::Cache,
        ));
    }
    persist_active_error_without_heartbeat(
        request, version, store, active, warnings, error, summary,
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_active_error_without_heartbeat(
    request: &BuildContextRequest,
    version: &VersionResponse,
    store: &mut ScannerStore,
    active: &ActiveRun,
    mut warnings: Vec<Diagnostic>,
    error: Diagnostic,
    summary: ContextSummary,
) -> Result<CommandOutput, EngineShellError> {
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
        context_run_id: Nullable(None),
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
                summary,
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
        cache_writes: Vec::new(),
        file_results: Vec::new(),
        diagnostics,
        stage_metrics: Vec::new(),
        extension_metrics: Vec::new(),
        context: None,
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
    match store.finalize(active, &batch, now_ms) {
        Ok(()) => CommandOutput::canonical_json(envelope_json, 1),
        Err(write_error) => build_error_output(
            request,
            version,
            write_error.diagnostic(DiagnosticStage::Cache),
            warnings,
            summary,
            Some(active.scan_run_id()),
        ),
    }
}

fn build_error_output(
    request: &BuildContextRequest,
    version: &VersionResponse,
    error: Diagnostic,
    mut warnings: Vec<Diagnostic>,
    summary: ContextSummary,
    scan_run_id: Option<u64>,
) -> Result<CommandOutput, EngineShellError> {
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
    CommandOutput::with_exit(&response, 1)
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
        error: WorkerDiagnosticV1 {
            error_code: WorkerDiagnosticV1ErrorCode::InvalidRequest,
            message: "command request could not be decoded".to_string(),
            retryable: false,
            stage: WorkerDiagnosticV1Stage::Request,
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

        attach_source_guards(&mut files);

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
}
