//! Rust-owned scanner inventory, successful parse cache, and run audit store.

pub mod cache;
pub mod inventory;
pub mod schema;

use ai_daily_discovery::normalize_contract_path_text;
use ai_daily_scanner_contract::{
    AutoVacuumMode, BuildContextRequest, ContextAction, ContextDecision,
    ContextEnvelope, ContextSummary, Diagnostic, DiagnosticStage, EngineStatus, ErrorCode,
    ExecutionMetricsV2, ExtensionMetric, MaintenanceDeletedV1, MaintenanceMode,
    MaintenancePostIntegrityCheck, MaintenancePreIntegrityCheck, MaintenanceRequestV1,
    MaintenanceResponseV1, MaintenanceSizeV1, MaintenanceStatus, MaintenanceVacuumStatus,
    MaintenanceVacuumV1, NormalizedScannerProfileV1, Nullable, ParseStatus, RunStatus, StageMetric,
    StageName, UpgradeDatabaseRequestV1, UpgradeDatabaseResponseV1, UpgradeIntegrityCheck,
    UpgradeStatus, Validate, VersionResponse,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::artifact::{ArtifactDecisionRow, ArtifactDraft, ArtifactFileRow, SnapshotKeyParts};

pub use cache::{
    classifier_profile_hash, parse_profile_hash, sha256_hex, CacheAwarePlanEntry, CacheEntry,
    CacheLookup, CacheWriteRecord, ClassificationCacheEntry, ClassificationCacheLookup,
    ClassificationCacheMissReason, ClassificationCacheWriteRecord, RouteStackFingerprint,
    RouteStackFingerprints, CLASSIFIER_PROFILE_HASH_ALGORITHM, PARSE_PROFILE_HASH_ALGORITHM,
};
pub use inventory::{FileResultRecord, InventoryRecord};
pub use schema::{BUSY_TIMEOUT_MS, LATEST_USER_VERSION};

pub const SCAN_DB_FILENAME: &str = "scan_index_v2.sqlite3";
pub const REQUEST_HASH_ALGORITHM: &str = "sha256-request-v1";
pub const HEARTBEAT_INTERVAL_MS: u64 = 5_000;
pub const LEASE_GRACE_MS: u64 = 20_000;
const _: () = assert!(LEASE_GRACE_MS >= HEARTBEAT_INTERVAL_MS * 3);
const _: () = assert!(LEASE_GRACE_MS > BUSY_TIMEOUT_MS);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("scanner request is invalid: {0}")]
    InvalidRequest(String),
    #[error("scanner database could not be opened")]
    CacheOpen { detail: String },
    #[error("scanner database transaction failed")]
    CacheWrite { detail: String },
    #[error("another scanner run owns the database lease")]
    ScanAlreadyRunning,
    #[error("this request is already running")]
    RequestInProgress,
    #[error("request id was reused with different logical input")]
    RequestIdConflict,
    #[error("persisted scanner run is corrupt: {0}")]
    RunCorrupt(String),
    #[error("scanner run was not found")]
    RunNotFound,
    #[error("scanner run lease ownership was lost")]
    LeaseLost,
    #[error("scanner database schema requires an explicit upgrade")]
    SchemaUpgradeRequired,
    #[error("scanner database schema is newer than this engine")]
    SchemaTooNew,
    #[error("artifact is not dedup-compatible with the stored snapshot: {0}")]
    ArtifactMismatch(String),
}

impl StoreError {
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::CacheOpen { .. } => ErrorCode::CacheOpenFailed,
            Self::CacheWrite { .. } | Self::LeaseLost => ErrorCode::CacheWriteFailed,
            Self::ScanAlreadyRunning => ErrorCode::ScanAlreadyRunning,
            Self::RequestInProgress => ErrorCode::RequestInProgress,
            Self::RequestIdConflict => ErrorCode::RequestIdConflict,
            Self::RunCorrupt(_) => ErrorCode::RunCorrupt,
            Self::RunNotFound => ErrorCode::RunNotFound,
            Self::SchemaUpgradeRequired | Self::SchemaTooNew => ErrorCode::SchemaUpgradeRequired,
            Self::ArtifactMismatch(_) => ErrorCode::BudgetModelMismatch,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::CacheOpen { .. }
                | Self::CacheWrite { .. }
                | Self::ScanAlreadyRunning
                | Self::RequestInProgress
                | Self::LeaseLost
        )
    }

    pub fn diagnostic(&self, stage: DiagnosticStage) -> Diagnostic {
        Diagnostic {
            error_code: self.error_code(),
            message: self.to_string(),
            retryable: self.retryable(),
            stage,
            file_path: Nullable(None),
            backend: Nullable(None),
        }
    }
}

#[derive(Debug)]
pub struct ScannerStore {
    connection: Connection,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRequest {
    request_id: String,
    json: String,
    hash_algorithm: &'static str,
    hash: String,
}

impl CanonicalRequest {
    pub fn json(&self) -> &str {
        &self.json
    }

    pub fn hash_algorithm(&self) -> &'static str {
        self.hash_algorithm
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }
}

#[derive(Serialize)]
struct CanonicalLogicalRequest<'a> {
    contract: &'a str,
    protocol_version: u64,
    work_dir: &'a str,
    start_date: &'a str,
    end_date: &'a str,
    report_mode: ai_daily_scanner_contract::ReportMode,
    scanner_profile: CanonicalScannerProfile<'a>,
    context_profile: &'a ai_daily_scanner_contract::ContextProfile,
}

#[derive(Serialize)]
struct CanonicalScannerProfile<'a> {
    schema_version: &'a str,
    parser_profile_version: &'a str,
    discovery: &'a ai_daily_scanner_contract::DiscoveryProfile,
    execution: &'a ai_daily_scanner_contract::ExecutionProfile,
    parse: &'a ai_daily_scanner_contract::ParseProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRuntime {
    normalized_scan_db_path: String,
    normalized_office_worker_path: String,
    normalized_python_executable: String,
    normalized_python_module_root: String,
    python_document_worker_module: String,
    engine_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineFingerprint {
    contract: String,
    protocol_version: u64,
    binary_name: String,
    engine_version: String,
    engine_build: String,
    target_triple: String,
}

impl AttemptRuntime {
    pub fn from_request(
        request: &BuildContextRequest,
        version: &VersionResponse,
    ) -> Result<Self, StoreError> {
        version.validate().map_err(StoreError::InvalidRequest)?;
        let engine_fingerprint = serde_json::to_string(&EngineFingerprint {
            contract: version.contract.clone(),
            protocol_version: version.protocol_version,
            binary_name: version.binary_name.clone(),
            engine_version: version.engine_version.clone(),
            engine_build: version.engine_build.clone(),
            target_triple: version.target_triple.clone(),
        })
        .map_err(|error| StoreError::InvalidRequest(error.to_string()))?;
        Ok(Self {
            normalized_scan_db_path: normalize_runtime_path(&request.scan_db_path),
            normalized_office_worker_path: normalize_runtime_path(
                &request.adapters.office_worker_path,
            ),
            normalized_python_executable: normalize_runtime_path(
                &request.adapters.python_executable,
            ),
            normalized_python_module_root: normalize_runtime_path(
                &request.adapters.python_module_root,
            ),
            python_document_worker_module: request.adapters.python_document_worker_module.clone(),
            engine_fingerprint,
        })
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.normalized_scan_db_path.is_empty()
            || !is_contract_absolute_text(&self.normalized_scan_db_path)
            || self.normalized_office_worker_path.is_empty()
            || !is_contract_absolute_text(&self.normalized_office_worker_path)
            || self.normalized_python_executable.is_empty()
            || !is_contract_absolute_text(&self.normalized_python_executable)
            || self.normalized_python_module_root.is_empty()
            || !is_contract_absolute_text(&self.normalized_python_module_root)
            || self.python_document_worker_module.is_empty()
            || self.python_document_worker_module.chars().count() > 1_024
            || self.engine_fingerprint.is_empty()
            || self.engine_fingerprint.chars().count() > 4_096
        {
            return Err(StoreError::InvalidRequest(
                "attempt runtime fingerprint is incomplete".to_string(),
            ));
        }
        Ok(())
    }
}

fn normalize_runtime_path(value: &str) -> String {
    if let Ok(canonical) = fs::canonicalize(value) {
        return normalize_contract_path_text(&canonical.to_string_lossy());
    }
    let mut normalized = PathBuf::new();
    for component in Path::new(value).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalize_contract_path_text(&normalized.to_string_lossy())
}

fn is_contract_absolute_text(value: &str) -> bool {
    let bytes = value.as_bytes();
    let drive_rooted = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    (value.starts_with('/') || value.starts_with("\\\\") || drive_rooted)
        && !value.contains('\0')
        && value.chars().count() <= 32_767
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerFingerprint {
    pub contract: String,
    pub version: String,
    pub build: String,
}

impl WorkerFingerprint {
    fn validate(&self) -> Result<(), StoreError> {
        if self.contract.is_empty()
            || self.contract.chars().count() > 1_024
            || self.version.is_empty()
            || self.version.chars().count() > 1_024
            || self.build.is_empty()
            || self.build.chars().count() > 4_096
        {
            Err(StoreError::InvalidRequest(
                "worker fingerprint is incomplete".to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    scan_run_id: i64,
    attempt_number: i64,
    owner_id: String,
    request_id: String,
}

impl ActiveRun {
    pub fn scan_run_id(&self) -> u64 {
        self.scan_run_id as u64
    }

    pub fn context_run_id(&self) -> u64 {
        self.scan_run_id()
    }

    pub fn attempt_number(&self) -> u64 {
        self.attempt_number as u64
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEnvelope {
    pub scan_run_id: u64,
    pub envelope_json: String,
    pub envelope: ContextEnvelope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginRunOutcome {
    Started(ActiveRun),
    Stored(Box<StoredEnvelope>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

impl DiagnosticSeverity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDiagnosticRecord {
    pub severity: DiagnosticSeverity,
    pub diagnostic: Diagnostic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextDecisionRecord {
    pub file_identity: String,
    pub decision: ContextDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRunRecord {
    pub context_profile_hash: String,
    pub status: RunStatus,
    pub final_context: String,
    pub context_sha256: String,
    pub summary: ContextSummary,
    pub decisions: Vec<ContextDecisionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizationBatch {
    pub status: RunStatus,
    /// Exact canonical JSON that is returned to Python and reused for retries.
    pub envelope_json: String,
    pub inventory: Vec<InventoryRecord>,
    pub cache_writes: Vec<CacheWriteRecord>,
    pub file_results: Vec<FileResultRecord>,
    pub diagnostics: Vec<RunDiagnosticRecord>,
    pub stage_metrics: Vec<StageMetric>,
    pub extension_metrics: Vec<ExtensionMetric>,
    pub context: Option<ContextRunRecord>,
    /// Success/Partial artifact draft to persist (spec Part 5.1). `None` for
    /// Error runs, which carry no artifact.
    pub artifact: Option<ArtifactDraft>,
    /// Canonical snapshot-key parts for an eligible artifact (spec Part 5.4).
    /// `Some` exactly when the artifact is `snapshot_eligible`.
    pub snapshot_key: Option<SnapshotKeyParts>,
    /// Snapshot-hit reference: when set, the current run reuses the referenced
    /// artifact instead of writing a new one (`snapshot_hit=1` +
    /// `reused_from_context_run_id` are recorded on the current `context_runs`).
    pub snapshot_hit: Option<SnapshotHitRef>,
    /// Authoritative `execution_metrics` (spec Part 5.3), written at finalize and
    /// read back by inspect v2. `None` only for non-production test batches.
    pub execution_metrics: Option<ExecutionMetricsV2>,
}

/// Sub-span wall timings and logical row count captured inside the terminal
/// transaction (spec Part 5.3 `execution_metrics`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalAuditTimings {
    pub current_run_audit_write_ms: u64,
    pub terminal_precommit_ms: u64,
    pub envelope_rebuild_ms: u64,
    pub terminal_rows_written: u64,
}

/// A snapshot lookup hit: the eligible artifact and the committed Success source
/// run selected by `(finished_at_ms DESC, context_run_id DESC)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHit {
    pub artifact_id: i64,
    pub source_context_run_id: i64,
}

/// The reference a snapshot-hit current run writes onto its `context_runs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHitRef {
    pub artifact_id: i64,
    pub reused_from_context_run_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedRunStatus {
    Running,
    Success,
    Partial,
    Error,
    Abandoned,
}

impl PersistedRunStatus {
    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "partial" => Ok(Self::Partial),
            "error" => Ok(Self::Error),
            "abandoned" => Ok(Self::Abandoned),
            _ => Err(StoreError::RunCorrupt("unknown run status".to_string())),
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Partial | Self::Error)
    }
}

#[derive(Debug)]
struct ExistingRun {
    scan_run_id: i64,
    canonical_request_json: String,
    request_hash: String,
    status: PersistedRunStatus,
    /// Body-free `final_envelope_metadata_json` (spec Part 5.1): the full
    /// `ContextEnvelope` is rebuilt from metadata + summary + artifact.
    final_envelope_metadata_json: Option<String>,
}

#[derive(Debug)]
struct ExistingLease {
    owner_id: String,
    heartbeat_at_ms: i64,
    expires_at_ms: i64,
}

#[derive(Debug)]
struct RawDiagnosticRow {
    severity: String,
    error_code: String,
    message: String,
    retryable: bool,
    stage: String,
    file_path: Option<String>,
    backend: Option<String>,
}

impl ScannerStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        validate_database_path(path)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut connection = Connection::open_with_flags(path, flags).map_err(cache_open)?;
        schema::configure_connection(&connection).map_err(|error| cache_open(error.to_string()))?;
        // A committed v1 database must not be auto-migrated by a business open;
        // only the separately-authorized `upgrade-db apply=true` may migrate it.
        ensure_schema_openable(&connection)?;
        schema::migrate(&mut connection).map_err(|error| cache_open(error.to_string()))?;
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    pub fn open_existing(path: &Path) -> Result<Self, StoreError> {
        validate_database_path(path)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut connection = Connection::open_with_flags(path, flags).map_err(cache_open)?;
        schema::configure_connection(&connection).map_err(|error| cache_open(error.to_string()))?;
        ensure_schema_openable(&connection)?;
        schema::migrate(&mut connection).map_err(|error| cache_open(error.to_string()))?;
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    /// Executes the `upgrade-db` command: a read-only audit (`apply=false`) or
    /// the only production upgrade entry (`apply=true`). No backup is created;
    /// rollback is operator-managed from a pre-upgrade copy (spec Part 8.3).
    pub fn upgrade_database(request: &UpgradeDatabaseRequestV1) -> UpgradeDatabaseResponseV1 {
        if request.apply {
            upgrade_database_apply(request)
        } else {
            upgrade_database_audit(request)
        }
    }

    /// Executes the `maintenance` command (spec Part 4/5.3): exclusive lease →
    /// before sizes → pre integrity → mode preflight → (dry-run ends) → deep row
    /// GC transaction → selected vacuum → post integrity → after sizes. Only
    /// `gc`/`incremental_vacuum`; `full_vacuum` is intentionally removed.
    pub fn maintenance(request: &MaintenanceRequestV1) -> MaintenanceResponseV1 {
        maintenance_command(request)
    }

    /// Batch `last_accessed_bucket` update for parse/classification cache hits
    /// (spec Part 4): same row at most once/day, all hits in one transaction.
    pub fn touch_cache_access(
        &mut self,
        now_ms: u64,
        parse_hits: &[String],
        classification_hits: &[String],
    ) -> Result<(), StoreError> {
        let now_ms = checked_i64(now_ms, "cache touch timestamp")?;
        if parse_hits.is_empty() && classification_hits.is_empty() {
            return Ok(());
        }
        let bucket = cache::date_bucket_for_ms(now_ms);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        cache::touch_parse_cache_access(&transaction, parse_hits, &bucket).map_err(cache_write)?;
        cache::touch_classification_cache_access(&transaction, classification_hits, &bucket)
            .map_err(cache_write)?;
        transaction.commit().map_err(cache_write)
    }

    /// Opportunistic age/orphan row GC (spec Part 4): runs after the terminal
    /// COMMIT only when `remaining_to_absolute_deadline >= 10ms`. Independent
    /// transaction with `busy_timeout=0`, bounded indexed delete batches, and a
    /// 10ms admission budget checked before each statement. It only forms a
    /// freelist (no vacuum) and never rewrites the committed terminal result.
    pub fn run_opportunistic_gc(&mut self, now_ms: u64, budget_ms: u64) -> Result<(), StoreError> {
        let now_ms = checked_i64(now_ms, "opportunistic gc timestamp")?;
        let budget_ms = checked_i64(budget_ms, "opportunistic gc budget")?;
        if budget_ms <= 0 {
            return Ok(());
        }
        self.connection
            .busy_timeout(Duration::from_millis(0))
            .map_err(cache_open)?;
        let outcome = (|| -> Result<(), StoreError> {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Deferred)
                .map_err(cache_write)?;
            let started = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(now_ms);
            let deadline = started.saturating_add(budget_ms);
            // Orphan artifacts first (zero `context_runs` references).
            loop {
                if remaining_opportunistic_ms(deadline) <= 0 {
                    break;
                }
                let deleted = transaction
                    .execute(
                        "DELETE FROM context_artifacts WHERE artifact_id IN (
                            SELECT artifact_id FROM context_artifacts
                            WHERE NOT EXISTS(
                                SELECT 1 FROM context_runs
                                WHERE context_runs.artifact_id = context_artifacts.artifact_id
                            )
                            LIMIT 64
                         )",
                        [],
                    )
                    .map_err(cache_write)?;
                if deleted == 0 {
                    break;
                }
            }
            // Aged terminal runs (>90 days) by finished_at_ms ASC.
            let cutoff = now_ms.saturating_sub(cache::TERMINAL_RUN_MAX_AGE_DAYS * 86_400_000);
            loop {
                if remaining_opportunistic_ms(deadline) <= 0 {
                    break;
                }
                let deleted = transaction
                    .execute(
                        "DELETE FROM scan_runs WHERE scan_run_id IN (
                            SELECT scan_run_id FROM scan_runs
                            WHERE status IN ('success', 'partial', 'error', 'abandoned')
                              AND finished_at_ms IS NOT NULL AND finished_at_ms < ?1
                            ORDER BY finished_at_ms ASC, scan_run_id ASC
                            LIMIT 64
                         )",
                        params![cutoff],
                    )
                    .map_err(cache_write)?;
                if deleted == 0 {
                    break;
                }
            }
            transaction.commit().map_err(cache_write)
        })();
        let _ = self
            .connection
            .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));
        outcome
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn canonicalize_request(
        request: &BuildContextRequest,
        normalized_profile: &NormalizedScannerProfileV1,
    ) -> Result<CanonicalRequest, StoreError> {
        request.validate().map_err(StoreError::InvalidRequest)?;
        normalized_profile
            .validate()
            .map_err(StoreError::InvalidRequest)?;
        if request.report_mode != normalized_profile.report_mode {
            return Err(StoreError::InvalidRequest(
                "request and normalized report modes disagree".to_string(),
            ));
        }
        let canonical_work_dir = fs::canonicalize(&request.work_dir)
            .map_err(|_| StoreError::InvalidRequest("work_dir is unavailable".to_string()))?;
        let canonical_work_dir =
            normalize_contract_path_text(&canonical_work_dir.to_string_lossy());
        let canonical = CanonicalLogicalRequest {
            contract: &request.contract,
            protocol_version: request.protocol_version,
            work_dir: &canonical_work_dir,
            start_date: &request.start_date,
            end_date: &request.end_date,
            report_mode: request.report_mode,
            scanner_profile: CanonicalScannerProfile {
                schema_version: &normalized_profile.schema_version,
                parser_profile_version: &normalized_profile.parser_profile_version,
                discovery: &normalized_profile.discovery,
                execution: &normalized_profile.execution,
                parse: &normalized_profile.parse,
            },
            context_profile: &normalized_profile.context,
        };
        let json = serde_json::to_string(&canonical).map_err(|error| {
            StoreError::InvalidRequest(format!("canonical request serialization failed: {error}"))
        })?;
        let hash = cache::domain_hash(b"request-v1\0", json.as_bytes());
        Ok(CanonicalRequest {
            request_id: request.request_id.clone(),
            json,
            hash_algorithm: REQUEST_HASH_ALGORITHM,
            hash,
        })
    }

    pub fn begin_run(
        &mut self,
        request_id: &str,
        canonical: &CanonicalRequest,
        runtime: &AttemptRuntime,
        now_ms: u64,
    ) -> Result<BeginRunOutcome, StoreError> {
        runtime.validate()?;
        let now_ms = checked_i64(now_ms, "run timestamp")?;
        if canonical.hash_algorithm != REQUEST_HASH_ALGORITHM
            || canonical.request_id != request_id
            || !inventory::is_sha256(&canonical.hash)
            || canonical.json.is_empty()
            || cache::domain_hash(b"request-v1\0", canonical.json.as_bytes()) != canonical.hash
        {
            return Err(StoreError::InvalidRequest(
                "canonical request fingerprint is invalid".to_string(),
            ));
        }
        if normalize_runtime_path(&self.path.to_string_lossy()) != runtime.normalized_scan_db_path {
            return Err(StoreError::InvalidRequest(
                "attempt scan database path does not match the opened store".to_string(),
            ));
        }

        if let Some(existing) = query_existing_run(&self.connection, request_id)? {
            ensure_request_hash(&existing, canonical)?;
            if existing.status.is_terminal() {
                return load_stored_envelope(&self.connection, existing, request_id);
            }
        }

        let owner_id = random_owner_id(&self.connection)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        let mut existing = query_existing_run(&transaction, request_id)?;
        if let Some(value) = &existing {
            ensure_request_hash(value, canonical)?;
            if value.status.is_terminal() {
                let stored = load_stored_envelope_ref(&transaction, value, request_id)?;
                transaction.commit().map_err(cache_write)?;
                return Ok(BeginRunOutcome::Stored(Box::new(stored)));
            }
        }

        let lease = query_lease(&transaction)?;
        if let Some(active_lease) = &lease {
            if lease_is_live(active_lease, now_ms) {
                if existing
                    .as_ref()
                    .is_some_and(|run| run.status == PersistedRunStatus::Running)
                {
                    return Err(StoreError::RequestInProgress);
                }
                return Err(StoreError::ScanAlreadyRunning);
            }
            reclaim_expired_lease(&transaction, active_lease, now_ms)?;
            if existing
                .as_ref()
                .is_some_and(|run| run.status == PersistedRunStatus::Running)
            {
                existing.as_mut().expect("existing run").status = PersistedRunStatus::Abandoned;
            }
        } else if existing
            .as_ref()
            .is_some_and(|run| run.status == PersistedRunStatus::Running)
        {
            return Err(StoreError::RunCorrupt(
                "running row has no engine lease".to_string(),
            ));
        }

        let expires_at_ms = lease_expiry(now_ms)?;
        transaction
            .execute(
                "INSERT INTO engine_lease(
                    lease_key, owner_id, owner_pid, acquired_at_ms, heartbeat_at_ms, expires_at_ms
                 ) VALUES (1, ?1, ?2, ?3, ?3, ?4)",
                params![
                    owner_id,
                    i64::from(std::process::id()),
                    now_ms,
                    expires_at_ms,
                ],
            )
            .map_err(cache_write)?;

        let scan_run_id = if let Some(existing) = existing {
            if existing.status != PersistedRunStatus::Abandoned {
                return Err(StoreError::RunCorrupt(
                    "nonterminal run cannot be restarted".to_string(),
                ));
            }
            clear_staging_rows(&transaction, existing.scan_run_id)?;
            let updated = transaction
                .execute(
                    "UPDATE scan_runs
                     SET owner_id=?1, status='running', started_at_ms=?2,
                         updated_at_ms=?2, finished_at_ms=NULL, final_envelope_json=NULL
                     WHERE scan_run_id=?3 AND status='abandoned'",
                    params![owner_id, now_ms, existing.scan_run_id],
                )
                .map_err(cache_write)?;
            if updated != 1 {
                return Err(StoreError::RunCorrupt(
                    "abandoned run could not be reactivated".to_string(),
                ));
            }
            existing.scan_run_id
        } else {
            transaction
                .execute(
                    "INSERT INTO scan_runs(
                        request_id, canonical_request_json, request_hash_algorithm,
                        request_hash, owner_id, status, created_at_ms, started_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?6, ?6)",
                    params![
                        request_id,
                        canonical.json,
                        canonical.hash_algorithm,
                        canonical.hash,
                        owner_id,
                        now_ms,
                    ],
                )
                .map_err(cache_write)?;
            transaction.last_insert_rowid()
        };
        let attempt_number: i64 = transaction
            .query_row(
                "SELECT coalesce(max(attempt_number), 0) + 1
                 FROM scan_run_attempts WHERE scan_run_id=?1",
                [scan_run_id],
                |row| row.get(0),
            )
            .map_err(cache_write)?;
        transaction
            .execute(
                "INSERT INTO scan_run_attempts(
                    scan_run_id, attempt_number, owner_id, normalized_scan_db_path,
                    normalized_office_worker_path, normalized_python_executable,
                    normalized_python_module_root, python_document_worker_module,
                    engine_fingerprint, started_at_ms, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'running')",
                params![
                    scan_run_id,
                    attempt_number,
                    owner_id,
                    runtime.normalized_scan_db_path,
                    runtime.normalized_office_worker_path,
                    runtime.normalized_python_executable,
                    runtime.normalized_python_module_root,
                    runtime.python_document_worker_module,
                    runtime.engine_fingerprint,
                    now_ms,
                ],
            )
            .map_err(cache_write)?;
        transaction.commit().map_err(cache_write)?;
        Ok(BeginRunOutcome::Started(ActiveRun {
            scan_run_id,
            attempt_number,
            owner_id,
            request_id: request_id.to_string(),
        }))
    }

    pub fn record_worker_fingerprints(
        &mut self,
        active: &ActiveRun,
        office: Option<&WorkerFingerprint>,
        python: Option<&WorkerFingerprint>,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        if let Some(fingerprint) = office {
            fingerprint.validate()?;
        }
        if let Some(fingerprint) = python {
            fingerprint.validate()?;
        }
        let now_ms = checked_i64(now_ms, "worker fingerprint timestamp")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        ensure_owner(&transaction, active)?;
        let updated = transaction
            .execute(
                "UPDATE scan_run_attempts SET
                    office_worker_contract=?1, office_worker_version=?2, office_worker_build=?3,
                    python_worker_contract=?4, python_worker_version=?5, python_worker_build=?6
                 WHERE scan_run_id=?7 AND attempt_number=?8 AND owner_id=?9 AND status='running'",
                params![
                    office.map(|value| value.contract.as_str()),
                    office.map(|value| value.version.as_str()),
                    office.map(|value| value.build.as_str()),
                    python.map(|value| value.contract.as_str()),
                    python.map(|value| value.version.as_str()),
                    python.map(|value| value.build.as_str()),
                    active.scan_run_id,
                    active.attempt_number,
                    active.owner_id,
                ],
            )
            .map_err(cache_write)?;
        if updated != 1 {
            return Err(StoreError::LeaseLost);
        }
        heartbeat_in_transaction(&transaction, active, now_ms)?;
        transaction.commit().map_err(cache_write)
    }

    pub fn heartbeat(&mut self, active: &ActiveRun, now_ms: u64) -> Result<(), StoreError> {
        let now_ms = checked_i64(now_ms, "heartbeat timestamp")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        heartbeat_in_transaction(&transaction, active, now_ms)?;
        transaction.commit().map_err(cache_write)
    }

    pub fn lookup_cache(
        &self,
        file_identity: &str,
        source_version: &str,
        source_guard_kind: &str,
        source_guard_sha256: &str,
        parse_profile_hash: &str,
        inventory_existed_before: bool,
    ) -> Result<CacheLookup, StoreError> {
        if file_identity.is_empty()
            || source_version.is_empty()
            || inventory::parse_source_version(source_version).is_err()
            || !valid_guard_wire(source_guard_kind, source_guard_sha256)
            || !inventory::is_sha256(parse_profile_hash)
        {
            return Err(StoreError::InvalidRequest(
                "cache lookup key is invalid".to_string(),
            ));
        }
        cache::lookup_cache(
            &self.connection,
            file_identity,
            source_version,
            source_guard_kind,
            source_guard_sha256,
            parse_profile_hash,
            inventory_existed_before,
        )
        .map_err(cache_open)
    }

    /// Upserts the global `file_inventory` in one bounded short transaction and
    /// returns the set of file_identities that already existed before this round
    /// (spec Part 4 miss-reason tree step 4). Only a completed receipt opens
    /// cache lookup; a later terminal `finalize` re-upserts the same rows with
    /// the active scan_run_id (idempotent ON CONFLICT DO UPDATE).
    pub fn prepare_inventory(
        &mut self,
        records: &[InventoryRecord],
        scan_run_id: i64,
        now_ms: u64,
    ) -> Result<HashSet<String>, StoreError> {
        let now_ms = checked_i64(now_ms, "inventory timestamp")?;
        let mut existed = std::collections::HashSet::new();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        for record in records {
            let already: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM file_inventory WHERE file_identity=?1)",
                    [&record.file_identity],
                    |row| row.get(0),
                )
                .map_err(cache_write)?;
            if already {
                existed.insert(record.file_identity.clone());
            }
        }
        inventory::upsert_inventory(&transaction, scan_run_id, now_ms, records)
            .map_err(cache_write)?;
        transaction.commit().map_err(cache_write)?;
        Ok(existed)
    }

    /// Successful parse-cache write in an independent short transaction
    /// (spec Solution persistence boundary 2). Receipt-typed: the COMMIT is the
    /// linearization point.
    pub fn write_success_parse_cache(
        &mut self,
        records: &[CacheWriteRecord],
        cached_at_ms: u64,
    ) -> Result<(), StoreError> {
        let cached_at_ms = checked_i64(cached_at_ms, "cache write timestamp")?;
        for record in records {
            record
                .validate()
                .map_err(StoreError::InvalidRequest)?;
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        cache::write_success_cache(&transaction, cached_at_ms, records).map_err(cache_write)?;
        transaction.commit().map_err(cache_write)
    }

    /// Typed classification-cache lookup (spec Part 3.2). The miss-reason tree
    /// distinguishes `entry_absent_or_evicted` from `new_file` via
    /// `inventory_existed_before` (returned by `prepare_inventory`).
    pub fn lookup_classification_cache(
        &self,
        file_identity: &str,
        source_version: &str,
        source_guard_kind: &str,
        source_guard_sha256: &str,
        classifier_profile_hash: &str,
        classifier_build: &str,
        inventory_existed_before: bool,
    ) -> Result<ClassificationCacheLookup, StoreError> {
        if file_identity.is_empty()
            || source_version.is_empty()
            || inventory::parse_source_version(source_version).is_err()
            || !inventory::is_sha256(classifier_profile_hash)
            || !inventory::is_sha256(classifier_build)
        {
            return Err(StoreError::InvalidRequest(
                "classification cache lookup key is invalid".to_string(),
            ));
        }
        cache::lookup_classification_cache(
            &self.connection,
            file_identity,
            source_version,
            source_guard_kind,
            source_guard_sha256,
            classifier_profile_hash,
            classifier_build,
            inventory_existed_before,
        )
        .map_err(cache_open)
    }

    /// Success-only classification-cache write in an independent short
    /// transaction (spec Part 3.2: no negative cache).
    pub fn write_success_classification_cache(
        &mut self,
        records: &[ClassificationCacheWriteRecord],
        cached_at_ms: u64,
    ) -> Result<(), StoreError> {
        let cached_at_ms = checked_i64(cached_at_ms, "classification cache write timestamp")?;
        for record in records {
            record
                .validate()
                .map_err(StoreError::InvalidRequest)?;
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        cache::write_success_classification_cache(&transaction, cached_at_ms, records)
            .map_err(cache_write)?;
        transaction.commit().map_err(cache_write)
    }

    pub fn attach_cache_evidence(
        &self,
        planned_files: Vec<crate::planner::PlannedFile>,
        profile: &NormalizedScannerProfileV1,
        route_stacks: &RouteStackFingerprints,
    ) -> Result<Vec<CacheAwarePlanEntry>, StoreError> {
        planned_files
            .into_iter()
            .map(|planned| match planned.action {
                crate::planner::PlanAction::Reject(_) => Ok(CacheAwarePlanEntry {
                    planned,
                    parse_profile_hash: None,
                    cache_lookup: None,
                }),
                crate::planner::PlanAction::Parse(route) => {
                    let profile_hash =
                        parse_profile_hash(1, route_stacks.for_route(route), profile)
                            .map_err(StoreError::InvalidRequest)?;
                    let lookup = self.lookup_cache(
                        &planned.file.file_identity,
                        &planned.file.source_version,
                        "content_sha256_v1",
                        &"0".repeat(64),
                        &profile_hash,
                        false,
                    )?;
                    Ok(CacheAwarePlanEntry {
                        planned,
                        parse_profile_hash: Some(profile_hash),
                        cache_lookup: Some(lookup),
                    })
                }
            })
            .collect()
    }

    pub fn finalize(
        &mut self,
        active: &ActiveRun,
        batch: &FinalizationBatch,
        now_ms: u64,
    ) -> Result<TerminalAuditTimings, StoreError> {
        let envelope = validate_finalization(active, batch)?;
        let now_ms = checked_i64(now_ms, "finalization timestamp")?;
        let metadata_json = envelope_metadata_json(&envelope)?;
        schema::require_durable_finalization(&self.connection)
            .map_err(|error| cache_write(error.to_string()))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        let transaction_started = Instant::now();
        heartbeat_in_transaction(&transaction, active, now_ms)?;
        ensure_engine_fingerprint(&transaction, active, &envelope)?;
        let handshake_failed = batch.diagnostics.iter().any(|record| {
            record.severity == DiagnosticSeverity::Error
                && matches!(
                    record.diagnostic.error_code,
                    ErrorCode::WorkerHandshakeFailed | ErrorCode::WorkerVersionMismatch
                )
        });
        let fingerprints_required = matches!(batch.status, RunStatus::Success | RunStatus::Partial)
            || !batch.inventory.is_empty()
            || !batch.file_results.is_empty()
            || !batch.cache_writes.is_empty()
            || !handshake_failed;
        if fingerprints_required {
            ensure_worker_fingerprints(&transaction, active)?;
        }

        let audit_write_started = Instant::now();
        inventory::upsert_inventory(&transaction, active.scan_run_id, now_ms, &batch.inventory)
            .map_err(cache_write)?;
        cache::write_success_cache(&transaction, now_ms, &batch.cache_writes)
            .map_err(cache_write)?;
        let snapshot_rows = batch.snapshot_hit.is_some();
        inventory::insert_file_results(
            &transaction,
            active.scan_run_id,
            &batch.file_results,
            snapshot_rows,
        )
        .map_err(cache_write)?;
        insert_diagnostics(&transaction, active.scan_run_id, &batch.diagnostics)
            .map_err(cache_write)?;
        crate::metrics::insert_metrics(
            &transaction,
            active.scan_run_id,
            &batch.stage_metrics,
            &batch.extension_metrics,
        )
        .map_err(cache_write)?;
        let current_run_audit_write_ms = elapsed_ms(audit_write_started);

        // ---- artifact write / snapshot-hit reference + context_runs ----
        // spec Part 4/5.2: establish the current `context_runs.artifact_id`
        // reference and a temporary protected set BEFORE the retention/orphan
        // sweep, so a just-hit artifact can never be reclaimed in the
        // "old reference deleted, current reference not yet created" window.
        let mut protected_runs: HashSet<i64> = HashSet::new();
        protected_runs.insert(active.scan_run_id);
        let mut protected_artifacts: HashSet<i64> = HashSet::new();
        let mut artifact_for_rebuild: Option<ArtifactDraft> = None;

        if let Some(context) = &batch.context {
            if batch.status == RunStatus::Error {
                // spec Part 2.3: a committed Error run MUST write a context_runs
                // row with artifact_id=NULL (no artifact, no snapshot, no reuse).
                insert_context(
                    &transaction,
                    active.scan_run_id,
                    context,
                    None,
                    false,
                    None,
                    now_ms,
                )
                .map_err(cache_write)?;
            } else {
            let artifact_id = if let Some(hit) = &batch.snapshot_hit {
                let draft = load_artifact_from_connection(&transaction, hit.artifact_id)?;
                protected_artifacts.insert(hit.artifact_id);
                protected_runs.insert(hit.reused_from_context_run_id);
                artifact_for_rebuild = Some(draft);
                hit.artifact_id
            } else {
                let draft = batch.artifact.as_ref().ok_or_else(|| {
                    StoreError::RunCorrupt(
                        "success/partial run must carry an artifact draft".to_string(),
                    )
                })?;
                if draft.snapshot_eligible != batch.snapshot_key.is_some() {
                    return Err(StoreError::RunCorrupt(
                        "artifact eligibility disagrees with the snapshot key".to_string(),
                    ));
                }
                let semantic_json = semantic_summary_json_for(draft)?;
                let artifact_id = if let Some(key) = &batch.snapshot_key {
                    if let Some(existing) = dedup_artifact(&transaction, draft, key)? {
                        existing
                    } else {
                        let size = artifact_size_bytes(
                            &draft.final_context,
                            &draft.context_sha256,
                            &semantic_json,
                            Some(key),
                            &draft.file_rows,
                            &draft.decision_rows,
                        );
                        make_room_for_artifact(&transaction, &protected_artifacts, size)?;
                        insert_artifact(&transaction, draft, Some(key), now_ms)?
                    }
                } else {
                    let size = artifact_size_bytes(
                        &draft.final_context,
                        &draft.context_sha256,
                        &semantic_json,
                        None,
                        &draft.file_rows,
                        &draft.decision_rows,
                    );
                    make_room_for_artifact(&transaction, &protected_artifacts, size)?;
                    insert_artifact(&transaction, draft, None, now_ms)?
                };
                protected_artifacts.insert(artifact_id);
                artifact_for_rebuild = Some(draft.clone());
                artifact_id
            };
            insert_context(
                &transaction,
                active.scan_run_id,
                context,
                Some(artifact_id),
                batch.snapshot_hit.is_some(),
                batch
                    .snapshot_hit
                    .as_ref()
                    .map(|hit| hit.reused_from_context_run_id),
                now_ms,
            )
            .map_err(cache_write)?;
            // spec Part 5.2: the artifact semantic summary must agree with the
            // current summary (both derive from the same semantic decisions).
            let semantic = &artifact_for_rebuild
                .as_ref()
                .expect("artifact set above")
                .semantic_summary;
            let summary = &context.summary;
            if semantic.source_file_count != summary.source_file_count
                || semantic.success_count != summary.success_count
                || semantic.timeout_count != summary.timeout_count
                || semantic.included_file_count != summary.included_file_count
                || semantic.omitted_file_count != summary.omitted_file_count
                || semantic.error_file_count != summary.error_file_count
                || semantic.input_chars != summary.input_chars
                || semantic.output_chars != summary.output_chars
            {
                return Err(StoreError::RunCorrupt(
                    "artifact semantic summary disagrees with the current summary".to_string(),
                ));
            }
            }
        }

        let status = terminal_status_text(batch.status)?;
        let audit_size = compute_audit_size(batch);
        let updated = transaction
            .execute(
                "UPDATE scan_runs
                 SET status=?1, updated_at_ms=?2, finished_at_ms=?2, final_envelope_json=?3,
                     final_envelope_metadata_json=?4, audit_size_bytes=?5
                 WHERE scan_run_id=?6 AND owner_id=?7 AND status='running'",
                params![
                    status,
                    now_ms,
                    // spec Part 5.1: the body is stored ONCE in the artifact; the
                    // persisted scan_runs JSON is the body-free metadata, and
                    // idempotent replay rebuilds the full ContextEnvelope from
                    // metadata + summary + artifact.
                    metadata_json,
                    metadata_json,
                    audit_size,
                    active.scan_run_id,
                    active.owner_id,
                ],
            )
            .map_err(cache_write)?;
        if updated != 1 {
            return Err(StoreError::LeaseLost);
        }
        let attempt_updated = transaction
            .execute(
                "UPDATE scan_run_attempts SET status=?1, finished_at_ms=?2
                 WHERE scan_run_id=?3 AND attempt_number=?4 AND owner_id=?5 AND status='running'",
                params![
                    status,
                    now_ms,
                    active.scan_run_id,
                    active.attempt_number,
                    active.owner_id,
                ],
            )
            .map_err(cache_write)?;
        if attempt_updated != 1 {
            return Err(StoreError::LeaseLost);
        }
        // spec Part 4/5.2 terminal run GC: protected set = current run +
        // snapshot-hit source run; the just-hit artifact is protected by the
        // current context_runs reference.
        retention_gc_for_current_run(&transaction, &protected_runs, now_ms, audit_size)
            .map_err(cache_write)?;
        // spec Part 5.3: the authoritative `execution_metrics` row is bound at
        // the precommit checkpoint. `terminal_precommit_ms` runs from transaction
        // begin to just before the metrics write; `envelope_rebuild_ms` covers the
        // metadata+summary+artifact rebuild and validation.
        let terminal_precommit_ms = elapsed_ms(transaction_started);
        let terminal_rows_written = compute_terminal_rows_written(batch);
        let rebuild_started = Instant::now();
        let metadata_value: serde_json::Value = serde_json::from_str(&metadata_json)
            .map_err(|error| StoreError::RunCorrupt(error.to_string()))?;
        let rebuilt = crate::artifact::rebuild_envelope(
            &metadata_value,
            &envelope.summary,
            artifact_for_rebuild.as_ref(),
        )
        .map_err(|message| {
            StoreError::RunCorrupt(format!("rebuilt envelope is invalid: {message}"))
        })?;
        if canonical_envelope_json(&rebuilt)? != batch.envelope_json {
            return Err(StoreError::RunCorrupt(
                "rebuilt envelope disagrees with the committed envelope".to_string(),
            ));
        }
        let envelope_rebuild_ms = elapsed_ms(rebuild_started);
        if let Some(metrics) = &batch.execution_metrics {
            let mut metrics = metrics.clone();
            metrics.current_run_audit_write_ms = current_run_audit_write_ms;
            metrics.terminal_precommit_ms = terminal_precommit_ms;
            metrics.terminal_rows_written = terminal_rows_written;
            metrics.envelope_rebuild_ms = envelope_rebuild_ms;
            metrics.validate().map_err(|message| {
                StoreError::RunCorrupt(format!("execution metrics are invalid: {message}"))
            })?;
            insert_execution_metrics(&transaction, active.scan_run_id, &metrics)?;
        }
        let lease_deleted = transaction
            .execute(
                "DELETE FROM engine_lease WHERE lease_key=1 AND owner_id=?1",
                [&active.owner_id],
            )
            .map_err(cache_write)?;
        if lease_deleted != 1 {
            return Err(StoreError::LeaseLost);
        }
        transaction.commit().map_err(cache_write)?;

        // The exact bytes committed above must still parse as the validated object.
        debug_assert_eq!(envelope.request_id, active.request_id);
        Ok(TerminalAuditTimings {
            current_run_audit_write_ms,
            terminal_precommit_ms,
            envelope_rebuild_ms,
            terminal_rows_written,
        })
    }

    pub fn load_terminal_envelope(&self, scan_run_id: u64) -> Result<StoredEnvelope, StoreError> {
        let scan_run_id = checked_i64(scan_run_id, "scan run id")?;
        let row: (String, String, String, String, Option<String>) = self
            .connection
            .query_row(
                "SELECT request_id, canonical_request_json, request_hash, status,
                        final_envelope_metadata_json
                 FROM scan_runs WHERE scan_run_id=?1",
                [scan_run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()
            .map_err(cache_open)?
            .ok_or(StoreError::RunNotFound)?;
        let existing = ExistingRun {
            scan_run_id,
            canonical_request_json: row.1,
            request_hash: row.2,
            status: PersistedRunStatus::parse(&row.3)?,
            final_envelope_metadata_json: row.4,
        };
        load_stored_envelope_ref(&self.connection, &existing, &row.0)
    }

    pub fn load_diagnostics(
        &self,
        scan_run_id: u64,
    ) -> Result<Vec<RunDiagnosticRecord>, StoreError> {
        let scan_run_id = checked_i64(scan_run_id, "scan run id")?;
        let exists: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scan_runs WHERE scan_run_id=?1)",
                [scan_run_id],
                |row| row.get(0),
            )
            .map_err(cache_open)?;
        if !exists {
            return Err(StoreError::RunNotFound);
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT severity, error_code, message, retryable, stage, file_path, backend
                 FROM run_diagnostics WHERE scan_run_id=?1 ORDER BY sequence",
            )
            .map_err(cache_open)?;
        let rows: Vec<RawDiagnosticRow> = statement
            .query_map([scan_run_id], |row| {
                Ok(RawDiagnosticRow {
                    severity: row.get(0)?,
                    error_code: row.get(1)?,
                    message: row.get(2)?,
                    retryable: row.get::<_, i64>(3)? != 0,
                    stage: row.get(4)?,
                    file_path: row.get(5)?,
                    backend: row.get(6)?,
                })
            })
            .map_err(cache_open)?
            .collect::<Result<_, _>>()
            .map_err(cache_open)?;
        rows.into_iter()
            .map(|row| {
                let severity = match row.severity.as_str() {
                    "warning" => DiagnosticSeverity::Warning,
                    "error" => DiagnosticSeverity::Error,
                    _ => {
                        return Err(StoreError::RunCorrupt(
                            "diagnostic severity is invalid".to_string(),
                        ));
                    }
                };
                let diagnostic = Diagnostic {
                    error_code: parse_contract_enum(&row.error_code)?,
                    message: row.message,
                    retryable: row.retryable,
                    stage: parse_contract_enum(&row.stage)?,
                    file_path: Nullable(row.file_path),
                    backend: Nullable(row.backend),
                };
                diagnostic.validate().map_err(|_| {
                    StoreError::RunCorrupt("persisted diagnostic is invalid".to_string())
                })?;
                Ok(RunDiagnosticRecord {
                    severity,
                    diagnostic,
                })
            })
            .collect()
    }

    pub fn inspect_run(
        &mut self,
        scan_run_id: u64,
        include_content: bool,
    ) -> Result<crate::context_audit::InspectSnapshot, crate::context_audit::InspectLoadError> {
        let scan_run_id =
            i64::try_from(scan_run_id).map_err(|_| crate::context_audit::InspectLoadError {
                error: crate::context_audit::InspectAuditError::RunNotFound,
                run_status: None,
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| crate::context_audit::InspectLoadError {
                error: crate::context_audit::InspectAuditError::Sql(error),
                run_status: None,
            })?;
        let snapshot = crate::context_audit::load_inspect_snapshot(
            &transaction,
            scan_run_id,
            include_content,
        )?;
        transaction
            .commit()
            .map_err(|error| crate::context_audit::InspectLoadError {
                error: crate::context_audit::InspectAuditError::Sql(error),
                run_status: Some(snapshot.run_status),
            })?;
        Ok(snapshot)
    }

    /// Spec Part 5.2/5.4 snapshot lookup. The SQL must select an eligible
    /// artifact AND at least one committed Success `context_runs` row
    /// referencing it; an orphan artifact (no source run) is NOT a hit. The
    /// `snapshot_key_json` is compared byte-for-byte (never trusts the hash
    /// alone). The source run is chosen by `(finished_at_ms DESC,
    /// context_run_id DESC)` from BEFORE the current transaction.
    pub fn snapshot_lookup(
        &self,
        key: &SnapshotKeyParts,
    ) -> Result<Option<SnapshotHit>, StoreError> {
        let row: Option<(i64, i64)> = self
            .connection
            .query_row(
                "SELECT a.artifact_id, r.context_run_id
                 FROM context_artifacts a
                 JOIN context_runs r ON r.artifact_id = a.artifact_id
                 JOIN scan_runs s ON s.scan_run_id = r.scan_run_id
                 WHERE a.snapshot_eligible = 1
                   AND a.snapshot_key_sha256 = ?1
                   AND a.snapshot_key_json = ?2
                   AND r.status = 'success'
                   AND s.status = 'success'
                 ORDER BY s.finished_at_ms DESC, r.context_run_id DESC
                 LIMIT 1",
                params![key.sha256, key.canonical_json],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(cache_open)?;
        Ok(row.map(|(artifact_id, source_context_run_id)| SnapshotHit {
            artifact_id,
            source_context_run_id,
        }))
    }

    /// Loads an artifact (parent + owned file/decision rows) for snapshot
    /// current-row rebuild and dedup comparison (spec Part 5.1 replay check).
    pub fn load_artifact(&self, artifact_id: i64) -> Result<ArtifactDraft, StoreError> {
        load_artifact_from_connection(&self.connection, artifact_id)
    }
}

pub fn canonical_envelope_json(envelope: &ContextEnvelope) -> Result<String, StoreError> {
    envelope.validate().map_err(StoreError::InvalidRequest)?;
    serde_json::to_string(envelope).map_err(|error| StoreError::InvalidRequest(error.to_string()))
}

fn validate_database_path(path: &Path) -> Result<(), StoreError> {
    if !path.is_absolute()
        || path.file_name().and_then(|value| value.to_str()) != Some(SCAN_DB_FILENAME)
        || !path.parent().is_some_and(Path::is_dir)
    {
        return Err(StoreError::InvalidRequest(format!(
            "scan_db_path must be an absolute path ending in {SCAN_DB_FILENAME} with an existing parent"
        )));
    }
    Ok(())
}

/// Business opens fail closed on a committed v1 database (`SCHEMA_UPGRADE_REQUIRED`,
/// non-retryable) and on a database newer than this engine (`TooNew`). Only the
/// separately-authorized `upgrade-db apply=true` migrates a v1 database.
fn ensure_schema_openable(connection: &Connection) -> Result<(), StoreError> {
    let version: i32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(cache_open)?;
    if version == 1 {
        return Err(StoreError::SchemaUpgradeRequired);
    }
    if version > LATEST_USER_VERSION {
        return Err(StoreError::SchemaTooNew);
    }
    Ok(())
}

/// Read-only audit path (`apply=false`): a read-only connection cannot switch
/// journal_mode to WAL, so this configures only the pragmas that are safe on a
/// read-only connection. `PRAGMA auto_vacuum=INCREMENTAL` on an existing database
/// is a no-op and does not touch the file.
fn configure_readonly_connection(connection: &Connection) -> Result<(), StoreError> {
    connection
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(cache_open)?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(cache_open)?;
    connection
        .execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")
        .map_err(cache_open)
}

/// Maintenance connection configuration. Unlike `schema::configure_connection`
/// this deliberately does NOT switch `journal_mode` to WAL: maintenance is an
/// exclusive single-connection operation, and a dry-run must not mutate the DB
/// header or create `-wal`/`-shm` sidecars (spec Part 5.3: dry-run 零写).
fn configure_maintenance_connection(connection: &Connection) -> Result<(), schema::SchemaError> {
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

const V1_SCHEMA_TABLES: &[&str] = &[
    "scan_runs",
    "engine_lease",
    "scan_run_attempts",
    "run_diagnostics",
    "file_inventory",
    "parse_cache",
    "scan_file_results",
    "scan_stage_metrics",
    "scan_extension_metrics",
    "context_runs",
    "context_decisions",
];

fn verify_v1_schema(connection: &Connection) -> Result<(), String> {
    for table in V1_SCHEMA_TABLES {
        let present: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1
                 )",
                [table],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !present {
            return Err(format!("v1 table {table} is missing"));
        }
    }
    Ok(())
}

fn read_user_version(connection: &Connection) -> Result<i32, StoreError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(cache_open)
}

fn run_integrity(connection: &Connection) -> UpgradeIntegrityCheck {
    match connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
        Ok(value) if value == "ok" => UpgradeIntegrityCheck::Ok,
        _ => UpgradeIntegrityCheck::Failed,
    }
}

fn count_parse_cache(connection: &Connection) -> Result<u64, StoreError> {
    connection
        .query_row("SELECT count(*) FROM parse_cache", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(cache_open)
        .map(|count| count as u64)
}

/// Configures the connection and re-verifies that it is a committed v1 database.
/// It deliberately never calls `migrate`: normal opens fail closed on v1 and this
/// gate exists only for the separately-authorized upgrade entry.
fn open_for_upgrade(connection: &mut Connection) -> Result<(), StoreError> {
    schema::configure_connection(connection).map_err(|error| cache_open(error.to_string()))?;
    let version = read_user_version(connection)?;
    if version != 1 {
        return Err(StoreError::SchemaUpgradeRequired);
    }
    verify_v1_schema(connection).map_err(|_| StoreError::SchemaUpgradeRequired)
}

fn upgrade_diagnostic(error_code: ErrorCode, message: String, retryable: bool) -> Diagnostic {
    Diagnostic {
        error_code,
        message: message.chars().take(4_096).collect(),
        retryable,
        stage: DiagnosticStage::Maintenance,
        file_path: Nullable(None),
        backend: Nullable(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn upgrade_ok_response(
    request: &UpgradeDatabaseRequestV1,
    source_user_version: u64,
    pre: UpgradeIntegrityCheck,
    post: UpgradeIntegrityCheck,
    schema_migrated: bool,
    auto_vacuum_converted: bool,
    detected: u64,
    invalidated: u64,
) -> UpgradeDatabaseResponseV1 {
    UpgradeDatabaseResponseV1 {
        contract: "ai_daily_scanner_upgrade".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        status: UpgradeStatus::Ok,
        source_user_version: Nullable(Some(source_user_version)),
        target_user_version: 2,
        apply: request.apply,
        schema_migrated,
        auto_vacuum_converted,
        legacy_parse_cache_rows_detected: detected,
        invalidated_parse_cache_rows: invalidated,
        pre_integrity_check: pre,
        post_integrity_check: post,
        warnings: Vec::new(),
        error: Nullable(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn upgrade_partial_response(
    request: &UpgradeDatabaseRequestV1,
    source_user_version: u64,
    pre: UpgradeIntegrityCheck,
    post: UpgradeIntegrityCheck,
    schema_migrated: bool,
    auto_vacuum_converted: bool,
    detected: u64,
    invalidated: u64,
    warnings: Vec<Diagnostic>,
) -> UpgradeDatabaseResponseV1 {
    UpgradeDatabaseResponseV1 {
        contract: "ai_daily_scanner_upgrade".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        status: UpgradeStatus::Partial,
        source_user_version: Nullable(Some(source_user_version)),
        target_user_version: 2,
        apply: request.apply,
        schema_migrated,
        auto_vacuum_converted,
        legacy_parse_cache_rows_detected: detected,
        invalidated_parse_cache_rows: invalidated,
        pre_integrity_check: pre,
        post_integrity_check: post,
        warnings,
        error: Nullable(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn upgrade_error_response(
    request: &UpgradeDatabaseRequestV1,
    source_user_version: Option<u64>,
    pre: UpgradeIntegrityCheck,
    post: UpgradeIntegrityCheck,
    schema_migrated: bool,
    detected: u64,
    error: Diagnostic,
) -> UpgradeDatabaseResponseV1 {
    UpgradeDatabaseResponseV1 {
        contract: "ai_daily_scanner_upgrade".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        status: UpgradeStatus::Error,
        source_user_version: Nullable(source_user_version),
        target_user_version: 2,
        apply: request.apply,
        schema_migrated,
        auto_vacuum_converted: false,
        legacy_parse_cache_rows_detected: detected,
        invalidated_parse_cache_rows: if schema_migrated { detected } else { 0 },
        pre_integrity_check: pre,
        post_integrity_check: post,
        warnings: Vec::new(),
        error: Nullable(Some(error)),
    }
}

fn sqlite_sidecar_path(path: &Path, kind: &str) -> PathBuf {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    path.with_file_name(format!("{name}-{kind}"))
}

fn upgrade_database_audit(request: &UpgradeDatabaseRequestV1) -> UpgradeDatabaseResponseV1 {
    let path = Path::new(&request.scan_db_path);
    let shm_path = sqlite_sidecar_path(path, "shm");
    let wal_path = sqlite_sidecar_path(path, "wal");
    let shm_existed = shm_path.is_file();
    let wal_existed = wal_path.is_file();

    let response = upgrade_audit_inner(request);

    // Opening a WAL-mode database with a read-only connection can make SQLite
    // reconstruct the `-shm`/`-wal` index files even though nothing is written.
    // Restore the pre-audit directory state so the audit stays zero-sidecar
    // against real (WAL-mode) v1 databases. The `-shm` is only an index and is
    // always reconstructible; a `-wal` is removed only when empty so a
    // concurrent writer's frames are never destroyed. The audit is intended to
    // run on a quiescent database (no active scanner).
    if !shm_existed {
        let _ = std::fs::remove_file(&shm_path);
    }
    if !wal_existed && wal_path.is_file() {
        let empty = std::fs::metadata(&wal_path)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(false);
        if empty {
            let _ = std::fs::remove_file(&wal_path);
        }
    }
    response
}

fn upgrade_audit_inner(request: &UpgradeDatabaseRequestV1) -> UpgradeDatabaseResponseV1 {
    let path = Path::new(&request.scan_db_path);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = match Connection::open_with_flags(path, flags) {
        Ok(connection) => connection,
        Err(error) => {
            return upgrade_error_response(
                request,
                None,
                UpgradeIntegrityCheck::NotRun,
                UpgradeIntegrityCheck::NotRun,
                false,
                0,
                upgrade_diagnostic(
                    ErrorCode::CacheOpenFailed,
                    format!("scan database could not be opened read-only: {error}"),
                    true,
                ),
            );
        }
    };
    if let Err(error) = configure_readonly_connection(&connection) {
        return upgrade_error_response(
            request,
            None,
            UpgradeIntegrityCheck::NotRun,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::CacheOpenFailed,
                format!("read-only connection could not be configured: {error}"),
                true,
            ),
        );
    }
    let version = match read_user_version(&connection) {
        Ok(version) => version,
        Err(error) => {
            return upgrade_error_response(
                request,
                None,
                UpgradeIntegrityCheck::NotRun,
                UpgradeIntegrityCheck::NotRun,
                false,
                0,
                upgrade_diagnostic(
                    ErrorCode::CacheOpenFailed,
                    format!("database user_version could not be read: {error}"),
                    true,
                ),
            );
        }
    };
    let pre = run_integrity(&connection);
    if version > LATEST_USER_VERSION {
        return upgrade_error_response(
            request,
            Some(version as u64),
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::SchemaUpgradeRequired,
                format!(
                    "scanner database user_version={version} is newer than this engine ({LATEST_USER_VERSION}); this release cannot open it"
                ),
                false,
            ),
        );
    }
    if version == 2 {
        return upgrade_ok_response(
            request,
            2,
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            false,
            0,
            0,
        );
    }
    if version != 1 {
        return upgrade_error_response(
            request,
            None,
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::SchemaUpgradeRequired,
                format!(
                    "scanner database user_version={version} is not a committed v1 database; upgrade-db audits v1 databases only"
                ),
                false,
            ),
        );
    }
    if let Err(message) = verify_v1_schema(&connection) {
        return upgrade_error_response(
            request,
            Some(1),
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::SchemaMigrationFailed,
                format!("v1 schema verification failed: {message}"),
                false,
            ),
        );
    }
    let now_ms = match current_time_millis() {
        Ok(now_ms) => now_ms as i64,
        Err(_) => {
            return upgrade_error_response(
                request,
                Some(1),
                pre,
                UpgradeIntegrityCheck::NotRun,
                false,
                0,
                upgrade_diagnostic(ErrorCode::InternalError, "system clock is invalid".to_string(), false),
            );
        }
    };
    let live_lease = match query_lease(&connection) {
        Ok(Some(lease)) if lease_is_live(&lease, now_ms) => true,
        Ok(_) => false,
        Err(_) => false,
    };
    if live_lease {
        return upgrade_error_response(
            request,
            Some(1),
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::ScanAlreadyRunning,
                "another scanner run owns the database lease; upgrade is blocked".to_string(),
                true,
            ),
        );
    }
    let detected = match count_parse_cache(&connection) {
        Ok(detected) => detected,
        Err(error) => {
            // The detected count is a hard precondition for the audit response;
            // a silent 0 could under-report `detected` below a later `invalidated`.
            return upgrade_error_response(
                request,
                Some(1),
                pre,
                UpgradeIntegrityCheck::NotRun,
                false,
                0,
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    upgrade_ok_response(
        request,
        1,
        pre,
        UpgradeIntegrityCheck::NotRun,
        false,
        false,
        detected,
        0,
    )
}

fn upgrade_database_apply(request: &UpgradeDatabaseRequestV1) -> UpgradeDatabaseResponseV1 {
    let path = Path::new(&request.scan_db_path);
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut connection = match Connection::open_with_flags(path, flags) {
        Ok(connection) => connection,
        Err(error) => {
            return upgrade_error_response(
                request,
                None,
                UpgradeIntegrityCheck::NotRun,
                UpgradeIntegrityCheck::NotRun,
                false,
                0,
                upgrade_diagnostic(
                    ErrorCode::CacheOpenFailed,
                    format!("scan database could not be opened read-write: {error}"),
                    true,
                ),
            );
        }
    };
    if let Err(error) = schema::configure_connection(&connection) {
        return upgrade_error_response(
            request,
            None,
            UpgradeIntegrityCheck::NotRun,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::CacheOpenFailed,
                format!("database connection could not be configured: {error}"),
                true,
            ),
        );
    }
    let version = match read_user_version(&connection) {
        Ok(version) => version,
        Err(error) => {
            return upgrade_error_response(
                request,
                None,
                UpgradeIntegrityCheck::NotRun,
                UpgradeIntegrityCheck::NotRun,
                false,
                0,
                upgrade_diagnostic(
                    ErrorCode::CacheOpenFailed,
                    format!("database user_version could not be read: {error}"),
                    true,
                ),
            );
        }
    };
    let pre = run_integrity(&connection);
    if version > LATEST_USER_VERSION {
        return upgrade_error_response(
            request,
            Some(version as u64),
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::SchemaUpgradeRequired,
                format!(
                    "scanner database user_version={version} is newer than this engine ({LATEST_USER_VERSION}); fail closed"
                ),
                false,
            ),
        );
    }
    if version == 2 {
        return upgrade_ok_response(
            request,
            2,
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            false,
            0,
            0,
        );
    }
    if version != 1 {
        return upgrade_error_response(
            request,
            None,
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::SchemaUpgradeRequired,
                format!(
                    "scanner database user_version={version} is not a committed v1 database; upgrade-db upgrades v1 databases only"
                ),
                false,
            ),
        );
    }
    if let Err(message) = verify_v1_schema(&connection) {
        return upgrade_error_response(
            request,
            Some(1),
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            0,
            upgrade_diagnostic(
                ErrorCode::SchemaMigrationFailed,
                format!("v1 schema verification failed: {message}"),
                false,
            ),
        );
    }
    let detected = match count_parse_cache(&connection) {
        Ok(detected) => detected,
        Err(error) => {
            // Propagate instead of defaulting to 0: if the count silently read 0
            // and the migration then invalidates N>0 rows, the response would
            // violate `invalidated <= detected`.
            return upgrade_error_response(
                request,
                Some(1),
                pre,
                UpgradeIntegrityCheck::NotRun,
                false,
                0,
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    if pre == UpgradeIntegrityCheck::Failed {
        return upgrade_error_response(
            request,
            Some(1),
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            detected,
            upgrade_diagnostic(
                ErrorCode::SchemaMigrationFailed,
                "pre-integrity check failed before the v1 migration".to_string(),
                false,
            ),
        );
    }
    let now_ms = match current_time_millis() {
        Ok(now_ms) => now_ms as i64,
        Err(error) => {
            return upgrade_error_response(
                request,
                Some(1),
                pre,
                UpgradeIntegrityCheck::NotRun,
                false,
                detected,
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    if let Err(error) = acquire_upgrade_lease(&mut connection, now_ms) {
        return upgrade_error_response(
            request,
            Some(1),
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            detected,
            error.diagnostic(DiagnosticStage::Maintenance),
        );
    }
    if let Err(error) = open_for_upgrade(&mut connection) {
        let _ = release_upgrade_lease(&connection);
        return upgrade_error_response(
            request,
            Some(1),
            pre,
            UpgradeIntegrityCheck::NotRun,
            false,
            detected,
            error.diagnostic(DiagnosticStage::Maintenance),
        );
    }
    let invalidated = match schema::upgrade_v1_to_v2(&mut connection, &request.request_id) {
        Ok(invalidated) => invalidated,
        Err(error) => {
            let _ = release_upgrade_lease(&connection);
            return upgrade_error_response(
                request,
                Some(1),
                pre,
                UpgradeIntegrityCheck::NotRun,
                false,
                detected,
                upgrade_diagnostic(
                    ErrorCode::SchemaMigrationFailed,
                    format!("v1 to v2 migration failed and was rolled back: {error}"),
                    false,
                ),
            );
        }
    };
    let post = run_integrity(&connection);
    if post == UpgradeIntegrityCheck::Failed {
        let _ = release_upgrade_lease(&connection);
        return upgrade_error_response(
            request,
            Some(1),
            pre,
            post,
            true,
            detected,
            upgrade_diagnostic(
                ErrorCode::SchemaMigrationFailed,
                "post-migration integrity check failed".to_string(),
                false,
            ),
        );
    }
    // Independent physical conversion; never part of the migration transaction.
    let auto_vacuum_converted = convert_auto_vacuum(&connection).unwrap_or(false);
    let _ = release_upgrade_lease(&connection);
    if auto_vacuum_converted {
        upgrade_ok_response(
            request,
            1,
            pre,
            post,
            true,
            true,
            detected,
            invalidated,
        )
    } else {
        upgrade_partial_response(
            request,
            1,
            pre,
            post,
            true,
            false,
            detected,
            invalidated,
            vec![upgrade_diagnostic(
                ErrorCode::MaintenanceModeUnavailable,
                "auto_vacuum=INCREMENTAL conversion failed; the v2 schema is committed but incremental maintenance is unavailable".to_string(),
                false,
            )],
        )
    }
}

// ---------------------------------------------------------------------------
// maintenance command（spec Part 4/5.3）：独占 lease → before sizes → pre
// integrity → mode preflight →（dry-run 结束）→ 深度 row GC transaction →
// 所选 vacuum → post integrity → after sizes。mode 只 `gc|incremental_vacuum`，
// 无 full_vacuum；v1 DB → SCHEMA_UPGRADE_REQUIRED；auto_vacuum=none +
// incremental_vacuum → MAINTENANCE_MODE_UNAVAILABLE。失败路径 deleted/before/
// after 报告真实部分进展，绝不伪造 ok。
// ---------------------------------------------------------------------------

fn maintenance_command(request: &MaintenanceRequestV1) -> MaintenanceResponseV1 {
    if let Err(message) = request.validate() {
        return maintenance_error_response(
            request,
            None,
            None,
            false,
            zero_maintenance_deleted(),
            MaintenancePreIntegrityCheck::Failed,
            MaintenancePostIntegrityCheck::NotRun,
            None,
            upgrade_diagnostic(ErrorCode::InvalidRequest, message, false),
        );
    }
    let path = Path::new(&request.scan_db_path);
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut connection = match Connection::open_with_flags(path, flags) {
        Ok(connection) => connection,
        Err(error) => {
            return maintenance_error_response(
                request,
                None,
                None,
                false,
                zero_maintenance_deleted(),
                MaintenancePreIntegrityCheck::Failed,
                MaintenancePostIntegrityCheck::NotRun,
                None,
                upgrade_diagnostic(
                    ErrorCode::CacheOpenFailed,
                    format!("scan database could not be opened read-write: {error}"),
                    true,
                ),
            );
        }
    };
    if let Err(error) = configure_maintenance_connection(&connection) {
        return maintenance_error_response(
            request,
            None,
            None,
            false,
            zero_maintenance_deleted(),
            MaintenancePreIntegrityCheck::Failed,
            MaintenancePostIntegrityCheck::NotRun,
            None,
            upgrade_diagnostic(
                ErrorCode::CacheOpenFailed,
                format!("database connection could not be configured: {error}"),
                true,
            ),
        );
    }
    let version = match read_user_version(&connection) {
        Ok(version) => version,
        Err(error) => {
            return maintenance_error_response(
                request,
                None,
                None,
                false,
                zero_maintenance_deleted(),
                MaintenancePreIntegrityCheck::Failed,
                MaintenancePostIntegrityCheck::NotRun,
                None,
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    if version > LATEST_USER_VERSION {
        return maintenance_error_response(
            request,
            None,
            None,
            false,
            zero_maintenance_deleted(),
            MaintenancePreIntegrityCheck::Failed,
            MaintenancePostIntegrityCheck::NotRun,
            None,
            upgrade_diagnostic(
                ErrorCode::SchemaUpgradeRequired,
                format!(
                    "scanner database user_version={version} is newer than this engine ({LATEST_USER_VERSION}); fail closed"
                ),
                false,
            ),
        );
    }
    if version != LATEST_USER_VERSION {
        // v1 (or an empty user_version 0) DBs are upgraded only by upgrade-db.
        return maintenance_error_response(
            request,
            None,
            None,
            false,
            zero_maintenance_deleted(),
            MaintenancePreIntegrityCheck::Failed,
            MaintenancePostIntegrityCheck::NotRun,
            None,
            upgrade_diagnostic(
                ErrorCode::SchemaUpgradeRequired,
                "maintenance requires a v2 scanner database; run upgrade-db apply=true first".to_string(),
                false,
            ),
        );
    }
    // Safe v2 amendment: rebuild parse_cache with the retention columns if a
    // pre-amendment v2 database is encountered.
    if let Err(error) = schema::migrate(&mut connection) {
        return maintenance_error_response(
            request,
            None,
            None,
            false,
            zero_maintenance_deleted(),
            MaintenancePreIntegrityCheck::Failed,
            MaintenancePostIntegrityCheck::NotRun,
            None,
            upgrade_diagnostic(
                ErrorCode::CacheOpenFailed,
                format!("v2 schema amendment failed: {error}"),
                false,
            ),
        );
    }
    let now_ms = match current_time_millis() {
        Ok(value) => value as i64,
        Err(error) => {
            return maintenance_error_response(
                request,
                None,
                None,
                false,
                zero_maintenance_deleted(),
                MaintenancePreIntegrityCheck::Failed,
                MaintenancePostIntegrityCheck::NotRun,
                None,
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    if let Err(error) = acquire_upgrade_lease(&mut connection, now_ms) {
        return maintenance_error_response(
            request,
            None,
            None,
            false,
            zero_maintenance_deleted(),
            MaintenancePreIntegrityCheck::Failed,
            MaintenancePostIntegrityCheck::NotRun,
            None,
            error.diagnostic(DiagnosticStage::Maintenance),
        );
    }
    let before = match maintenance_sizes(&connection, path) {
        Ok(sizes) => sizes,
        Err(error) => {
            let _ = release_upgrade_lease(&connection);
            return maintenance_error_response(
                request,
                None,
                None,
                false,
                zero_maintenance_deleted(),
                MaintenancePreIntegrityCheck::Failed,
                MaintenancePostIntegrityCheck::NotRun,
                None,
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    let pre = maintenance_integrity(&connection);
    if pre == MaintenancePreIntegrityCheck::Failed {
        let _ = release_upgrade_lease(&connection);
        return maintenance_error_response(
            request,
            Some(before.clone()),
            Some(before.clone()),
            true,
            zero_maintenance_deleted(),
            pre,
            MaintenancePostIntegrityCheck::NotRun,
            Some(MaintenanceVacuumV1 {
                mode: request.mode,
                status: MaintenanceVacuumStatus::NotRequested,
                pages_changed: 0,
            }),
            upgrade_diagnostic(
                ErrorCode::RunCorrupt,
                "pre-integrity check failed before any maintenance mutation".to_string(),
                false,
            ),
        );
    }
    let mut vacuum = MaintenanceVacuumV1 {
        mode: request.mode,
        status: MaintenanceVacuumStatus::NotRequested,
        pages_changed: 0,
    };
    let auto_vacuum = match auto_vacuum_mode(&connection) {
        Ok(mode) => mode,
        Err(error) => {
            let _ = release_upgrade_lease(&connection);
            return maintenance_error_response(
                request,
                Some(before.clone()),
                Some(before.clone()),
                true,
                zero_maintenance_deleted(),
                pre,
                MaintenancePostIntegrityCheck::NotRun,
                Some(vacuum),
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    if request.mode == MaintenanceMode::IncrementalVacuum && auto_vacuum == AutoVacuumMode::None {
        let _ = release_upgrade_lease(&connection);
        vacuum.status = MaintenanceVacuumStatus::Error;
        return maintenance_error_response(
            request,
            Some(before.clone()),
            Some(before.clone()),
            true,
            zero_maintenance_deleted(),
            pre,
            MaintenancePostIntegrityCheck::NotRun,
            Some(vacuum),
            upgrade_diagnostic(
                ErrorCode::MaintenanceModeUnavailable,
                "incremental_vacuum requires auto_vacuum=INCREMENTAL; this database is auto_vacuum=none (upgrade-path conversion is a known limitation)".to_string(),
                false,
            ),
        );
    }
    if request.dry_run {
        let _ = release_upgrade_lease(&connection);
        vacuum.status = MaintenanceVacuumStatus::SkippedDryRun;
        return maintenance_ok_response(
            request,
            before.clone(),
            before.clone(),
            true,
            zero_maintenance_deleted(),
            pre,
            MaintenancePostIntegrityCheck::NotRun,
            vacuum,
        );
    }
    let deleted = match run_maintenance_gc(&mut connection, now_ms) {
        Ok(deleted) => deleted,
        Err(error) => {
            // GC transaction failed: no vacuum; still attempt read-only post
            // integrity + after sizing so partial progress is honest.
            let post = maintenance_post_integrity(&connection);
            let after = maintenance_sizes(&connection, path).ok();
            let _ = release_upgrade_lease(&connection);
            vacuum.status = MaintenanceVacuumStatus::Error;
            return maintenance_error_response(
                request,
                Some(before.clone()),
                after.or(Some(before.clone())),
                true,
                zero_maintenance_deleted(),
                pre,
                post,
                Some(vacuum),
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    if request.mode == MaintenanceMode::IncrementalVacuum {
        match run_incremental_vacuum(&connection) {
            Ok(pages_changed) => {
                vacuum.status = MaintenanceVacuumStatus::Ok;
                vacuum.pages_changed = pages_changed;
            }
            Err(error) => {
                // Vacuum failed after the committed GC: GC deletions are NOT
                // rolled back; deleted/before/after report real partial progress.
                let post = maintenance_post_integrity(&connection);
                let after = maintenance_sizes(&connection, path).ok();
                let _ = release_upgrade_lease(&connection);
                vacuum.status = MaintenanceVacuumStatus::Error;
                return maintenance_error_response(
                    request,
                    Some(before.clone()),
                    after.or(Some(before.clone())),
                    true,
                    deleted,
                    pre,
                    post,
                    Some(vacuum),
                    error.diagnostic(DiagnosticStage::Maintenance),
                );
            }
        }
    }
    let post = maintenance_post_integrity(&connection);
    let after = match maintenance_sizes(&connection, path) {
        Ok(sizes) => sizes,
        Err(error) => {
            let _ = release_upgrade_lease(&connection);
            return maintenance_error_response(
                request,
                Some(before.clone()),
                Some(before.clone()),
                false,
                deleted,
                pre,
                post,
                Some(vacuum),
                error.diagnostic(DiagnosticStage::Maintenance),
            );
        }
    };
    if post == MaintenancePostIntegrityCheck::Failed {
        let _ = release_upgrade_lease(&connection);
        vacuum.status = MaintenanceVacuumStatus::Error;
        return maintenance_error_response(
            request,
            Some(before.clone()),
            Some(after.clone()),
            true,
            deleted,
            pre,
            post,
            Some(vacuum),
            upgrade_diagnostic(
                ErrorCode::RunCorrupt,
                "post-integrity check failed after maintenance mutations".to_string(),
                false,
            ),
        );
    }
    let _ = release_upgrade_lease(&connection);
    maintenance_ok_response(request, before, after, true, deleted, pre, post, vacuum)
}

fn maintenance_ok_response(
    request: &MaintenanceRequestV1,
    before: MaintenanceSizeV1,
    after: MaintenanceSizeV1,
    after_complete: bool,
    deleted: MaintenanceDeletedV1,
    pre: MaintenancePreIntegrityCheck,
    post: MaintenancePostIntegrityCheck,
    vacuum: MaintenanceVacuumV1,
) -> MaintenanceResponseV1 {
    MaintenanceResponseV1 {
        contract: "ai_daily_scanner_maintenance".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        status: MaintenanceStatus::Ok,
        cache_retention_policy: cache::cache_retention_policy(),
        before,
        after,
        after_complete,
        deleted,
        pre_integrity_check: pre,
        post_integrity_check: post,
        vacuum,
        warnings: Vec::new(),
        error: Nullable(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn maintenance_error_response(
    request: &MaintenanceRequestV1,
    before: Option<MaintenanceSizeV1>,
    after: Option<MaintenanceSizeV1>,
    after_complete: bool,
    deleted: MaintenanceDeletedV1,
    pre: MaintenancePreIntegrityCheck,
    post: MaintenancePostIntegrityCheck,
    vacuum: Option<MaintenanceVacuumV1>,
    error: Diagnostic,
) -> MaintenanceResponseV1 {
    let before = before.unwrap_or_else(zero_maintenance_size);
    let after = after.unwrap_or_else(|| before.clone());
    MaintenanceResponseV1 {
        contract: "ai_daily_scanner_maintenance".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        status: MaintenanceStatus::Error,
        cache_retention_policy: cache::cache_retention_policy(),
        before,
        after,
        after_complete,
        deleted,
        pre_integrity_check: pre,
        post_integrity_check: post,
        vacuum: vacuum.unwrap_or(MaintenanceVacuumV1 {
            mode: request.mode,
            status: MaintenanceVacuumStatus::Error,
            pages_changed: 0,
        }),
        warnings: Vec::new(),
        error: Nullable(Some(error)),
    }
}

fn zero_maintenance_deleted() -> MaintenanceDeletedV1 {
    MaintenanceDeletedV1 {
        parse_cache_rows: 0,
        classification_cache_rows: 0,
        context_artifacts_rows: 0,
        context_artifact_files_rows: 0,
        context_artifact_decisions_rows: 0,
        scan_runs_rows: 0,
        scan_run_attempts_rows: 0,
        run_diagnostics_rows: 0,
        scan_file_results_rows: 0,
        scan_stage_metrics_rows: 0,
        scan_extension_metrics_rows: 0,
        context_runs_rows: 0,
        context_decisions_rows: 0,
        file_inventory_rows: 0,
    }
}

fn zero_maintenance_size() -> MaintenanceSizeV1 {
    MaintenanceSizeV1 {
        parse_cache_logical_bytes: 0,
        classification_cache_logical_bytes: 0,
        context_artifacts_logical_bytes: 0,
        terminal_audit_logical_bytes: 0,
        database_file_bytes: 0,
        wal_file_bytes: 0,
        shm_file_bytes: 0,
        total_physical_bytes: 0,
        freelist_bytes: 0,
        auto_vacuum_mode: AutoVacuumMode::None,
    }
}

fn maintenance_sizes(
    connection: &Connection,
    db_path: &Path,
) -> Result<MaintenanceSizeV1, StoreError> {
    let sum = |sql: &str| -> Result<u64, StoreError> {
        connection
            .query_row(sql, [], |row| row.get::<_, i64>(0))
            .map_err(cache_open)
            .map(|value| value.max(0) as u64)
    };
    let parse_cache_logical_bytes =
        sum("SELECT COALESCE(SUM(entry_size_bytes), 0) FROM parse_cache")?;
    let classification_cache_logical_bytes =
        sum("SELECT COALESCE(SUM(entry_size_bytes), 0) FROM classification_cache")?;
    let context_artifacts_logical_bytes =
        sum("SELECT COALESCE(SUM(artifact_size_bytes), 0) FROM context_artifacts")?;
    let terminal_audit_logical_bytes = sum(
        "SELECT COALESCE(SUM(audit_size_bytes), 0) FROM scan_runs
         WHERE status IN ('success', 'partial', 'error', 'abandoned')",
    )?;
    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .map_err(cache_open)?;
    let freelist_count: i64 = connection
        .query_row("PRAGMA freelist_count", [], |row| row.get(0))
        .map_err(cache_open)?;
    let freelist_bytes = page_size.max(0) as u64 * freelist_count.max(0) as u64;
    let database_file_bytes = fs::metadata(db_path).map(|meta| meta.len()).unwrap_or(0);
    let wal_file_bytes = sidecar_len(db_path, "-wal");
    let shm_file_bytes = sidecar_len(db_path, "-shm");
    let total_physical_bytes = database_file_bytes + wal_file_bytes + shm_file_bytes;
    Ok(MaintenanceSizeV1 {
        parse_cache_logical_bytes,
        classification_cache_logical_bytes,
        context_artifacts_logical_bytes,
        terminal_audit_logical_bytes,
        database_file_bytes,
        wal_file_bytes,
        shm_file_bytes,
        total_physical_bytes,
        freelist_bytes,
        auto_vacuum_mode: auto_vacuum_mode(connection)?,
    })
}

fn sidecar_len(db_path: &Path, kind: &str) -> u64 {
    fs::metadata(sqlite_sidecar_path(db_path, kind))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

fn auto_vacuum_mode(connection: &Connection) -> Result<AutoVacuumMode, StoreError> {
    let value: i64 = connection
        .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
        .map_err(cache_open)?;
    Ok(match value {
        1 => AutoVacuumMode::Full,
        2 => AutoVacuumMode::Incremental,
        _ => AutoVacuumMode::None,
    })
}

/// 单次 integrity check（spec Part 4/5.3）：`PRAGMA integrity_check` +
/// `foreign_key_check` + entry_size 全量重算 + artifact hash/row-count/nullability
/// invariant 全部通过才为 ok。
fn maintenance_integrity(connection: &Connection) -> MaintenancePreIntegrityCheck {
    if integrity_checks_pass(connection) {
        MaintenancePreIntegrityCheck::Ok
    } else {
        MaintenancePreIntegrityCheck::Failed
    }
}

fn maintenance_post_integrity(connection: &Connection) -> MaintenancePostIntegrityCheck {
    if integrity_checks_pass(connection) {
        MaintenancePostIntegrityCheck::Ok
    } else {
        MaintenancePostIntegrityCheck::Failed
    }
}

fn integrity_checks_pass(connection: &Connection) -> bool {
    let integrity_ok: bool = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .map(|value| value == "ok")
        .unwrap_or(false);
    if !integrity_ok {
        return false;
    }
    let fk_ok: bool = connection
        .prepare("PRAGMA foreign_key_check")
        .and_then(|mut statement| statement.query_map([], |_| Ok(())).map(|rows| rows.count()))
        .map(|count| count == 0)
        .unwrap_or(false);
    if !fk_ok {
        return false;
    }
    if !parse_cache_entry_sizes_match(connection) {
        return false;
    }
    if !classification_cache_entry_sizes_match(connection) {
        return false;
    }
    if !artifact_invariants_hold(connection) {
        return false;
    }
    true
}

fn parse_cache_entry_sizes_match(connection: &Connection) -> bool {
    let mut statement = match connection.prepare(
        "SELECT file_identity, source_version, source_guard_kind, source_guard_sha256,
                parse_profile_hash, content, content_sha256, parser_backend, worker_lane,
                worker_contract_version, worker_version, worker_build, entry_size_bytes
         FROM parse_cache",
    ) {
        Ok(statement) => statement,
        Err(_) => return false,
    };
    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(_) => return false,
    };
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(_) => return false,
        };
        let columns: Vec<String> = (0..12)
            .map(|index| row.get::<_, String>(index).unwrap_or_default())
            .collect();
        let stored: i64 = row.get(12).unwrap_or(-1);
        let text_bytes: usize = columns.iter().map(|value| value.len()).sum();
        if stored != text_bytes as i64 + 8 {
            return false;
        }
    }
    true
}

fn classification_cache_entry_sizes_match(connection: &Connection) -> bool {
    let mut statement = match connection.prepare(
        "SELECT file_identity, source_version, source_guard_kind, source_guard_sha256,
                classifier_profile_hash, classifier_build, status, entry_size_bytes
         FROM classification_cache",
    ) {
        Ok(statement) => statement,
        Err(_) => return false,
    };
    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(_) => return false,
    };
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(_) => return false,
        };
        let columns: Vec<String> = (0..7)
            .map(|index| row.get::<_, String>(index).unwrap_or_default())
            .collect();
        let stored: i64 = row.get(7).unwrap_or(-1);
        let text_bytes: usize = columns.iter().map(|value| value.len()).sum();
        if stored != text_bytes as i64 + 16 {
            return false;
        }
    }
    true
}

fn artifact_invariants_hold(connection: &Connection) -> bool {
    let nullability_bad: i64 = connection
        .query_row(
            "SELECT count(*) FROM context_artifacts WHERE
                (snapshot_eligible = 1 AND (snapshot_key_sha256 IS NULL OR snapshot_key_json IS NULL))
             OR (snapshot_eligible = 0 AND (snapshot_key_sha256 IS NOT NULL OR snapshot_key_json IS NOT NULL))",
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    if nullability_bad != 0 {
        return false;
    }
    let rowcount_bad: i64 = connection
        .query_row(
            "SELECT count(*) FROM context_artifacts a WHERE
                (a.snapshot_eligible = 1 AND NOT EXISTS(
                    SELECT 1 FROM context_artifact_files f WHERE f.artifact_id = a.artifact_id
                 ))
             OR (a.snapshot_eligible = 1 AND NOT EXISTS(
                    SELECT 1 FROM context_artifact_decisions d WHERE d.artifact_id = a.artifact_id
                 ))
             OR (a.snapshot_eligible = 0 AND EXISTS(
                    SELECT 1 FROM context_artifact_files f WHERE f.artifact_id = a.artifact_id
                 ))
             OR (a.snapshot_eligible = 0 AND EXISTS(
                    SELECT 1 FROM context_artifact_decisions d WHERE d.artifact_id = a.artifact_id
                 ))",
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    if rowcount_bad != 0 {
        return false;
    }
    let eligible_count_mismatch: i64 = connection
        .query_row(
            "SELECT count(*) FROM context_artifacts a WHERE a.snapshot_eligible = 1 AND
                (SELECT count(*) FROM context_artifact_files f WHERE f.artifact_id = a.artifact_id)
                <> (SELECT count(*) FROM context_artifact_decisions d WHERE d.artifact_id = a.artifact_id)",
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    if eligible_count_mismatch != 0 {
        return false;
    }
    let mut statement = match connection
        .prepare("SELECT final_context, context_sha256 FROM context_artifacts")
    {
        Ok(statement) => statement,
        Err(_) => return false,
    };
    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(_) => return false,
    };
    loop {
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(_) => return false,
        };
        let final_context: String = row.get(0).unwrap_or_default();
        let context_sha256: String = row.get(1).unwrap_or_default();
        if cache::sha256_hex(final_context.as_bytes()) != context_sha256 {
            return false;
        }
    }
    true
}

fn run_maintenance_gc(
    connection: &mut Connection,
    now_ms: i64,
) -> Result<MaintenanceDeletedV1, StoreError> {
    let before = table_counts(connection)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(cache_write)?;
    // maintenance holds the exclusive lease; any remaining running row is stale.
    expire_stale_running_runs(&transaction, now_ms).map_err(cache_write)?;
    gc_terminal_runs(&transaction, now_ms).map_err(cache_write)?;
    gc_orphan_artifacts(&transaction).map_err(cache_write)?;
    cache::evict_parse_cache(&transaction, cache::PARSE_CACHE_MAX_BYTES)
        .map_err(cache_write)?;
    cache::evict_classification_cache(&transaction, cache::CLASSIFICATION_CACHE_MAX_BYTES)
        .map_err(cache_write)?;
    // Count the deletions inside the transaction so a commit failure rolls back
    // everything and the reported deleted counts stay honest (zero on rollback).
    let after = table_counts(&*transaction)?;
    transaction.commit().map_err(cache_write)?;
    Ok(deleted_diff(&before, &after))
}

fn expire_stale_running_runs(
    transaction: &rusqlite::Transaction<'_>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    transaction.execute(
        "UPDATE scan_runs SET status='abandoned', finished_at_ms=?1, updated_at_ms=?1
         WHERE status='running'",
        params![now_ms],
    )?;
    Ok(())
}

/// Terminal run GC（spec Part 4）：先删超 90 天且不在 protected set 的 rows
///（maintenance 无当前 record，protected set 为空），再按
/// `(finished_at_ms ASC, scan_run_id ASC)` 删到 ≤500 runs 且 ≤2GiB。不级联删除
/// 全局 inventory/parse cache。
fn gc_terminal_runs(
    transaction: &rusqlite::Transaction<'_>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let cutoff_ms = now_ms.saturating_sub(cache::TERMINAL_RUN_MAX_AGE_DAYS * 86_400_000);
    transaction.execute(
        "DELETE FROM scan_runs
         WHERE status IN ('success', 'partial', 'error', 'abandoned')
           AND finished_at_ms IS NOT NULL AND finished_at_ms < ?1",
        params![cutoff_ms],
    )?;
    loop {
        let count: i64 = transaction.query_row(
            "SELECT count(*) FROM scan_runs
             WHERE status IN ('success', 'partial', 'error', 'abandoned')",
            [],
            |row| row.get(0),
        )?;
        let total: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(audit_size_bytes), 0) FROM scan_runs
             WHERE status IN ('success', 'partial', 'error', 'abandoned')",
            [],
            |row| row.get(0),
        )?;
        if count <= cache::TERMINAL_RUN_MAX_COUNT && total <= cache::TERMINAL_AUDIT_MAX_BYTES {
            break;
        }
        let deleted = transaction.execute(
            "DELETE FROM scan_runs WHERE scan_run_id IN (
                SELECT scan_run_id FROM scan_runs
                WHERE status IN ('success', 'partial', 'error', 'abandoned')
                ORDER BY finished_at_ms ASC, scan_run_id ASC
                LIMIT 1
             )",
            [],
        )?;
        if deleted == 0 {
            break;
        }
    }
    Ok(())
}

/// Orphan artifact GC（spec Part 4）：只删除引用计数为零（无任何
/// `context_runs.artifact_id`）的 artifact；被引用的绝不淘汰。
fn gc_orphan_artifacts(transaction: &rusqlite::Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute(
        "DELETE FROM context_artifacts
         WHERE NOT EXISTS(
             SELECT 1 FROM context_runs WHERE context_runs.artifact_id = context_artifacts.artifact_id
         )",
        [],
    )?;
    Ok(())
}

/// `scan_runs.audit_size_bytes` 的逻辑 payload 近似（spec Part 4）：覆盖 run 的
/// envelope/attempt/diagnostic/current file/decision/stage/extension/context-run
/// 元数据；不含其引用的 artifact、全局 inventory 或 cache。Store 在 insert 时
/// 计算，不接受 wire 输入。
/// Logical INSERT/UPDATE row count of the terminal transaction (spec Part 5.3
/// `terminal_rows_written`): current run/artifact/context/metric rows only;
/// retention DELETE, cache-transaction and maintenance rows are excluded.
fn compute_terminal_rows_written(batch: &FinalizationBatch) -> u64 {
    let mut rows = 0_u64;
    rows = rows.saturating_add(batch.inventory.len() as u64);
    rows = rows.saturating_add(batch.cache_writes.len() as u64);
    rows = rows.saturating_add(batch.file_results.len() as u64);
    rows = rows.saturating_add(batch.file_results.len() as u64); // scan_file_execution_v2
    rows = rows.saturating_add(batch.diagnostics.len() as u64);
    rows = rows.saturating_add(batch.stage_metrics.len() as u64);
    rows = rows.saturating_add(batch.extension_metrics.len() as u64);
    if let Some(context) = &batch.context {
        rows = rows.saturating_add(1); // context_runs row
        rows = rows.saturating_add(context.decisions.len() as u64);
        if batch.snapshot_hit.is_none() {
            if let Some(artifact) = &batch.artifact {
                rows = rows.saturating_add(1); // artifact parent
                rows = rows.saturating_add(artifact.file_rows.len() as u64);
                rows = rows.saturating_add(artifact.decision_rows.len() as u64);
            }
        }
    }
    rows = rows.saturating_add(2); // scan_runs + scan_run_attempts UPDATE
    rows = rows.saturating_add(1); // execution_metrics row
    rows
}

fn elapsed_ms(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis() as u64
}

fn compute_audit_size(batch: &FinalizationBatch) -> i64 {
    let mut total = batch.envelope_json.len() as i64;
    total += batch.inventory.len() as i64 * 256;
    for record in &batch.file_results {
        total += record.relative_path.len() as i64 + 256;
        total += file_execution_audit_size(record, batch.snapshot_hit.is_some());
    }
    for record in &batch.diagnostics {
        total += record.diagnostic.message.len() as i64 + 160;
    }
    total += batch.stage_metrics.len() as i64 * 48;
    for record in &batch.extension_metrics {
        total += record.extension.len() as i64 + 64;
    }
    if let Some(context) = &batch.context {
        total += context.final_context.len() as i64 + 320;
        for record in &context.decisions {
            total += record.decision.relative_path.len() as i64 + 160;
        }
    }
    total
}

fn file_execution_audit_size(record: &FileResultRecord, snapshot_rows: bool) -> i64 {
    const INTEGER_BYTES: i64 = 8;
    const NULL_MARKER_BYTES: i64 = 1;

    let text_bytes = |value: &str| i64::try_from(value.len()).unwrap_or(i64::MAX);
    let nullable_integer_bytes = |value: Option<u64>| {
        if value.is_some() {
            INTEGER_BYTES
        } else {
            NULL_MARKER_BYTES
        }
    };
    let parse_transport = if snapshot_rows {
        "snapshot".to_string()
    } else {
        inventory::enum_text(&record.parse_transport)
    };
    let mut total = 64_i64
        .saturating_add(INTEGER_BYTES)
        .saturating_add(text_bytes(&record.file_identity))
        .saturating_add(text_bytes(&parse_transport))
        .saturating_add(INTEGER_BYTES);

    let Some(classification) = record.pdf_classification.as_ref() else {
        return total.saturating_add(12 * NULL_MARKER_BYTES);
    };
    let classification_cache_status = if snapshot_rows
        && classification.status
            != ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
    {
        "snapshot".to_string()
    } else {
        inventory::enum_text(&classification.classification_cache_status)
    };
    let classification_miss_reason = if snapshot_rows {
        ""
    } else {
        classification.classification_cache_miss_reason.as_str()
    };
    let classification_transport = if snapshot_rows
        && classification.status
            != ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
    {
        "snapshot".to_string()
    } else {
        inventory::enum_text(&classification.transport)
    };
    total = total
        .saturating_add(text_bytes(&inventory::enum_text(&classification.status)))
        .saturating_add(nullable_integer_bytes(classification.page_count.0))
        .saturating_add(text_bytes(&classification_cache_status))
        .saturating_add(text_bytes(classification_miss_reason))
        .saturating_add(nullable_integer_bytes(
            classification.result_examined_pages.0,
        ))
        .saturating_add(nullable_integer_bytes(
            classification.run_inspected_pages.0,
        ))
        .saturating_add(INTEGER_BYTES)
        .saturating_add(INTEGER_BYTES)
        .saturating_add(text_bytes(&classification_transport))
        .saturating_add(INTEGER_BYTES)
        .saturating_add(text_bytes(&classification.classifier_build))
        .saturating_add(text_bytes(&classification.classifier_profile_hash));
    total
}

/// Terminal run GC for the CURRENT finalize（spec Part 4）：先删超 90 天且不在
/// protected set（当前 run）的 rows，再按 `(finished_at_ms ASC, scan_run_id ASC)`
/// 删到 ≤500 runs 且 ≤2GiB。为当前 record 腾挪使用「现存 + 当前 audit」比较；
/// 若删尽未保护旧 run 后当前 record 自身仍超 cap → fail closed（不部分落 audit）。
fn retention_gc_for_current_run(
    transaction: &rusqlite::Transaction<'_>,
    protected_runs: &HashSet<i64>,
    now_ms: i64,
    audit_size: i64,
) -> Result<(), StoreError> {
    if audit_size > cache::TERMINAL_AUDIT_MAX_BYTES {
        return Err(StoreError::RunCorrupt(
            "current terminal audit exceeds the 2 GiB retention cap".to_string(),
        ));
    }
    let cutoff_ms = now_ms.saturating_sub(cache::TERMINAL_RUN_MAX_AGE_DAYS * 86_400_000);
    let clause = terminal_run_not_in_clause(protected_runs);
    let sql = format!(
        "DELETE FROM scan_runs
         WHERE status IN ('success', 'partial', 'error', 'abandoned')
           AND finished_at_ms IS NOT NULL AND finished_at_ms < ?1
           {clause}"
    );
    let mut params = vec![cutoff_ms];
    params.extend(protected_runs.iter().copied());
    transaction
        .execute(&sql, rusqlite::params_from_iter(params.iter()))
        .map_err(cache_write)?;
    loop {
        let count: i64 = transaction
            .query_row(
                "SELECT count(*) FROM scan_runs
                 WHERE status IN ('success', 'partial', 'error', 'abandoned')",
                [],
                |row| row.get(0),
            )
            .map_err(cache_write)?;
        let total: i64 = transaction
            .query_row(
                "SELECT COALESCE(SUM(audit_size_bytes), 0) FROM scan_runs
                 WHERE status IN ('success', 'partial', 'error', 'abandoned')",
                [],
                |row| row.get(0),
            )
            .map_err(cache_write)?;
        if count <= cache::TERMINAL_RUN_MAX_COUNT && total <= cache::TERMINAL_AUDIT_MAX_BYTES {
            return Ok(());
        }
        let clause = terminal_run_not_in_clause(protected_runs);
        let sql = format!(
            "DELETE FROM scan_runs WHERE scan_run_id IN (
                SELECT scan_run_id FROM scan_runs
                WHERE status IN ('success', 'partial', 'error', 'abandoned')
                  {clause}
                ORDER BY finished_at_ms ASC, scan_run_id ASC
                LIMIT 1
             )"
        );
        let mut params: Vec<i64> = protected_runs.iter().copied().collect();
        params.sort_unstable();
        let deleted = transaction
            .execute(&sql, rusqlite::params_from_iter(params.iter()))
            .map_err(cache_write)?;
        if deleted == 0 {
            // 无未保护旧 run 可删，仍超 cap → fail closed。
            return Err(StoreError::RunCorrupt(
                "terminal audit retention could not make room for the current record".to_string(),
            ));
        }
    }
}

fn terminal_run_not_in_clause(protected_runs: &HashSet<i64>) -> String {
    if protected_runs.is_empty() {
        return String::new();
    }
    let placeholders = vec!["?"; protected_runs.len()].join(",");
    format!(" AND scan_run_id NOT IN ({placeholders})")
}

fn run_incremental_vacuum(connection: &Connection) -> Result<u64, StoreError> {
    let before_freelist: i64 = connection
        .query_row("PRAGMA freelist_count", [], |row| row.get(0))
        .map_err(cache_open)?;
    connection
        .execute_batch("PRAGMA incremental_vacuum;")
        .map_err(cache_write)?;
    let after_freelist: i64 = connection
        .query_row("PRAGMA freelist_count", [], |row| row.get(0))
        .map_err(cache_open)?;
    Ok(before_freelist.max(0).saturating_sub(after_freelist.max(0)) as u64)
}

fn table_counts(connection: &Connection) -> Result<MaintenanceDeletedV1, StoreError> {
    Ok(MaintenanceDeletedV1 {
        parse_cache_rows: count_table(connection, "parse_cache")?,
        classification_cache_rows: count_table(connection, "classification_cache")?,
        context_artifacts_rows: count_table(connection, "context_artifacts")?,
        context_artifact_files_rows: count_table(connection, "context_artifact_files")?,
        context_artifact_decisions_rows: count_table(connection, "context_artifact_decisions")?,
        scan_runs_rows: count_table(connection, "scan_runs")?,
        scan_run_attempts_rows: count_table(connection, "scan_run_attempts")?,
        run_diagnostics_rows: count_table(connection, "run_diagnostics")?,
        scan_file_results_rows: count_table(connection, "scan_file_results")?,
        scan_stage_metrics_rows: count_table(connection, "scan_stage_metrics")?,
        scan_extension_metrics_rows: count_table(connection, "scan_extension_metrics")?,
        context_runs_rows: count_table(connection, "context_runs")?,
        context_decisions_rows: count_table(connection, "context_decisions")?,
        file_inventory_rows: count_table(connection, "file_inventory")?,
    })
}

fn count_table(connection: &Connection, table: &str) -> Result<u64, StoreError> {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(cache_open)
        .map(|count| count.max(0) as u64)
}

fn deleted_diff(
    before: &MaintenanceDeletedV1,
    after: &MaintenanceDeletedV1,
) -> MaintenanceDeletedV1 {
    MaintenanceDeletedV1 {
        parse_cache_rows: before.parse_cache_rows.saturating_sub(after.parse_cache_rows),
        classification_cache_rows: before
            .classification_cache_rows
            .saturating_sub(after.classification_cache_rows),
        context_artifacts_rows: before
            .context_artifacts_rows
            .saturating_sub(after.context_artifacts_rows),
        context_artifact_files_rows: before
            .context_artifact_files_rows
            .saturating_sub(after.context_artifact_files_rows),
        context_artifact_decisions_rows: before
            .context_artifact_decisions_rows
            .saturating_sub(after.context_artifact_decisions_rows),
        scan_runs_rows: before.scan_runs_rows.saturating_sub(after.scan_runs_rows),
        scan_run_attempts_rows: before
            .scan_run_attempts_rows
            .saturating_sub(after.scan_run_attempts_rows),
        run_diagnostics_rows: before
            .run_diagnostics_rows
            .saturating_sub(after.run_diagnostics_rows),
        scan_file_results_rows: before
            .scan_file_results_rows
            .saturating_sub(after.scan_file_results_rows),
        scan_stage_metrics_rows: before
            .scan_stage_metrics_rows
            .saturating_sub(after.scan_stage_metrics_rows),
        scan_extension_metrics_rows: before
            .scan_extension_metrics_rows
            .saturating_sub(after.scan_extension_metrics_rows),
        context_runs_rows: before.context_runs_rows.saturating_sub(after.context_runs_rows),
        context_decisions_rows: before
            .context_decisions_rows
            .saturating_sub(after.context_decisions_rows),
        file_inventory_rows: before
            .file_inventory_rows
            .saturating_sub(after.file_inventory_rows),
    }
}

fn remaining_opportunistic_ms(deadline: i64) -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| deadline.saturating_sub(duration.as_millis() as i64))
        .unwrap_or(0)
}

fn acquire_upgrade_lease(connection: &mut Connection, now_ms: i64) -> Result<(), StoreError> {
    if let Some(lease) = query_lease(connection)? {
        if lease_is_live(&lease, now_ms) {
            return Err(StoreError::ScanAlreadyRunning);
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        reclaim_expired_lease(&transaction, &lease, now_ms)?;
        transaction.commit().map_err(cache_write)?;
    }
    let owner_id = random_owner_id(connection)?;
    connection
        .execute(
            "INSERT INTO engine_lease(
                lease_key, owner_id, owner_pid, acquired_at_ms, heartbeat_at_ms, expires_at_ms
             ) VALUES (1, ?1, ?2, ?3, ?3, ?4)",
            params![
                owner_id,
                i64::from(std::process::id()),
                now_ms,
                lease_expiry(now_ms)?,
            ],
        )
        .map_err(cache_write)?;
    Ok(())
}

fn release_upgrade_lease(connection: &Connection) -> Result<(), StoreError> {
    connection
        .execute("DELETE FROM engine_lease WHERE lease_key=1", [])
        .map_err(cache_write)?;
    Ok(())
}

fn convert_auto_vacuum(connection: &Connection) -> Result<bool, StoreError> {
    #[cfg(test)]
    if TEST_FORCE_AUTO_VACUUM_FAILURE.with(|flag| flag.get()) {
        // Test seam: simulate the post-migration vacuum failing so the
        // `partial`/`auto_vacuum_converted=false` response shape is exercised.
        return Ok(false);
    }
    connection
        .execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")
        .map_err(cache_write)?;
    let auto_vacuum: i64 = connection
        .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
        .map_err(cache_write)?;
    Ok(auto_vacuum == 2)
}

#[cfg(test)]
thread_local! {
    static TEST_FORCE_AUTO_VACUUM_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

fn checked_i64(value: u64, field: &str) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::InvalidRequest(format!("{field} exceeds SQLite integer range")))
}

/// SourceGuardV2 wire for the parse-cache key: only an available kind with a
/// 64-char lowercase-hex SHA-256 (unavailable files never reach the cache).
fn valid_guard_wire(kind: &str, hash: &str) -> bool {
    matches!(
        kind,
        "windows_file_id_change_time_v1" | "unix_inode_ctime_v1" | "content_sha256_v1"
    ) && inventory::is_sha256(hash)
}

fn now_millis() -> Result<u64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|_| StoreError::InvalidRequest("system clock is invalid".to_string()))
}

pub fn current_time_millis() -> Result<u64, StoreError> {
    now_millis()
}

fn cache_open(error: impl ToString) -> StoreError {
    StoreError::CacheOpen {
        detail: error.to_string(),
    }
}

fn cache_write(error: impl ToString) -> StoreError {
    StoreError::CacheWrite {
        detail: error.to_string(),
    }
}

fn query_existing_run(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<ExistingRun>, StoreError> {
    let row: Option<(i64, String, String, String, Option<String>)> = connection
        .query_row(
            "SELECT scan_run_id, canonical_request_json, request_hash, status,
                    final_envelope_metadata_json
             FROM scan_runs WHERE request_id=?1",
            [request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(cache_open)?;
    row.map(|row| {
        Ok(ExistingRun {
            scan_run_id: row.0,
            canonical_request_json: row.1,
            request_hash: row.2,
            status: PersistedRunStatus::parse(&row.3)?,
            final_envelope_metadata_json: row.4,
        })
    })
    .transpose()
}

fn ensure_request_hash(
    existing: &ExistingRun,
    canonical: &CanonicalRequest,
) -> Result<(), StoreError> {
    if cache::domain_hash(b"request-v1\0", existing.canonical_request_json.as_bytes())
        != existing.request_hash
    {
        return Err(StoreError::RunCorrupt(
            "stored logical request hash is invalid".to_string(),
        ));
    }
    if existing.request_hash != canonical.hash || existing.canonical_request_json != canonical.json
    {
        return Err(StoreError::RequestIdConflict);
    }
    Ok(())
}

fn load_stored_envelope(
    connection: &Connection,
    existing: ExistingRun,
    request_id: &str,
) -> Result<BeginRunOutcome, StoreError> {
    load_stored_envelope_ref(connection, &existing, request_id)
        .map(Box::new)
        .map(BeginRunOutcome::Stored)
}

/// Spec Part 5.1 idempotent replay: the full `ContextEnvelope v1` is REBUILT
/// from the body-free `final_envelope_metadata_json` + the stored summary +
/// the artifact's `final_context` (Success/Partial), never read from a
/// body-carrying scan_runs JSON. `final_context` is stored exactly once.
fn load_stored_envelope_ref(
    connection: &Connection,
    existing: &ExistingRun,
    request_id: &str,
) -> Result<StoredEnvelope, StoreError> {
    if cache::domain_hash(b"request-v1\0", existing.canonical_request_json.as_bytes())
        != existing.request_hash
    {
        return Err(StoreError::RunCorrupt(
            "stored logical request hash is invalid".to_string(),
        ));
    }
    if !existing.status.is_terminal() {
        return Err(StoreError::RunCorrupt(
            "nonterminal run has no reusable envelope".to_string(),
        ));
    }
    let metadata_json = existing
        .final_envelope_metadata_json
        .clone()
        .ok_or_else(|| StoreError::RunCorrupt("terminal run has no envelope metadata".to_string()))?;
    let envelope = rebuild_envelope_from_metadata(connection, existing.scan_run_id, &metadata_json)?;
    let envelope_json = canonical_envelope_json(&envelope)?;
    if envelope.request_id != request_id
        || envelope.scan_run_id.0 != Some(existing.scan_run_id as u64)
        || !envelope_status_matches(existing.status, envelope.status)
    {
        return Err(StoreError::RunCorrupt(
            "rebuilt envelope does not match its run".to_string(),
        ));
    }
    Ok(StoredEnvelope {
        scan_run_id: existing.scan_run_id as u64,
        envelope_json,
        envelope,
    })
}

/// Shared spec Part 5.1 envelope rebuild for both idempotent replay and
/// inspect: parse the body-free `final_envelope_metadata_json`, read the stored
/// summary, load the referenced artifact, and rebuild + re-validate the full
/// `ContextEnvelope v1`. `final_context` is never read from scan_runs.
pub(crate) fn rebuild_envelope_from_metadata(
    connection: &Connection,
    scan_run_id: i64,
    metadata_json: &str,
) -> Result<ContextEnvelope, StoreError> {
    let metadata: serde_json::Value = serde_json::from_str(metadata_json)
        .map_err(|_| StoreError::RunCorrupt("envelope metadata JSON is invalid".to_string()))?;
    let summary: ContextSummary = serde_json::from_value(
        metadata
            .get("summary")
            .cloned()
            .ok_or_else(|| StoreError::RunCorrupt("envelope metadata missing summary".to_string()))?,
    )
    .map_err(|_| StoreError::RunCorrupt("envelope metadata summary is invalid".to_string()))?;
    let artifact = load_artifact_for_replay(connection, scan_run_id)?;
    let envelope = crate::artifact::rebuild_envelope(&metadata, &summary, artifact.as_ref())
        .map_err(|message| {
            StoreError::RunCorrupt(format!("rebuilt envelope is invalid: {message}"))
        })?;
    envelope
        .validate()
        .map_err(|_| StoreError::RunCorrupt("rebuilt envelope violates the contract".to_string()))?;
    Ok(envelope)
}

/// Loads the artifact referenced by a run's `context_runs` row for replay
/// (spec Part 5.1). Error runs and runs without an artifact return `None`.
fn load_artifact_for_replay(
    connection: &Connection,
    scan_run_id: i64,
) -> Result<Option<ArtifactDraft>, StoreError> {
    let artifact_id: Option<Option<i64>> = connection
        .query_row(
            "SELECT artifact_id FROM context_runs WHERE scan_run_id=?1",
            [scan_run_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(cache_open)?;
    match artifact_id {
        Some(Some(id)) => Ok(Some(load_artifact_from_connection(connection, id)?)),
        _ => Ok(None),
    }
}
fn envelope_status_matches(run_status: PersistedRunStatus, engine_status: EngineStatus) -> bool {
    matches!(
        (run_status, engine_status),
        (PersistedRunStatus::Success, EngineStatus::Ok)
            | (PersistedRunStatus::Partial, EngineStatus::Partial)
            | (PersistedRunStatus::Error, EngineStatus::Error)
    )
}

fn query_lease(connection: &Connection) -> Result<Option<ExistingLease>, StoreError> {
    connection
        .query_row(
            "SELECT owner_id, heartbeat_at_ms, expires_at_ms FROM engine_lease WHERE lease_key=1",
            [],
            |row| {
                Ok(ExistingLease {
                    owner_id: row.get(0)?,
                    heartbeat_at_ms: row.get(1)?,
                    expires_at_ms: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(cache_write)
}

fn lease_is_live(lease: &ExistingLease, now_ms: i64) -> bool {
    let heartbeat_cutoff = now_ms.saturating_sub(LEASE_GRACE_MS as i64);
    lease.expires_at_ms > now_ms || lease.heartbeat_at_ms > heartbeat_cutoff
}

fn reclaim_expired_lease(
    transaction: &rusqlite::Transaction<'_>,
    lease: &ExistingLease,
    now_ms: i64,
) -> Result<(), StoreError> {
    if lease_is_live(lease, now_ms) {
        return Err(StoreError::ScanAlreadyRunning);
    }
    transaction
        .execute(
            "UPDATE scan_runs
             SET status='abandoned', updated_at_ms=?1, finished_at_ms=?1
             WHERE owner_id=?2 AND status='running'",
            params![now_ms, lease.owner_id],
        )
        .map_err(cache_write)?;
    transaction
        .execute(
            "UPDATE scan_run_attempts
             SET status='abandoned', finished_at_ms=?1
             WHERE owner_id=?2 AND status='running'",
            params![now_ms, lease.owner_id],
        )
        .map_err(cache_write)?;
    let deleted = transaction
        .execute(
            "DELETE FROM engine_lease WHERE lease_key=1 AND owner_id=?1",
            [&lease.owner_id],
        )
        .map_err(cache_write)?;
    if deleted != 1 {
        return Err(StoreError::LeaseLost);
    }
    Ok(())
}

fn random_owner_id(connection: &Connection) -> Result<String, StoreError> {
    let mut hex: Vec<char> = connection
        .query_row("SELECT lower(hex(randomblob(16)))", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(cache_write)?
        .chars()
        .collect();
    if hex.len() != 32 {
        return Err(StoreError::CacheWrite {
            detail: "SQLite random owner id had invalid length".to_string(),
        });
    }
    hex[12] = '4';
    hex[16] = '8';
    let compact: String = hex.into_iter().collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &compact[0..8],
        &compact[8..12],
        &compact[12..16],
        &compact[16..20],
        &compact[20..32]
    ))
}

fn clear_staging_rows(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
) -> Result<(), StoreError> {
    for sql in [
        "DELETE FROM context_runs WHERE scan_run_id=?1",
        "DELETE FROM scan_extension_metrics WHERE scan_run_id=?1",
        "DELETE FROM scan_stage_metrics WHERE scan_run_id=?1",
        "DELETE FROM scan_file_results WHERE scan_run_id=?1",
        "DELETE FROM run_diagnostics WHERE scan_run_id=?1",
    ] {
        transaction
            .execute(sql, [scan_run_id])
            .map_err(cache_write)?;
    }
    Ok(())
}

fn ensure_owner(
    transaction: &rusqlite::Transaction<'_>,
    active: &ActiveRun,
) -> Result<(), StoreError> {
    let owned: bool = transaction
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM scan_runs r JOIN engine_lease l ON l.lease_key=1
                WHERE r.scan_run_id=?1 AND r.owner_id=?2 AND r.status='running'
                  AND l.owner_id=?2
             )",
            params![active.scan_run_id, active.owner_id],
            |row| row.get(0),
        )
        .map_err(cache_write)?;
    if owned {
        Ok(())
    } else {
        Err(StoreError::LeaseLost)
    }
}

fn ensure_worker_fingerprints(
    transaction: &rusqlite::Transaction<'_>,
    active: &ActiveRun,
) -> Result<(), StoreError> {
    let complete: bool = transaction
        .query_row(
            "SELECT
                office_worker_contract IS NOT NULL
                AND office_worker_version IS NOT NULL
                AND office_worker_build IS NOT NULL
                AND python_worker_contract IS NOT NULL
                AND python_worker_version IS NOT NULL
                AND python_worker_build IS NOT NULL
             FROM scan_run_attempts
             WHERE scan_run_id=?1 AND attempt_number=?2 AND owner_id=?3 AND status='running'",
            params![active.scan_run_id, active.attempt_number, active.owner_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(cache_write)?
        .ok_or(StoreError::LeaseLost)?;
    if complete {
        Ok(())
    } else {
        Err(StoreError::RunCorrupt(
            "terminal attempt is missing required worker handshakes".to_string(),
        ))
    }
}

fn ensure_engine_fingerprint(
    transaction: &rusqlite::Transaction<'_>,
    active: &ActiveRun,
    envelope: &ContextEnvelope,
) -> Result<(), StoreError> {
    let persisted: String = transaction
        .query_row(
            "SELECT engine_fingerprint FROM scan_run_attempts
             WHERE scan_run_id=?1 AND attempt_number=?2 AND owner_id=?3 AND status='running'",
            params![active.scan_run_id, active.attempt_number, active.owner_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(cache_write)?
        .ok_or(StoreError::LeaseLost)?;
    let fingerprint: EngineFingerprint = serde_json::from_str(&persisted)
        .map_err(|_| StoreError::RunCorrupt("attempt engine fingerprint is invalid".to_string()))?;
    if fingerprint.contract != envelope.contract
        || fingerprint.protocol_version != envelope.protocol_version
        || fingerprint.engine_version != envelope.engine_version
        || fingerprint.engine_build != envelope.engine_build
    {
        return Err(StoreError::RunCorrupt(
            "final envelope engine identity changed during the run".to_string(),
        ));
    }
    Ok(())
}

fn heartbeat_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    active: &ActiveRun,
    now_ms: i64,
) -> Result<(), StoreError> {
    ensure_owner(transaction, active)?;
    let expires_at_ms = lease_expiry(now_ms)?;
    let heartbeat_cutoff = now_ms.saturating_sub(LEASE_GRACE_MS as i64);
    let updated = transaction
        .execute(
            "UPDATE engine_lease
             SET heartbeat_at_ms=?1, expires_at_ms=?2
             WHERE lease_key=1 AND owner_id=?3
               AND (expires_at_ms > ?1 OR heartbeat_at_ms > ?4)",
            params![now_ms, expires_at_ms, active.owner_id, heartbeat_cutoff],
        )
        .map_err(cache_write)?;
    if updated != 1 {
        return Err(StoreError::LeaseLost);
    }
    transaction
        .execute(
            "UPDATE scan_runs SET updated_at_ms=?1
             WHERE scan_run_id=?2 AND owner_id=?3 AND status='running'",
            params![now_ms, active.scan_run_id, active.owner_id],
        )
        .map_err(cache_write)?;
    Ok(())
}

fn lease_expiry(now_ms: i64) -> Result<i64, StoreError> {
    now_ms
        .checked_add(LEASE_GRACE_MS as i64)
        .ok_or_else(|| StoreError::InvalidRequest("lease timestamp overflows".to_string()))
}

fn validate_finalization(
    active: &ActiveRun,
    batch: &FinalizationBatch,
) -> Result<ContextEnvelope, StoreError> {
    terminal_status_text(batch.status)?;
    if batch.inventory.len() > 1_000_000
        || batch.cache_writes.len() > 1_000_000
        || batch.file_results.len() > 1_000_000
        || batch.diagnostics.len() > 100_001
    {
        return Err(StoreError::InvalidRequest(
            "finalization batch exceeds the v1 row limits".to_string(),
        ));
    }
    let mut inventory_by_identity = std::collections::HashMap::new();
    for record in &batch.inventory {
        record.validate().map_err(StoreError::InvalidRequest)?;
        if inventory_by_identity
            .insert(record.file_identity.as_str(), record)
            .is_some()
        {
            return Err(StoreError::InvalidRequest(
                "inventory identities must be unique".to_string(),
            ));
        }
    }
    let mut cache_keys = std::collections::HashSet::new();
    for record in &batch.cache_writes {
        record.validate().map_err(StoreError::InvalidRequest)?;
        if !inventory_by_identity.contains_key(record.file_identity.as_str())
            || !cache_keys.insert((
                record.file_identity.as_str(),
                record.source_version.as_str(),
                record.parse_profile_hash.as_str(),
            ))
        {
            return Err(StoreError::InvalidRequest(
                "cache writes must reference unique inventory versions".to_string(),
            ));
        }
    }
    let mut file_results_by_identity = std::collections::HashMap::new();
    for record in &batch.file_results {
        record
            .validate_for_persistence(batch.snapshot_hit.is_some())
            .map_err(StoreError::InvalidRequest)?;
        let inventory = inventory_by_identity
            .get(record.file_identity.as_str())
            .ok_or_else(|| {
                StoreError::InvalidRequest(
                    "file result does not reference final inventory".to_string(),
                )
            })?;
        if inventory.source_version != record.source_version
            || file_results_by_identity
                .insert(record.file_identity.as_str(), record)
                .is_some()
        {
            return Err(StoreError::InvalidRequest(
                "file results must uniquely match inventory source versions".to_string(),
            ));
        }
    }
    for cache_write in &batch.cache_writes {
        let result = file_results_by_identity
            .get(cache_write.file_identity.as_str())
            .ok_or_else(|| {
                StoreError::InvalidRequest(
                    "cache write has no matching successful file result".to_string(),
                )
            })?;
        if result.parse_status != ai_daily_scanner_contract::ParseStatus::Success
            || result.source_version != cache_write.source_version
            || result.parse_profile_hash != cache_write.parse_profile_hash
            || result.content_sha256 != cache_write.content_sha256
        {
            return Err(StoreError::InvalidRequest(
                "cache write disagrees with its successful file result".to_string(),
            ));
        }
    }
    for record in &batch.diagnostics {
        record
            .diagnostic
            .validate()
            .map_err(StoreError::InvalidRequest)?;
    }
    crate::metrics::validate_metrics(&batch.stage_metrics, &batch.extension_metrics)
        .map_err(StoreError::InvalidRequest)?;

    let envelope: ContextEnvelope = serde_json::from_str(&batch.envelope_json)
        .map_err(|_| StoreError::InvalidRequest("final envelope JSON is invalid".to_string()))?;
    envelope.validate().map_err(StoreError::InvalidRequest)?;
    if canonical_envelope_json(&envelope).as_deref() != Ok(batch.envelope_json.as_str()) {
        return Err(StoreError::InvalidRequest(
            "final envelope JSON must use canonical serialization".to_string(),
        ));
    }
    if envelope.request_id != active.request_id
        || envelope.scan_run_id.0 != Some(active.scan_run_id())
        || !final_status_matches(batch.status, envelope.status)
    {
        return Err(StoreError::InvalidRequest(
            "final envelope does not match the active run".to_string(),
        ));
    }

    let warning_diagnostics: Vec<Diagnostic> = batch
        .diagnostics
        .iter()
        .filter(|record| record.severity == DiagnosticSeverity::Warning)
        .map(|record| record.diagnostic.clone())
        .collect();
    let error_diagnostics: Vec<Diagnostic> = batch
        .diagnostics
        .iter()
        .filter(|record| record.severity == DiagnosticSeverity::Error)
        .map(|record| record.diagnostic.clone())
        .collect();
    if warning_diagnostics != envelope.warnings
        || error_diagnostics != envelope.error.0.iter().cloned().collect::<Vec<_>>()
    {
        return Err(StoreError::InvalidRequest(
            "final envelope diagnostics disagree with run diagnostics".to_string(),
        ));
    }

    match (&batch.context, envelope.context_run_id.0) {
        (Some(context), Some(context_run_id)) if context_run_id == active.context_run_id() => {
            validate_context(context)?;
            let decision_identities: std::collections::HashSet<&str> = context
                .decisions
                .iter()
                .map(|record| record.file_identity.as_str())
                .collect();
            let inventory_identities: std::collections::HashSet<&str> =
                inventory_by_identity.keys().copied().collect();
            if context.status != batch.status
                || context.final_context != envelope.file_context
                || context.summary != envelope.summary
                || context.summary.source_file_count != batch.inventory.len() as u64
                || decision_identities != inventory_identities
                || file_results_by_identity.len() != inventory_by_identity.len()
            {
                return Err(StoreError::InvalidRequest(
                    "context rows disagree with the final envelope".to_string(),
                ));
            }
            validate_context_relations(
                context,
                &inventory_by_identity,
                &file_results_by_identity,
                &batch.stage_metrics,
                &batch.extension_metrics,
            )?;
        }
        (None, None) => {}
        _ => {
            return Err(StoreError::InvalidRequest(
                "context run identity is inconsistent".to_string(),
            ));
        }
    }
    Ok(envelope)
}

fn validate_context_relations(
    context: &ContextRunRecord,
    inventory: &std::collections::HashMap<&str, &InventoryRecord>,
    file_results: &std::collections::HashMap<&str, &FileResultRecord>,
    stage_metrics: &[StageMetric],
    extension_metrics: &[ExtensionMetric],
) -> Result<(), StoreError> {
    let summary = &context.summary;
    let success_count = file_results
        .values()
        .filter(|result| result.parse_status == ParseStatus::Success)
        .count() as u64;
    let timeout_count = file_results
        .values()
        .filter(|result| result.parse_status == ParseStatus::Timeout)
        .count() as u64;
    let error_count = file_results
        .values()
        .filter(|result| result.parse_status == ParseStatus::Error)
        .count() as u64;
    // spec Part 2.2: `not_parsed_count` is DERIVED, not a stored counter.
    let not_parsed_count = (file_results.len() as u64)
        .checked_sub(success_count)
        .and_then(|value| value.checked_sub(timeout_count))
        .and_then(|value| value.checked_sub(error_count))
        .ok_or_else(|| {
            StoreError::InvalidRequest("file status counts overflow".to_string())
        })?;
    let included_count = context
        .decisions
        .iter()
        .filter(|record| {
            matches!(
                record.decision.action,
                ContextAction::Keep | ContextAction::Compress | ContextAction::MetadataOnly
            )
        })
        .count() as u64;
    let omitted_count = context
        .decisions
        .iter()
        .filter(|record| record.decision.action == ContextAction::Omit)
        .count() as u64;
    let decision_error_count = context
        .decisions
        .iter()
        .filter(|record| record.decision.action == ContextAction::Error)
        .count() as u64;
    let input_chars = context.decisions.iter().try_fold(0_u64, |total, record| {
        total
            .checked_add(record.decision.input_chars)
            .ok_or_else(|| StoreError::InvalidRequest("decision input count overflows".to_string()))
    })?;
    for record in &context.decisions {
        let item = inventory
            .get(record.file_identity.as_str())
            .ok_or_else(|| {
                StoreError::InvalidRequest("decision inventory is missing".to_string())
            })?;
        if item.relative_path != record.decision.relative_path {
            return Err(StoreError::InvalidRequest(
                "decision path disagrees with inventory".to_string(),
            ));
        }
    }
    if summary.success_count != success_count
        || summary.timeout_count != timeout_count
        || summary.error_file_count != error_count
        || summary.included_file_count != included_count
        || summary.omitted_file_count != omitted_count
        // spec Part 2.2 count equations: included = success, omitted = derived
        // not_parsed, decision_error = error + timeout.
        || included_count != success_count
        || omitted_count != not_parsed_count
        || decision_error_count != error_count + timeout_count
        || included_count + omitted_count + decision_error_count != context.decisions.len() as u64
        || summary.input_chars != input_chars
        || summary.output_chars != context.final_context.chars().count() as u64
    {
        return Err(StoreError::InvalidRequest(
            "context summary disagrees with file or decision rows".to_string(),
        ));
    }

    let stage_by_name: std::collections::HashMap<StageName, &StageMetric> = stage_metrics
        .iter()
        .map(|metric| (metric.stage, metric))
        .collect();
    if stage_metrics.len() != 4
        || stage_by_name.len() != 4
        || stage_by_name
            .get(&StageName::Discovery)
            .is_none_or(|metric| {
                metric.item_count != summary.source_file_count
                    || metric.duration_ms != summary.discovery_duration_ms
            })
        || stage_by_name
            .get(&StageName::Cache)
            .is_none_or(|metric| metric.item_count != summary.source_file_count)
        || stage_by_name
            .get(&StageName::Parse)
            .is_none_or(|metric| metric.duration_ms != summary.parse_duration_ms)
        || stage_by_name.get(&StageName::Context).is_none_or(|metric| {
            metric.item_count != context.decisions.len() as u64
                || metric.duration_ms != summary.compression_duration_ms
        })
    {
        return Err(StoreError::InvalidRequest(
            "context summary disagrees with stage metrics".to_string(),
        ));
    }
    let stage_duration = stage_metrics.iter().try_fold(0_u64, |total, metric| {
        total
            .checked_add(metric.duration_ms)
            .ok_or_else(|| StoreError::InvalidRequest("stage durations overflow".to_string()))
    })?;
    if summary.total_duration_ms < stage_duration {
        return Err(StoreError::InvalidRequest(
            "total duration is shorter than its stages".to_string(),
        ));
    }

    let mut expected_extensions: std::collections::BTreeMap<&str, (u64, u64, u64, u64, u64)> =
        std::collections::BTreeMap::new();
    for (identity, item) in inventory {
        let result = file_results.get(identity).ok_or_else(|| {
            StoreError::InvalidRequest("extension metric file result is missing".to_string())
        })?;
        let values = expected_extensions
            .entry(item.file_type.as_str())
            .or_insert((0, 0, 0, 0, 0));
        values.0 = values.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidRequest("extension file count overflows".to_string())
        })?;
        values.1 = values
            .1
            .checked_add(result.parse_duration_ms)
            .ok_or_else(|| {
                StoreError::InvalidRequest("extension duration overflows".to_string())
            })?;
        match result.parse_status {
            ParseStatus::Success => values.2 += 1,
            // spec Part 2.2: extension `error_count` counts ONLY Error; NotParsed
            // is derived as file_count - success - error - timeout.
            ParseStatus::Error => values.3 += 1,
            ParseStatus::Timeout => values.4 += 1,
            ParseStatus::NotParsed => {}
        }
    }
    if extension_metrics.len() != expected_extensions.len()
        || extension_metrics.iter().any(|metric| {
            expected_extensions
                .get(metric.extension.as_str())
                .is_none_or(|expected| {
                    *expected
                        != (
                            metric.file_count,
                            metric.parse_duration_ms,
                            metric.success_count,
                            metric.error_count,
                            metric.timeout_count,
                        )
                })
        })
    {
        return Err(StoreError::InvalidRequest(
            "context summary disagrees with extension metrics".to_string(),
        ));
    }
    Ok(())
}

fn validate_context(context: &ContextRunRecord) -> Result<(), StoreError> {
    terminal_status_text(context.status)?;
    if context.decisions.len() > 1_000_000 {
        return Err(StoreError::InvalidRequest(
            "context decision count exceeds the v1 limit".to_string(),
        ));
    }
    context
        .summary
        .validate()
        .map_err(StoreError::InvalidRequest)?;
    if !inventory::is_sha256(&context.context_profile_hash)
        || !inventory::is_sha256(&context.context_sha256)
        || cache::sha256_hex(context.final_context.as_bytes()) != context.context_sha256
        || !summary_fits_sqlite(&context.summary)
    {
        return Err(StoreError::InvalidRequest(
            "context fingerprint is invalid".to_string(),
        ));
    }
    let mut identities = std::collections::HashSet::new();
    for record in &context.decisions {
        record
            .decision
            .validate()
            .map_err(StoreError::InvalidRequest)?;
        if record.file_identity.is_empty() || !identities.insert(&record.file_identity) {
            return Err(StoreError::InvalidRequest(
                "context decision identity is invalid or duplicate".to_string(),
            ));
        }
        if [
            record.decision.priority,
            record.decision.input_chars,
            record.decision.output_chars,
        ]
        .into_iter()
        .any(|value| value > i64::MAX as u64)
        {
            return Err(StoreError::InvalidRequest(
                "context decision exceeds SQLite integer range".to_string(),
            ));
        }
    }
    Ok(())
}

fn summary_fits_sqlite(summary: &ContextSummary) -> bool {
    [
        summary.source_file_count,
        summary.success_count,
        summary.timeout_count,
        summary.included_file_count,
        summary.omitted_file_count,
        summary.error_file_count,
        summary.input_chars,
        summary.output_chars,
        summary.total_duration_ms,
        summary.discovery_duration_ms,
        summary.parse_duration_ms,
        summary.compression_duration_ms,
    ]
    .into_iter()
    .all(|value| value <= i64::MAX as u64)
}

fn final_status_matches(status: RunStatus, engine_status: EngineStatus) -> bool {
    matches!(
        (status, engine_status),
        (RunStatus::Success, EngineStatus::Ok)
            | (RunStatus::Partial, EngineStatus::Partial)
            | (RunStatus::Error, EngineStatus::Error)
    )
}

fn terminal_status_text(status: RunStatus) -> Result<&'static str, StoreError> {
    match status {
        RunStatus::Success => Ok("success"),
        RunStatus::Partial => Ok("partial"),
        RunStatus::Error => Ok("error"),
        RunStatus::Running | RunStatus::Abandoned => Err(StoreError::InvalidRequest(
            "final status must be success, partial, or error".to_string(),
        )),
    }
}

fn insert_diagnostics(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
    diagnostics: &[RunDiagnosticRecord],
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO run_diagnostics(
            scan_run_id, sequence, severity, error_code, message, retryable,
            stage, file_path, backend
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (sequence, record) in diagnostics.iter().enumerate() {
        statement.execute(params![
            scan_run_id,
            sequence as i64,
            record.severity.as_str(),
            inventory::enum_text(&record.diagnostic.error_code),
            record.diagnostic.message,
            i64::from(record.diagnostic.retryable),
            inventory::enum_text(&record.diagnostic.stage),
            record.diagnostic.file_path.0,
            record.diagnostic.backend.0,
        ])?;
    }
    Ok(())
}

/// Persists the authoritative `execution_metrics` row (spec Part 5.3). Migrated
/// v1 runs have no row; every full_v2 terminal run writes exactly one.
fn insert_execution_metrics(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
    metrics: &ExecutionMetricsV2,
) -> Result<(), StoreError> {
    let all_hit = |value: &Option<bool>| value.map(i64::from);
    let c = |value: u64, field: &str| -> Result<i64, StoreError> {
        checked_i64(value, field)
    };
    transaction
        .execute(
            "INSERT INTO scan_execution_metrics(
                scan_run_id,
                discovery_observed_file_count, source_guard_content_hash_file_count,
                source_guard_unavailable_count, source_guard_bytes_read,
                candidate_file_count, admitted_file_count, classification_slot_count,
                confirmed_run_inspected_pages_total, unobserved_classification_attempt_count,
                nominal_charged_pages_total, extraction_slot_count, pdfplumber_invocations,
                snapshot_hit, parse_cache_lookup_count, classification_cache_lookup_count,
                parse_cache_all_hit, classification_cache_all_hit,
                stage_deadline_exhausted_count, session_restart_count, session_fallback_count,
                classify_attempt_count, parse_attempt_count, reserved_chars, rendered_chars,
                worker_handshake_ms, discovery_ms, snapshot_lookup_ms,
                current_run_audit_write_ms, terminal_precommit_ms,
                deadline_precommit_elapsed_ms, envelope_rebuild_ms,
                terminal_rows_written, peak_worker_rss_bytes
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26,
                       ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34)",
            params![
                scan_run_id,
                c(metrics.discovery_observed_file_count, "discovery_observed_file_count")?,
                c(metrics.source_guard_content_hash_file_count, "source_guard_content_hash_file_count")?,
                c(metrics.source_guard_unavailable_count, "source_guard_unavailable_count")?,
                c(metrics.source_guard_bytes_read, "source_guard_bytes_read")?,
                c(metrics.candidate_file_count, "candidate_file_count")?,
                c(metrics.admitted_file_count, "admitted_file_count")?,
                c(metrics.classification_slot_count, "classification_slot_count")?,
                c(metrics.confirmed_run_inspected_pages_total, "confirmed_run_inspected_pages_total")?,
                c(metrics.unobserved_classification_attempt_count, "unobserved_classification_attempt_count")?,
                c(metrics.nominal_charged_pages_total, "nominal_charged_pages_total")?,
                c(metrics.extraction_slot_count, "extraction_slot_count")?,
                c(metrics.pdfplumber_invocations, "pdfplumber_invocations")?,
                i64::from(metrics.snapshot_hit),
                c(metrics.parse_cache_lookup_count, "parse_cache_lookup_count")?,
                c(metrics.classification_cache_lookup_count, "classification_cache_lookup_count")?,
                all_hit(&metrics.parse_cache_all_hit.0),
                all_hit(&metrics.classification_cache_all_hit.0),
                c(metrics.stage_deadline_exhausted_count, "stage_deadline_exhausted_count")?,
                c(metrics.session_restart_count, "session_restart_count")?,
                c(metrics.session_fallback_count, "session_fallback_count")?,
                c(metrics.classify_attempt_count, "classify_attempt_count")?,
                c(metrics.parse_attempt_count, "parse_attempt_count")?,
                c(metrics.reserved_chars, "reserved_chars")?,
                c(metrics.rendered_chars, "rendered_chars")?,
                c(metrics.worker_handshake_ms, "worker_handshake_ms")?,
                c(metrics.discovery_ms, "discovery_ms")?,
                c(metrics.snapshot_lookup_ms, "snapshot_lookup_ms")?,
                c(metrics.current_run_audit_write_ms, "current_run_audit_write_ms")?,
                c(metrics.terminal_precommit_ms, "terminal_precommit_ms")?,
                c(metrics.deadline_precommit_elapsed_ms, "deadline_precommit_elapsed_ms")?,
                c(metrics.envelope_rebuild_ms, "envelope_rebuild_ms")?,
                c(metrics.terminal_rows_written, "terminal_rows_written")?,
                metrics.peak_worker_rss_bytes.0.map(|value| value as i64),
            ],
        )
        .map_err(cache_write)?;
    Ok(())
}

fn insert_context(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
    context: &ContextRunRecord,
    artifact_id: Option<i64>,
    snapshot_hit: bool,
    reused_from_context_run_id: Option<i64>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let summary = &context.summary;
    transaction.execute(
        "INSERT INTO context_runs(
            context_run_id, scan_run_id, context_profile_hash, status,
            final_context, context_sha256, source_file_count, success_count,
            timeout_count, included_file_count, omitted_file_count,
            error_file_count, input_chars, output_chars, total_duration_ms,
            discovery_duration_ms, parse_duration_ms, compression_duration_ms,
            created_at_ms, artifact_id, reused_from_context_run_id, snapshot_hit
         ) VALUES (
            ?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21
         )",
        params![
            scan_run_id,
            context.context_profile_hash,
            terminal_status_text(context.status).map_err(|_| rusqlite::Error::InvalidQuery)?,
            context.final_context,
            context.context_sha256,
            summary.source_file_count as i64,
            summary.success_count as i64,
            summary.timeout_count as i64,
            summary.included_file_count as i64,
            summary.omitted_file_count as i64,
            summary.error_file_count as i64,
            summary.input_chars as i64,
            summary.output_chars as i64,
            summary.total_duration_ms as i64,
            summary.discovery_duration_ms as i64,
            summary.parse_duration_ms as i64,
            summary.compression_duration_ms as i64,
            now_ms,
            artifact_id,
            reused_from_context_run_id,
            i64::from(snapshot_hit),
        ],
    )?;
    let mut statement = transaction.prepare_cached(
        "INSERT INTO context_decisions(
            context_run_id, file_identity, relative_path, action, reason,
            priority, input_chars, output_chars, truncated, error_code
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for record in &context.decisions {
        statement.execute(params![
            scan_run_id,
            record.file_identity,
            record.decision.relative_path,
            inventory::enum_text(&record.decision.action),
            record.decision.reason,
            record.decision.priority as i64,
            record.decision.input_chars as i64,
            record.decision.output_chars as i64,
            i64::from(record.decision.truncated),
            record.decision.error_code,
        ])?;
    }
    Ok(())
}

fn parse_contract_enum<T>(value: &str) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .map_err(|_| StoreError::RunCorrupt("persisted contract enum is invalid".to_string()))
}

// ---------------------------------------------------------------------------
// context artifact write path + snapshot finalization（spec Part 4/5.1/5.2）
// ---------------------------------------------------------------------------

/// `final_envelope_metadata_json`（spec Part 5.1）：只保存 request/engine/status/
/// warnings/error 等小字段 + summary（供幂等重放从 metadata+summary+artifact 重建
/// 完整 Envelope），`warnings` 恒为数组（绝不 null），`error` 可为 null；
/// `file_context` 不进入 metadata（正文在 artifact 只存一次）。形状必须与
/// `rebuild_envelope` 期望完全一致。
fn envelope_metadata_json(envelope: &ContextEnvelope) -> Result<String, StoreError> {
    let value = serde_json::json!({
        "contract": envelope.contract,
        "protocol_version": envelope.protocol_version,
        "request_id": envelope.request_id,
        "engine_version": envelope.engine_version,
        "engine_build": envelope.engine_build,
        "status": serde_json::to_value(&envelope.status)
            .map_err(|error| StoreError::InvalidRequest(error.to_string()))?,
        "scan_run_id": envelope.scan_run_id.0,
        "context_run_id": envelope.context_run_id.0,
        "warnings": envelope.warnings,
        "error": envelope.error.0,
        "summary": envelope.summary,
    });
    serde_json::to_string(&value).map_err(|error| StoreError::InvalidRequest(error.to_string()))
}

/// spec Part 4: artifact 的 exact logical bytes —— 一次精确覆盖 parent
/// `context_artifacts` 的全部 UTF-8/canonical JSON/BLOB 列（final_context +
/// semantic_summary_json + context_sha256 + snapshot_eligible + created_at_ms +
/// last_accessed_bucket + 可选 snapshot key 两字段）与全部 owned file/decision row
/// 的每个 text 列（含 enum text）与 i64 列。child 不重复计费。写路径与 v1→v2
/// 迁移共用这一个 helper（store/schema.rs 的迁移也调用本函数）。
pub(crate) fn artifact_size_bytes(
    final_context: &str,
    context_sha256: &str,
    semantic_summary_json: &str,
    snapshot_key: Option<&SnapshotKeyParts>,
    file_rows: &[ArtifactFileRow],
    decision_rows: &[ArtifactDecisionRow],
) -> i64 {
    // parent context_artifacts columns
    let mut size = 8  // snapshot_eligible (i64)
        + final_context.len() as i64
        + context_sha256.len() as i64
        + semantic_summary_json.len() as i64
        + 8  // created_at_ms (i64)
        + 10; // last_accessed_bucket TEXT（YYYY-MM-DD，固定 10 字节）
    if let Some(key) = snapshot_key {
        size += key.sha256.len() as i64 + key.canonical_json.len() as i64;
    }
    for row in file_rows {
        size += 8; // artifact_id (i64)
        size += row.file_identity.len() as i64;
        size += row.relative_path.len() as i64;
        size += row.legacy_source_version.len() as i64;
        size += row.source_guard_kind.as_ref().map_or(0, |value| value.len() as i64);
        size += row.source_guard_sha256.as_ref().map_or(0, |value| value.len() as i64);
        size += row.parse_profile_hash.len() as i64;
        size += inventory::enum_text(&row.parse_status).len() as i64; // parse_status TEXT
        size += row.parser_backend.len() as i64;
        size += row.worker_lane.len() as i64;
        size += 8; // truncated (i64)
        size += row.content_sha256.len() as i64;
        if let Some(classifier) = &row.classifier {
            size += inventory::enum_text(&classifier.status).len() as i64; // classifier_status TEXT
            size += classifier.page_count.map_or(0, |_| 8);       // nullable i64
            size += classifier.result_examined_pages.map_or(0, |_| 8); // nullable i64
            size += 8; // classifier_nominal_charged_pages (i64, NOT NULL)
            size += classifier.classifier_build.len() as i64;
            size += classifier.classifier_profile_hash.len() as i64;
        }
    }
    for row in decision_rows {
        size += 8; // artifact_id (i64)
        size += row.file_identity.len() as i64;
        size += row.relative_path.len() as i64;
        size += inventory::enum_text(&row.action).len() as i64; // action TEXT
        size += row.reason.len() as i64;
        size += 8; // priority (i64)
        size += 8; // input_chars (i64)
        size += 8; // output_chars (i64)
        size += 8; // truncated (i64)
        size += row.error_code.len() as i64;
    }
    size
}

fn semantic_summary_json_for(draft: &ArtifactDraft) -> Result<String, StoreError> {
    serde_json::to_string(&draft.semantic_summary)
        .map_err(|error| StoreError::InvalidRequest(error.to_string()))
}

/// Persists an `ArtifactDraft` (parent + owned file/decision rows) and returns
/// the new `artifact_id` (spec Part 5.1). Eligible artifacts carry the snapshot
/// key fields plus per-source-file rows; ineligible payload artifacts carry
/// neither.
fn insert_artifact(
    transaction: &rusqlite::Transaction<'_>,
    draft: &ArtifactDraft,
    snapshot_key: Option<&SnapshotKeyParts>,
    now_ms: i64,
) -> Result<i64, StoreError> {
    let semantic_summary_json = serde_json::to_string(&draft.semantic_summary)
        .map_err(|error| StoreError::InvalidRequest(error.to_string()))?;
    let artifact_size = artifact_size_bytes(
        &draft.final_context,
        &draft.context_sha256,
        &semantic_summary_json,
        snapshot_key,
        &draft.file_rows,
        &draft.decision_rows,
    );
    let (key_sha256, key_json) = match snapshot_key {
        Some(key) => (Some(key.sha256.as_str()), Some(key.canonical_json.as_str())),
        None => (None, None),
    };
    let bucket = cache::date_bucket_for_ms(now_ms);
    transaction
        .execute(
            "INSERT INTO context_artifacts(
                snapshot_eligible, snapshot_key_sha256, snapshot_key_json,
                final_context, context_sha256, semantic_summary_json,
                artifact_size_bytes, created_at_ms, last_accessed_bucket
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                i64::from(snapshot_key.is_some()),
                key_sha256,
                key_json,
                draft.final_context,
                draft.context_sha256,
                semantic_summary_json,
                artifact_size,
                now_ms,
                bucket,
            ],
        )
        .map_err(cache_write)?;
    let artifact_id = transaction.last_insert_rowid();
    if draft.snapshot_eligible {
        let mut file_stmt = transaction.prepare_cached(
            "INSERT INTO context_artifact_files(
                artifact_id, file_identity, relative_path, source_version,
                source_guard_kind, source_guard_sha256, parse_profile_hash, parse_status,
                parser_backend, worker_lane, truncated, content_sha256,
                classifier_status, classifier_page_count, classifier_result_examined_pages,
                classifier_nominal_charged_pages, classifier_build, classifier_profile_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )
        .map_err(cache_write)?;
        for row in &draft.file_rows {
            file_stmt
                .execute(params![
                    artifact_id,
                    row.file_identity,
                    row.relative_path,
                    row.legacy_source_version,
                    row.source_guard_kind,
                    row.source_guard_sha256,
                    row.parse_profile_hash,
                    inventory::parse_status_text(row.parse_status),
                    row.parser_backend,
                    row.worker_lane,
                    i64::from(row.truncated),
                    row.content_sha256,
                    row.classifier
                        .as_ref()
                        .map(|classifier| inventory::enum_text(&classifier.status)),
                    row.classifier
                        .as_ref()
                        .and_then(|classifier| classifier.page_count.map(|value| value as i64)),
                    row.classifier
                        .as_ref()
                        .and_then(|classifier| classifier.result_examined_pages.map(|value| value as i64)),
                    row.classifier
                        .as_ref()
                        .map(|classifier| classifier.nominal_charged_pages as i64),
                    row.classifier
                        .as_ref()
                        .map(|classifier| classifier.classifier_build.clone()),
                    row.classifier
                        .as_ref()
                        .map(|classifier| classifier.classifier_profile_hash.clone()),
                ])
                .map_err(cache_write)?;
        }
        let mut decision_stmt = transaction.prepare_cached(
            "INSERT INTO context_artifact_decisions(
                artifact_id, file_identity, relative_path, action, reason,
                priority, input_chars, output_chars, truncated, error_code
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .map_err(cache_write)?;
        for row in &draft.decision_rows {
            decision_stmt
                .execute(params![
                    artifact_id,
                    row.file_identity,
                    row.relative_path,
                    inventory::enum_text(&row.action),
                    row.reason,
                    row.priority as i64,
                    row.input_chars as i64,
                    row.output_chars as i64,
                    i64::from(row.truncated),
                    row.error_code,
                ])
                .map_err(cache_write)?;
        }
    }
    Ok(artifact_id)
}

/// Reconstructs an `ArtifactDraft` from the persisted artifact rows
/// (spec Part 5.1 replay direction). The context hash is re-verified.
fn load_artifact_from_connection(
    connection: &Connection,
    artifact_id: i64,
) -> Result<ArtifactDraft, StoreError> {
    let row: Option<(i64, String, String, String)> = connection
        .query_row(
            "SELECT snapshot_eligible, final_context, context_sha256, semantic_summary_json
             FROM context_artifacts WHERE artifact_id=?1",
            [artifact_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(cache_open)?;
    let Some((snapshot_eligible, final_context, context_sha256, semantic_summary_json)) = row else {
        return Err(StoreError::RunNotFound);
    };
    let semantic_summary: crate::artifact::SemanticSummary =
        serde_json::from_str(&semantic_summary_json).map_err(|error| {
            StoreError::RunCorrupt(format!("artifact semantic summary is invalid: {error}"))
        })?;

    let mut file_stmt = connection
        .prepare(
            "SELECT file_identity, relative_path, source_version,
                    source_guard_kind, source_guard_sha256, parse_profile_hash, parse_status,
                    parser_backend, worker_lane, truncated, content_sha256,
                    classifier_status, classifier_page_count, classifier_result_examined_pages,
                    classifier_nominal_charged_pages, classifier_build, classifier_profile_hash
             FROM context_artifact_files WHERE artifact_id=?1 ORDER BY file_identity",
        )
        .map_err(cache_open)?;
    let file_query = file_stmt
        .query_map([artifact_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)? != 0,
                row.get::<_, String>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<i64>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<String>>(16)?,
            ))
        })
        .map_err(cache_open)?;
    let mut file_rows = Vec::new();
    for row in file_query {
        let (
            file_identity,
            relative_path,
            source_version,
            source_guard_kind,
            source_guard_sha256,
            parse_profile_hash,
            parse_status_text,
            parser_backend,
            worker_lane,
            truncated,
            content_sha256,
            classifier_status,
            classifier_page_count,
            classifier_examined,
            classifier_nominal,
            classifier_build,
            classifier_profile_hash,
        ) = row.map_err(cache_open)?;
        let parse_status: ParseStatus = parse_contract_enum(&parse_status_text)?;
        let classifier = match (classifier_status, classifier_build, classifier_profile_hash) {
            (Some(status_text), Some(build), Some(profile_hash)) => {
                let status = parse_contract_enum(&status_text)?;
                Some(crate::artifact::PdfClassificationProvenanceV1 {
                    status,
                    page_count: classifier_page_count.map(|value| value as u64),
                    result_examined_pages: classifier_examined.map(|value| value as u64),
                    nominal_charged_pages: classifier_nominal.unwrap_or(0) as u64,
                    classifier_build: build,
                    classifier_profile_hash: profile_hash,
                })
            }
            _ => None,
        };
        file_rows.push(ArtifactFileRow {
            file_identity,
            relative_path,
            legacy_source_version: source_version,
            source_guard_kind,
            source_guard_sha256,
            parse_profile_hash,
            parse_status,
            parser_backend,
            worker_lane,
            truncated,
            content_sha256,
            classifier,
        });
    }

    let mut decision_stmt = connection
        .prepare(
            "SELECT file_identity, relative_path, action, reason, priority,
                    input_chars, output_chars, truncated, error_code
             FROM context_artifact_decisions WHERE artifact_id=?1 ORDER BY file_identity",
        )
        .map_err(cache_open)?;
    let decision_query = decision_stmt
        .query_map([artifact_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)? != 0,
                row.get::<_, String>(8)?,
            ))
        })
        .map_err(cache_open)?;
    let mut decision_rows = Vec::new();
    for row in decision_query {
        let (
            file_identity,
            relative_path,
            action_text,
            reason,
            priority,
            input_chars,
            output_chars,
            truncated,
            error_code,
        ) = row.map_err(cache_open)?;
        let action: ContextAction = parse_contract_enum(&action_text)?;
        decision_rows.push(crate::artifact::ArtifactDecisionRow {
            file_identity,
            relative_path,
            action,
            reason,
            priority: priority as u64,
            input_chars: input_chars as u64,
            output_chars: output_chars as u64,
            truncated,
            error_code,
        });
    }

    if crate::artifact::sha256_hex(final_context.as_bytes()) != context_sha256 {
        return Err(StoreError::RunCorrupt(
            "artifact context hash is invalid".to_string(),
        ));
    }
    Ok(ArtifactDraft {
        snapshot_eligible: snapshot_eligible != 0,
        final_context,
        context_sha256,
        semantic_summary,
        file_rows,
        decision_rows,
    })
}

/// spec Part 5.1 eligible dedup: a recompute whose snapshot key already exists
/// reuses the stored artifact ONLY when `snapshot_key_json` is byte-exact and
/// the artifact is field-for-field identical; any difference is a non-retryable
/// `BUDGET_MODEL_MISMATCH`/store invariant (never overwrites the old artifact).
fn dedup_artifact(
    transaction: &rusqlite::Transaction<'_>,
    draft: &ArtifactDraft,
    key: &SnapshotKeyParts,
) -> Result<Option<i64>, StoreError> {
    let existing_id: Option<i64> = transaction
        .query_row(
            "SELECT artifact_id FROM context_artifacts
             WHERE snapshot_eligible = 1 AND snapshot_key_sha256 = ?1
             LIMIT 1",
            [&key.sha256],
            |row| row.get(0),
        )
        .optional()
        .map_err(cache_write)?;
    let Some(existing_id) = existing_id else {
        return Ok(None);
    };
    let existing = load_artifact_from_connection(&*transaction, existing_id)?;
    let existing_key_json: Option<String> = transaction
        .query_row(
            "SELECT snapshot_key_json FROM context_artifacts WHERE artifact_id=?1",
            [existing_id],
            |row| row.get(0),
        )
        .map_err(cache_write)?;
    if existing_key_json.as_deref() == Some(key.canonical_json.as_str()) && existing == *draft {
        Ok(Some(existing_id))
    } else {
        Err(StoreError::ArtifactMismatch(format!(
            "snapshot key {} already exists with different artifact semantics",
            key.sha256
        )))
    }
}

/// spec Part 4: 512 MiB context-artifact cap enforcement inside the terminal
/// transaction. Deletes retention-allowed orphans first (zero `context_runs`
/// references, not in the protected set), then the oldest terminal runs
/// (cascading their context_runs so their artifacts become orphans for the next
/// orphan sweep). If the current artifact itself exceeds the cap or no rows are
/// deletable (pinned references), finalization fails closed.
fn make_room_for_artifact(
    transaction: &rusqlite::Transaction<'_>,
    protected_artifacts: &HashSet<i64>,
    new_size: i64,
) -> Result<(), StoreError> {
    if new_size > cache::CONTEXT_ARTIFACTS_MAX_BYTES {
        return Err(StoreError::RunCorrupt(
            "current artifact exceeds the 512 MiB context artifact cap".to_string(),
        ));
    }
    let total_artifacts = || -> Result<i64, StoreError> {
        transaction
            .query_row(
                "SELECT COALESCE(SUM(artifact_size_bytes), 0) FROM context_artifacts",
                [],
                |row| row.get(0),
            )
            .map_err(cache_write)
    };
    let mut current = total_artifacts()?;
    if current.saturating_add(new_size) <= cache::CONTEXT_ARTIFACTS_MAX_BYTES {
        return Ok(());
    }
    loop {
        // 1) delete orphan artifacts (zero context_runs references, not protected).
        let clause = not_in_clause(protected_artifacts);
        let sql = format!(
            "DELETE FROM context_artifacts WHERE artifact_id IN (
                SELECT artifact_id FROM context_artifacts
                WHERE NOT EXISTS(
                    SELECT 1 FROM context_runs WHERE context_runs.artifact_id = context_artifacts.artifact_id
                )
                  {clause}
                ORDER BY created_at_ms ASC, artifact_id ASC
                LIMIT 64
             )"
        );
        let deleted = transaction
            .execute(&sql, rusqlite::params_from_iter(protected_artifacts.iter()))
            .map_err(cache_write)?;
        if deleted > 0 {
            current = total_artifacts()?;
            if current.saturating_add(new_size) <= cache::CONTEXT_ARTIFACTS_MAX_BYTES {
                return Ok(());
            }
            continue;
        }
        // 2) delete the oldest terminal runs. Their context_runs rows cascade
        //    away (the referenced artifacts become orphans and are reclaimed by
        //    the next orphan sweep). The current run is still 'running' and the
        //    snapshot-hit source run is protected by the caller (make_room is
        //    only reached on the new-artifact path).
        let deleted = transaction
            .execute(
                "DELETE FROM scan_runs WHERE scan_run_id IN (
                    SELECT scan_run_id FROM scan_runs
                    WHERE status IN ('success', 'partial', 'error', 'abandoned')
                    ORDER BY finished_at_ms ASC, scan_run_id ASC
                    LIMIT 64
                 )",
                [],
            )
            .map_err(cache_write)?;
        if deleted > 0 {
            current = total_artifacts()?;
            if current.saturating_add(new_size) <= cache::CONTEXT_ARTIFACTS_MAX_BYTES {
                return Ok(());
            }
            continue;
        }
        // 3) no deletable rows → pinned references → fail closed.
        return Err(StoreError::RunCorrupt(
            "context artifact retention could not make room for the current artifact".to_string(),
        ));
    }
}

fn not_in_clause(ids: &HashSet<i64>) -> String {
    if ids.is_empty() {
        return String::new();
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    format!(" AND artifact_id NOT IN ({placeholders})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::{ClassificationError, ParserRoute};
    use crate::config::{normalize_scanner_profile, normalize_scanner_profile_for_request};
    use crate::planner::{PlanAction, PlannedFile};
    use ai_daily_discovery::DiscoveredFileOut;
    use ai_daily_scanner_contract::{
        AdapterPaths, AuditWorkerLane, CacheMissReason, CacheStatus, ContextAction,
        ClassificationCacheStatus, ClassificationTransport, ContextDecision, DiagnosticStage,
        ParseStatus, ParseTransport, PdfClassificationAuditV1, PdfClassificationStatus,
        RawScannerProfileV1, ReportMode,
    };
    use tempfile::TempDir;

    struct Harness {
        _directory: TempDir,
        store: ScannerStore,
        request: BuildContextRequest,
        profile: NormalizedScannerProfileV1,
        canonical: CanonicalRequest,
        runtime: AttemptRuntime,
        db_path: PathBuf,
    }

    fn harness(request_id: &str) -> Harness {
        let directory = tempfile::tempdir().expect("temporary directory");
        let db_path = directory.path().join(SCAN_DB_FILENAME);
        let mut request: BuildContextRequest = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/scanner_contract/v1/request.json"
        ))
        .expect("request fixture");
        request.request_id = request_id.to_string();
        request.work_dir = directory.path().to_string_lossy().to_string();
        request.scan_db_path = db_path.to_string_lossy().to_string();
        request.adapters = AdapterPaths {
            office_worker_path: directory
                .path()
                .join("office-worker.exe")
                .to_string_lossy()
                .to_string(),
            python_executable: directory
                .path()
                .join("python.exe")
                .to_string_lossy()
                .to_string(),
            python_module_root: directory.path().to_string_lossy().to_string(),
            python_document_worker_module: "src.workers.document_parser_worker".to_string(),
        };
        let profile = normalize_scanner_profile_for_request(&request.scanner_profile, request.report_mode)
            .expect("normalized scanner profile");
        let canonical =
            ScannerStore::canonicalize_request(&request, &profile).expect("canonical request");
        let runtime = AttemptRuntime::from_request(&request, &crate::version_response()).unwrap();
        let store = ScannerStore::open(&db_path).expect("scanner store");
        Harness {
            _directory: directory,
            store,
            request,
            profile,
            canonical,
            runtime,
            db_path,
        }
    }

    fn started(outcome: BeginRunOutcome) -> ActiveRun {
        match outcome {
            BeginRunOutcome::Started(active) => active,
            BeginRunOutcome::Stored(_) => panic!("expected a new active run"),
        }
    }

    fn canonical_for_request_id(harness: &Harness, request_id: &str) -> CanonicalRequest {
        let mut request = harness.request.clone();
        request.request_id = request_id.to_string();
        ScannerStore::canonicalize_request(&request, &harness.profile).unwrap()
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

    fn run_error() -> Diagnostic {
        Diagnostic {
            error_code: ErrorCode::WorkerHandshakeFailed,
            message: "worker handshake failed".to_string(),
            retryable: false,
            stage: DiagnosticStage::Process,
            file_path: Nullable(None),
            backend: Nullable(None),
        }
    }

    fn error_batch(active: &ActiveRun) -> FinalizationBatch {
        let error = run_error();
        let version = crate::version_response();
        let envelope = ContextEnvelope {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: active.request_id().to_string(),
            engine_version: version.engine_version,
            engine_build: version.engine_build,
            status: EngineStatus::Error,
            file_context: String::new(),
            summary: empty_summary(),
            scan_run_id: Nullable(Some(active.scan_run_id())),
            context_run_id: Nullable(None),
            warnings: Vec::new(),
            error: Nullable(Some(error.clone())),
        };
        FinalizationBatch {
            status: RunStatus::Error,
            envelope_json: canonical_envelope_json(&envelope).expect("canonical envelope"),
            inventory: Vec::new(),
            cache_writes: Vec::new(),
            file_results: Vec::new(),
            diagnostics: vec![RunDiagnosticRecord {
                severity: DiagnosticSeverity::Error,
                diagnostic: error,
            }],
            stage_metrics: Vec::new(),
            extension_metrics: Vec::new(),
            context: None,
            artifact: None,
            snapshot_key: None,
            snapshot_hit: None,
            execution_metrics: None,
        }
    }

    fn inventory_record(identity: &str, source_version: &str) -> InventoryRecord {
        let (mtime_ns, size_bytes) = inventory::parse_source_version(source_version).unwrap();
        InventoryRecord {
            file_identity: identity.to_string(),
            absolute_path: "C:\\fixture\\evidence.txt".to_string(),
            relative_path: "evidence.txt".to_string(),
            file_type: ".txt".to_string(),
            source_version: source_version.to_string(),
            size_bytes,
            mtime_ns,
            source_guard_kind: None,
            source_guard_sha256: None,
        }
    }

    fn success_file_result(
        identity: &str,
        source_version: &str,
        profile_hash: &str,
        content: &str,
    ) -> FileResultRecord {
        FileResultRecord {
            file_identity: identity.to_string(),
            relative_path: "evidence.txt".to_string(),
            source_version: source_version.to_string(),
            parse_profile_hash: profile_hash.to_string(),
            cache_status: CacheStatus::Miss,
            cache_miss_reason: CacheMissReason::NewFile,
            parse_status: ParseStatus::Success,
            parser_backend: "light_text_v1".to_string(),
            worker_lane: AuditWorkerLane::RustCore,
            truncated: false,
            content_sha256: sha256_hex(content.as_bytes()),
            primary_duration_ms: 3,
            fallback_duration_ms: 0,
            parse_duration_ms: 3,
            failure_class: String::new(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ParseTransport::RustInProcess,
            parse_attempt_count: 1,
            pdf_classification: None,
            error: None,
        }
    }

    #[test]
    fn file_result_validation_rejects_impossible_execution_provenance() {
        let source = "mtime_ns=100:size=5";
        let profile_hash = "a".repeat(64);

        let mut fresh_with_execution =
            success_file_result("file-a", source, &profile_hash, "hello");
        fresh_with_execution.cache_status = CacheStatus::Fresh;
        fresh_with_execution.cache_miss_reason = CacheMissReason::None;
        fresh_with_execution.parse_transport = ParseTransport::OneShot;
        assert!(fresh_with_execution.validate().is_err());

        let mut miss_without_execution =
            success_file_result("file-a", source, &profile_hash, "hello");
        miss_without_execution.parse_transport = ParseTransport::NotApplicable;
        miss_without_execution.parse_attempt_count = 0;
        miss_without_execution.primary_duration_ms = 0;
        miss_without_execution.parse_duration_ms = 0;
        assert!(miss_without_execution.validate().is_err());
    }

    #[test]
    fn audit_size_counts_the_execution_child_payload() {
        let source = "mtime_ns=100:size=5";
        let profile_hash = "a".repeat(64);
        let mut batch = error_batch(&ActiveRun {
            scan_run_id: 1,
            attempt_number: 1,
            owner_id: "owner".to_string(),
            request_id: "00000000-0000-4000-8000-000000000099".to_string(),
        });
        batch.file_results = vec![success_file_result(
            "file-a",
            source,
            &profile_hash,
            "hello",
        )];
        let parse_only_size = compute_audit_size(&batch);
        batch.file_results[0].pdf_classification = Some(PdfClassificationAuditV1 {
            status: PdfClassificationStatus::TextInParseWindow,
            page_count: Nullable(Some(2)),
            classification_cache_status: ClassificationCacheStatus::Miss,
            classification_cache_miss_reason: "classifier_identity_changed".to_string(),
            result_examined_pages: Nullable(Some(1)),
            run_inspected_pages: Nullable(Some(1)),
            nominal_charged_pages: 2,
            duration_ms: 3,
            transport: ClassificationTransport::Session,
            attempt_count: 1,
            classifier_build: "b".repeat(64),
            classifier_profile_hash: "c".repeat(64),
        });

        assert!(
            compute_audit_size(&batch) > parse_only_size,
            "classification execution child fields must contribute to audit_size_bytes"
        );
    }

    fn cache_record(
        identity: &str,
        source_version: &str,
        profile_hash: &str,
        content: &str,
    ) -> CacheWriteRecord {
        let version = crate::version_response();
        CacheWriteRecord {
            file_identity: identity.to_string(),
            source_version: source_version.to_string(),
            source_guard_kind: "content_sha256_v1".to_string(),
            source_guard_sha256: "0".repeat(64),
            parse_profile_hash: profile_hash.to_string(),
            content: content.to_string(),
            content_sha256: sha256_hex(content.as_bytes()),
            parser_backend: "light_text_v1".to_string(),
            worker_lane: "rust_core".to_string(),
            truncated: false,
            worker_contract_version: "ai_daily_context_v1".to_string(),
            worker_version: version.engine_version,
            worker_build: version.engine_build,
        }
    }

    fn record_both_workers(store: &mut ScannerStore, active: &ActiveRun, now_ms: u64) {
        let office = WorkerFingerprint {
            contract: "ai_daily_worker_v1".to_string(),
            version: "0.1.0".to_string(),
            build: "office-build".to_string(),
        };
        let python = WorkerFingerprint {
            contract: "ai_daily_worker_v1".to_string(),
            version: "0.1.0".to_string(),
            build: "python-build".to_string(),
        };
        store
            .record_worker_fingerprints(active, Some(&office), Some(&python), now_ms)
            .unwrap();
    }

    fn success_batch(
        active: &ActiveRun,
        identity: &str,
        source_version: &str,
        profile_hash: &str,
        content: &str,
    ) -> FinalizationBatch {
        let version = crate::version_response();
        let summary = ContextSummary {
            source_file_count: 1,
            success_count: 1,
            timeout_count: 0,
            included_file_count: 1,
            omitted_file_count: 0,
            error_file_count: 0,
            input_chars: content.chars().count() as u64,
            output_chars: content.chars().count() as u64,
            total_duration_ms: 8,
            discovery_duration_ms: 2,
            parse_duration_ms: 3,
            compression_duration_ms: 1,
        };
        let envelope = ContextEnvelope {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: active.request_id().to_string(),
            engine_version: version.engine_version,
            engine_build: version.engine_build,
            status: EngineStatus::Ok,
            file_context: content.to_string(),
            summary: summary.clone(),
            scan_run_id: Nullable(Some(active.scan_run_id())),
            context_run_id: Nullable(Some(active.context_run_id())),
            warnings: Vec::new(),
            error: Nullable(None),
        };
        let artifact = crate::artifact::ArtifactDraft::new(
            false,
            content.to_string(),
            crate::artifact::SemanticSummary {
                source_file_count: 1,
                success_count: 1,
                timeout_count: 0,
                included_file_count: 1,
                omitted_file_count: 0,
                error_file_count: 0,
                input_chars: content.chars().count() as u64,
                output_chars: content.chars().count() as u64,
                reserved_chars: content.chars().count() as u64,
                rendered_chars: content.chars().count() as u64,
            },
            Vec::new(),
            Vec::new(),
        )
        .expect("ineligible payload artifact");
        FinalizationBatch {
            status: RunStatus::Success,
            envelope_json: canonical_envelope_json(&envelope).expect("canonical envelope"),
            inventory: vec![inventory_record(identity, source_version)],
            cache_writes: vec![cache_record(
                identity,
                source_version,
                profile_hash,
                content,
            )],
            file_results: vec![success_file_result(
                identity,
                source_version,
                profile_hash,
                content,
            )],
            diagnostics: Vec::new(),
            stage_metrics: vec![
                StageMetric {
                    stage: StageName::Discovery,
                    item_count: 1,
                    duration_ms: 2,
                },
                StageMetric {
                    stage: StageName::Cache,
                    item_count: 1,
                    duration_ms: 1,
                },
                StageMetric {
                    stage: StageName::Parse,
                    item_count: 1,
                    duration_ms: 3,
                },
                StageMetric {
                    stage: StageName::Context,
                    item_count: 1,
                    duration_ms: 1,
                },
            ],
            extension_metrics: vec![ExtensionMetric {
                extension: ".txt".to_string(),
                file_count: 1,
                parse_duration_ms: 3,
                success_count: 1,
                error_count: 0,
                timeout_count: 0,
            }],
            context: Some(ContextRunRecord {
                context_profile_hash: "b".repeat(64),
                status: RunStatus::Success,
                final_context: content.to_string(),
                context_sha256: sha256_hex(content.as_bytes()),
                summary,
                decisions: vec![ContextDecisionRecord {
                    file_identity: identity.to_string(),
                    decision: ContextDecision {
                        relative_path: "evidence.txt".to_string(),
                        action: ContextAction::Keep,
                        reason: "fits budgets".to_string(),
                        priority: 1,
                        input_chars: content.chars().count() as u64,
                        output_chars: content.chars().count() as u64,
                        truncated: false,
                        error_code: String::new(),
                    },
                }],
            }),
            artifact: Some(artifact),
            snapshot_key: None,
            snapshot_hit: None,
            execution_metrics: None,
        }
    }

    #[test]
    fn canonical_request_excludes_ids_runtime_paths_and_builds() {
        let first = harness("00000000-0000-4000-8000-000000000001");
        let mut changed = first.request.clone();
        changed.request_id = "00000000-0000-4000-8000-000000000002".to_string();
        changed.scan_db_path = first
            ._directory
            .path()
            .join("other.sqlite3")
            .to_string_lossy()
            .to_string();
        changed.adapters.office_worker_path = first
            ._directory
            .path()
            .join("changed-worker.exe")
            .to_string_lossy()
            .to_string();
        let equivalent = ScannerStore::canonicalize_request(&changed, &first.profile).unwrap();

        assert_eq!(equivalent.json, first.canonical.json);
        assert_eq!(equivalent.hash, first.canonical.hash);
        assert!(!first.canonical.json.contains("request_id"));
        assert!(!first.canonical.json.contains("scan_db_path"));
        assert!(!first.canonical.json.contains("adapters"));
        assert!(first.canonical.json.contains("context_profile"));
    }

    #[test]
    fn worker_count_does_not_change_parse_profile_hash() {
        let harness = harness("00000000-0000-4000-8000-000000000003");
        let route = RouteStackFingerprint::text("build-a").unwrap();
        let original = parse_profile_hash(1, &route, &harness.profile).unwrap();
        let mut changed = harness.profile.clone();
        changed.execution.max_workers = 1;
        let changed = parse_profile_hash(1, &route, &changed).unwrap();
        assert_eq!(original, changed);
    }

    #[test]
    fn cache_aware_plan_consumes_pure_planner_output_without_reclassification() {
        let harness = harness("00000000-0000-4000-8000-000000000019");
        let file = DiscoveredFileOut {
            file_identity: "file-a".to_string(),
            path: "C:\\fixture\\evidence.txt".to_string(),
            extension: ".txt".to_string(),
            modified_at: "2026-07-16T00:00:00+08:00".to_string(),
            size_bytes: 5,
            source_version: "mtime_ns=100:size=5".to_string(),
            source_guard_kind: None,
            source_guard_sha256: None,
        };
        let stacks = RouteStackFingerprints {
            text_like: RouteStackFingerprint::text("engine-build").unwrap(),
            modern_office: RouteStackFingerprint::modern_office(
                "engine-build",
                "ai_daily_worker_v1",
                "office-build",
                Some(("ai_daily_worker_v1", "python-build")),
            )
            .unwrap(),
            python_document: RouteStackFingerprint::python_document(
                "engine-build",
                "ai_daily_worker_v1",
                "python-build",
            )
            .unwrap(),
        };
        let entries = harness
            .store
            .attach_cache_evidence(
                vec![
                    PlannedFile {
                        file: file.clone(),
                        action: PlanAction::Parse(ParserRoute::LightText),
                        timeout_ms: 30_000,
                    },
                    PlannedFile {
                        file,
                        action: PlanAction::Reject(ClassificationError::FileTooLarge),
                        timeout_ms: 30_000,
                    },
                ],
                &harness.profile,
                &stacks,
            )
            .unwrap();

        assert!(entries[0].parse_profile_hash.is_some());
        assert_eq!(
            entries[0].cache_lookup,
            Some(CacheLookup::Miss(CacheMissReason::NewFile))
        );
        assert_eq!(
            entries[1].planned.action,
            PlanAction::Reject(ClassificationError::FileTooLarge)
        );
        assert!(entries[1].parse_profile_hash.is_none());
        assert!(entries[1].cache_lookup.is_none());
    }

    #[test]
    fn live_lease_returns_structured_same_and_different_request_errors() {
        let mut first = harness("00000000-0000-4000-8000-000000000004");
        let active = started(
            first
                .store
                .begin_run(
                    &first.request.request_id,
                    &first.canonical,
                    &first.runtime,
                    100_000,
                )
                .unwrap(),
        );
        let mut second = ScannerStore::open(&first.db_path).unwrap();

        assert_eq!(
            second
                .begin_run(
                    &first.request.request_id,
                    &first.canonical,
                    &first.runtime,
                    100_001
                )
                .unwrap_err(),
            StoreError::RequestInProgress
        );
        assert_eq!(
            second
                .begin_run(
                    "00000000-0000-4000-8000-000000000005",
                    &canonical_for_request_id(&first, "00000000-0000-4000-8000-000000000005",),
                    &first.runtime,
                    100_001,
                )
                .unwrap_err(),
            StoreError::ScanAlreadyRunning
        );
        let attempts: i64 = first
            .store
            .connection
            .query_row(
                "SELECT count(*) FROM scan_run_attempts WHERE scan_run_id=?1",
                [active.scan_run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 1);
    }

    #[test]
    fn heartbeat_prevents_reclaim_but_expired_lease_is_recovered() {
        let mut first = harness("00000000-0000-4000-8000-000000000006");
        let active = started(
            first
                .store
                .begin_run(
                    &first.request.request_id,
                    &first.canonical,
                    &first.runtime,
                    100_000,
                )
                .unwrap(),
        );
        first.store.heartbeat(&active, 115_000).unwrap();
        let mut second = ScannerStore::open(&first.db_path).unwrap();
        assert_eq!(
            second
                .begin_run(
                    "00000000-0000-4000-8000-000000000007",
                    &canonical_for_request_id(&first, "00000000-0000-4000-8000-000000000007",),
                    &first.runtime,
                    130_000,
                )
                .unwrap_err(),
            StoreError::ScanAlreadyRunning
        );

        let recovered = started(
            second
                .begin_run(
                    "00000000-0000-4000-8000-000000000007",
                    &canonical_for_request_id(&first, "00000000-0000-4000-8000-000000000007"),
                    &first.runtime,
                    135_001,
                )
                .unwrap(),
        );
        assert_ne!(recovered.scan_run_id(), active.scan_run_id());
        let first_status: String = second
            .connection
            .query_row(
                "SELECT status FROM scan_runs WHERE scan_run_id=?1",
                [active.scan_run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_status, "abandoned");
    }

    #[test]
    fn abandoned_same_request_reuses_row_appends_attempt_and_clears_lease_on_finish() {
        let mut first = harness("00000000-0000-4000-8000-000000000008");
        let active = started(
            first
                .store
                .begin_run(
                    &first.request.request_id,
                    &first.canonical,
                    &first.runtime,
                    1_000,
                )
                .unwrap(),
        );
        let mut recovered_store = ScannerStore::open(&first.db_path).unwrap();
        let recovered = started(
            recovered_store
                .begin_run(
                    &first.request.request_id,
                    &first.canonical,
                    &first.runtime,
                    1_000 + LEASE_GRACE_MS + 1,
                )
                .unwrap(),
        );
        assert_eq!(recovered.scan_run_id(), active.scan_run_id());
        assert_eq!(recovered.attempt_number(), 2);
        let batch = error_batch(&recovered);
        recovered_store
            .finalize(&recovered, &batch, 30_000)
            .unwrap();
        let lease_count: i64 = recovered_store
            .connection
            .query_row("SELECT count(*) FROM engine_lease", [], |row| row.get(0))
            .unwrap();
        assert_eq!(lease_count, 0);
    }

    #[test]
    fn expired_owner_cannot_revive_its_own_stale_lease() {
        let mut harness = harness("00000000-0000-4000-8000-000000000018");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1_000,
                )
                .unwrap(),
        );

        assert_eq!(
            harness
                .store
                .heartbeat(&active, 1_000 + LEASE_GRACE_MS + 1)
                .unwrap_err(),
            StoreError::LeaseLost
        );
    }

    #[test]
    fn terminal_retry_is_byte_identical_after_runtime_build_changes_and_conflicts_reject() {
        let mut harness = harness("00000000-0000-4000-8000-000000000009");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        let batch = error_batch(&active);
        let expected = batch.envelope_json.clone();
        harness.store.finalize(&active, &batch, 2).unwrap();

        let mut changed_version = crate::version_response();
        changed_version.engine_build = "engine-build-that-did-not-exist-on-first-run".to_string();
        let changed_runtime =
            AttemptRuntime::from_request(&harness.request, &changed_version).unwrap();
        let stored = harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &changed_runtime,
                3,
            )
            .unwrap();
        let BeginRunOutcome::Stored(stored) = stored else {
            panic!("terminal retry must return stored bytes")
        };
        assert_eq!(stored.envelope_json.as_bytes(), expected.as_bytes());

        let mut changed_request = harness.request.clone();
        changed_request.end_date = "2099-12-31".to_string();
        let changed_profile = normalize_scanner_profile_for_request(
            &changed_request.scanner_profile,
            changed_request.report_mode,
        )
        .unwrap();
        let conflicting =
            ScannerStore::canonicalize_request(&changed_request, &changed_profile).unwrap();
        assert_eq!(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &conflicting,
                    &changed_runtime,
                    4,
                )
                .unwrap_err(),
            StoreError::RequestIdConflict
        );
    }

    #[test]
    fn replay_rebuilds_success_envelope_from_metadata_and_artifact() {
        // spec Part 5.1: final_context is stored ONCE in the artifact; the
        // persisted scan_runs JSON is body-free, and idempotent replay rebuilds
        // the full ContextEnvelope from metadata + summary + artifact.
        let mut harness = harness("00000000-0000-4000-8000-000000000023");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        let batch = success_batch(
            &active,
            "file-a",
            "mtime_ns=100:size=5",
            &"a".repeat(64),
            "hello",
        );
        let expected = batch.envelope_json.clone();
        assert!(expected.contains("hello"), "original envelope carries the body");
        harness.store.finalize(&active, &batch, 2).unwrap();

        let stored_json: String = harness
            .store
            .connection
            .query_row(
                "SELECT final_envelope_json FROM scan_runs WHERE scan_run_id=?1",
                [active.scan_run_id() as i64],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !stored_json.contains("hello"),
            "scan_runs JSON must not duplicate the body"
        );
        let value: serde_json::Value = serde_json::from_str(&stored_json).unwrap();
        assert!(
            value.get("file_context").is_none(),
            "body-free metadata must not carry file_context"
        );

        // Idempotent replay rebuilds the full envelope byte-for-byte.
        let stored = harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                3,
            )
            .unwrap();
        let BeginRunOutcome::Stored(stored) = stored else {
            panic!("terminal retry must return stored bytes")
        };
        assert_eq!(
            stored.envelope_json.as_bytes(),
            expected.as_bytes(),
            "rebuilt envelope must match the original byte-for-byte"
        );
        assert!(
            stored.envelope.file_context.contains("hello"),
            "rebuilt envelope must recover the body from the artifact"
        );
    }

    #[test]
    fn error_run_with_file_rows_inspects_cleanly_v1_and_v2() {
        // spec Part 2.3: a committed Error run with file rows must carry a
        // context_runs row (artifact_id=NULL) whose summary reconciles with the
        // persisted file/decision/stage rows, so v1 AND v2 inspect both succeed
        // (previously: "error envelope has unexpected context rows" RUN_CORRUPT).
        let mut harness = harness("00000000-0000-4000-8000-000000000024");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        let source = "mtime_ns=100:size=5";
        let profile_hash = "a".repeat(64);
        let file_error = Diagnostic {
            error_code: ErrorCode::ParserFailed,
            message: "parse failed".to_string(),
            retryable: false,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some("C:\\fixture\\evidence.txt".to_string())),
            backend: Nullable(Some("light_text_v1".to_string())),
        };
        let summary = ContextSummary {
            source_file_count: 1,
            success_count: 0,
            timeout_count: 0,
            included_file_count: 0,
            omitted_file_count: 0,
            error_file_count: 1,
            input_chars: 5,
            output_chars: 0,
            total_duration_ms: 3,
            discovery_duration_ms: 2,
            parse_duration_ms: 1,
            compression_duration_ms: 0,
        };
        let context = ContextRunRecord {
            context_profile_hash: "0".repeat(64),
            status: RunStatus::Error,
            final_context: String::new(),
            context_sha256: sha256_hex(b""),
            summary: summary.clone(),
            decisions: vec![ContextDecisionRecord {
                file_identity: "file-a".to_string(),
                decision: ContextDecision {
                    relative_path: "evidence.txt".to_string(),
                    action: ContextAction::Error,
                    reason: "parse_error".to_string(),
                    priority: 0,
                    input_chars: 5,
                    output_chars: 0,
                    truncated: false,
                    error_code: "PARSER_FAILED".to_string(),
                },
            }],
        };
        let version = crate::version_response();
        let envelope = ContextEnvelope {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: active.request_id().to_string(),
            engine_version: version.engine_version,
            engine_build: version.engine_build,
            status: EngineStatus::Error,
            file_context: String::new(),
            summary: summary.clone(),
            scan_run_id: Nullable(Some(active.scan_run_id())),
            context_run_id: Nullable(Some(active.context_run_id())),
            warnings: Vec::new(),
            error: Nullable(Some(file_error.clone())),
        };
        let mut inventory = inventory_record("file-a", source);
        inventory.source_guard_kind = Some("content_sha256_v1".to_string());
        inventory.source_guard_sha256 = Some("a".repeat(64));
        let batch = FinalizationBatch {
            status: RunStatus::Error,
            envelope_json: canonical_envelope_json(&envelope).expect("canonical envelope"),
            inventory: vec![inventory],
            cache_writes: Vec::new(),
            file_results: vec![FileResultRecord {
                file_identity: "file-a".to_string(),
                relative_path: "evidence.txt".to_string(),
                source_version: source.to_string(),
                parse_profile_hash: profile_hash,
                cache_status: CacheStatus::Miss,
                cache_miss_reason: CacheMissReason::NewFile,
                parse_status: ParseStatus::Error,
                parser_backend: "light_text_v1".to_string(),
                worker_lane: AuditWorkerLane::RustCore,
                truncated: false,
                content_sha256: sha256_hex(b""),
                primary_duration_ms: 1,
                fallback_duration_ms: 0,
                parse_duration_ms: 1,
                failure_class: "parser_failed".to_string(),
                fallback_backend: String::new(),
                fallback_reason_code: String::new(),
                parse_transport: ParseTransport::RustInProcess,
                parse_attempt_count: 1,
                pdf_classification: None,
                error: Some(file_error),
            }],
            diagnostics: vec![RunDiagnosticRecord {
                severity: DiagnosticSeverity::Error,
                diagnostic: Diagnostic {
                    error_code: ErrorCode::ParserFailed,
                    message: "parse failed".to_string(),
                    retryable: false,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(Some("C:\\fixture\\evidence.txt".to_string())),
                    backend: Nullable(Some("light_text_v1".to_string())),
                },
            }],
            stage_metrics: vec![
                StageMetric { stage: StageName::Discovery, item_count: 1, duration_ms: 2 },
                StageMetric { stage: StageName::Cache, item_count: 1, duration_ms: 0 },
                StageMetric { stage: StageName::Parse, item_count: 1, duration_ms: 1 },
                StageMetric { stage: StageName::Context, item_count: 1, duration_ms: 0 },
            ],
            extension_metrics: vec![ExtensionMetric {
                extension: ".txt".to_string(),
                file_count: 1,
                parse_duration_ms: 1,
                success_count: 0,
                error_count: 1,
                timeout_count: 0,
            }],
            context: Some(context),
            artifact: None,
            snapshot_key: None,
            snapshot_hit: None,
            execution_metrics: Some(ai_daily_scanner_contract::ExecutionMetricsV2 {
                discovery_observed_file_count: 1,
                source_guard_content_hash_file_count: 0,
                source_guard_unavailable_count: 0,
                source_guard_bytes_read: 0,
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
                worker_handshake_ms: 5,
                discovery_ms: 2,
                snapshot_lookup_ms: 0,
                current_run_audit_write_ms: 0,
                terminal_precommit_ms: 0,
                deadline_precommit_elapsed_ms: 0,
                envelope_rebuild_ms: 0,
                terminal_rows_written: 0,
                peak_worker_rss_bytes: Nullable(None),
            }),
        };
        harness.store.finalize(&active, &batch, 2).expect("error finalize");

        // The context_runs row must exist with artifact_id=NULL and status=error.
        let (context_run_id, status, artifact_id): (i64, String, Option<i64>) = harness
            .store
            .connection
            .query_row(
                "SELECT context_run_id, status, artifact_id
                 FROM context_runs WHERE scan_run_id=?1",
                [active.scan_run_id() as i64],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(context_run_id, active.context_run_id() as i64);
        assert_eq!(status, "error");
        assert!(artifact_id.is_none(), "error run must reference no artifact");

        // v1 inspect succeeds.
        let snapshot = harness
            .store
            .inspect_run(active.scan_run_id(), false)
            .expect("v1 inspect must succeed for an error run with file rows");
        assert_eq!(snapshot.run_status, RunStatus::Error);

        // v2 inspect succeeds (full_v2 provenance with a reconciling summary).
        let request = ai_daily_scanner_contract::InspectRunRequest {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: "00000000-0000-4000-8000-000000000024".to_string(),
            scan_db_path: harness.db_path.to_string_lossy().to_string(),
            scan_run_id: active.scan_run_id(),
            include_content: false,
        };
        let v2 = crate::inspect::assemble_inspect_v2(&request, &snapshot)
            .expect("v2 inspect must succeed for an error run with file rows");
        assert_eq!(v2.status, ai_daily_scanner_contract::InspectStatus::Ok);
        assert_eq!(v2.artifact_id.0, None, "error run has no artifact");
        assert_eq!(v2.execution_metrics.worker_handshake_ms, 5);
        assert_eq!(v2.execution_metrics.discovery_ms, 2);

        let rows_written: i64 = harness
            .store
            .connection
            .query_row(
                "SELECT terminal_rows_written FROM scan_execution_metrics WHERE scan_run_id=?1",
                [active.scan_run_id() as i64],
                |row| row.get(0),
            )
            .expect("terminal row count");
        assert_eq!(
            rows_written, 14,
            "one scan_file_execution_v2 child row must be included"
        );

        // A historical full-v2 database amended with an empty execution child
        // table must keep default v1 inspect usable. V2 fails closed with the
        // dedicated provenance-unavailable code instead of fabricating values
        // or classifying the otherwise valid run as corrupt.
        harness
            .store
            .connection
            .execute(
                "DELETE FROM scan_file_execution_v2 WHERE scan_run_id=?1",
                [active.scan_run_id() as i64],
            )
            .expect("simulate pre-amendment full-v2 run");
        let historical = harness
            .store
            .inspect_run(active.scan_run_id(), false)
            .expect("v1 inspect remains available");
        assert_eq!(historical.files.len(), 1);
        assert_eq!(historical.files_v2[0].parse_transport, None);
        assert_eq!(historical.files_v2[0].parse_attempt_count, None);

        let input = serde_json::to_vec(&request).expect("inspect request JSON");
        let output = crate::dispatch_with_response_version("inspect-run", &input, 2)
            .expect("v2 inspect response");
        assert_eq!(output.exit_code, 1);
        let value: serde_json::Value = serde_json::from_str(&output.json).expect("v2 JSON");
        assert_eq!(
            value.pointer("/error/error_code").and_then(serde_json::Value::as_str),
            Some("INSPECT_V2_PROVENANCE_UNAVAILABLE")
        );
    }

    #[test]
    fn zero_file_error_run_inspects_cleanly_v1_and_v2() {
        // spec Part 2.3 regression: a zero-file scheduler Error outcome
        // (source-file ceiling, BUDGET_MODEL_MISMATCH, enforced-render mismatch)
        // commits a context_runs row with 4 reconciling zero stage metrics, so
        // v1 AND v2 inspect both succeed (previously `stages.len() != 4` RUN_CORRUPT).
        let mut harness = harness("00000000-0000-4000-8000-000000000025");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        let error = Diagnostic {
            error_code: ErrorCode::SourceFileLimitExceeded,
            message: "discovery observed 1000001 source files, exceeding the engine ceiling of 1000000".to_string(),
            retryable: false,
            stage: DiagnosticStage::Discovery,
            file_path: Nullable(None),
            backend: Nullable(None),
        };
        let summary = empty_summary();
        let context = ContextRunRecord {
            context_profile_hash: "0".repeat(64),
            status: RunStatus::Error,
            final_context: String::new(),
            context_sha256: sha256_hex(b""),
            summary: summary.clone(),
            decisions: Vec::new(),
        };
        let version = crate::version_response();
        let envelope = ContextEnvelope {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: active.request_id().to_string(),
            engine_version: version.engine_version,
            engine_build: version.engine_build,
            status: EngineStatus::Error,
            file_context: String::new(),
            summary,
            scan_run_id: Nullable(Some(active.scan_run_id())),
            context_run_id: Nullable(Some(active.context_run_id())),
            warnings: Vec::new(),
            error: Nullable(Some(error.clone())),
        };
        let batch = FinalizationBatch {
            status: RunStatus::Error,
            envelope_json: canonical_envelope_json(&envelope).expect("canonical envelope"),
            inventory: Vec::new(),
            cache_writes: Vec::new(),
            file_results: Vec::new(),
            diagnostics: vec![RunDiagnosticRecord {
                severity: DiagnosticSeverity::Error,
                diagnostic: error,
            }],
            stage_metrics: vec![
                StageMetric { stage: StageName::Discovery, item_count: 0, duration_ms: 0 },
                StageMetric { stage: StageName::Cache, item_count: 0, duration_ms: 0 },
                StageMetric { stage: StageName::Parse, item_count: 0, duration_ms: 0 },
                StageMetric { stage: StageName::Context, item_count: 0, duration_ms: 0 },
            ],
            extension_metrics: Vec::new(),
            context: Some(context),
            artifact: None,
            snapshot_key: None,
            snapshot_hit: None,
            execution_metrics: Some(ai_daily_scanner_contract::ExecutionMetricsV2 {
                discovery_observed_file_count: 1_000_001,
                source_guard_content_hash_file_count: 0,
                source_guard_unavailable_count: 0,
                source_guard_bytes_read: 0,
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
                worker_handshake_ms: 5,
                discovery_ms: 2,
                snapshot_lookup_ms: 0,
                current_run_audit_write_ms: 0,
                terminal_precommit_ms: 0,
                deadline_precommit_elapsed_ms: 0,
                envelope_rebuild_ms: 0,
                terminal_rows_written: 0,
                peak_worker_rss_bytes: Nullable(None),
            }),
        };
        harness.store.finalize(&active, &batch, 2).expect("zero-file error finalize");

        let (context_run_id, status, artifact_id): (i64, String, Option<i64>) = harness
            .store
            .connection
            .query_row(
                "SELECT context_run_id, status, artifact_id
                 FROM context_runs WHERE scan_run_id=?1",
                [active.scan_run_id() as i64],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(context_run_id, active.context_run_id() as i64);
        assert_eq!(status, "error");
        assert!(artifact_id.is_none());

        let snapshot = harness
            .store
            .inspect_run(active.scan_run_id(), false)
            .expect("v1 inspect must succeed for a zero-file error run");
        assert_eq!(snapshot.run_status, RunStatus::Error);
        assert_eq!(snapshot.stage_metrics.len(), 4, "zero-file error must persist 4 stage rows");

        let request = ai_daily_scanner_contract::InspectRunRequest {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: "00000000-0000-4000-8000-000000000025".to_string(),
            scan_db_path: harness.db_path.to_string_lossy().to_string(),
            scan_run_id: active.scan_run_id(),
            include_content: false,
        };
        let v2 = crate::inspect::assemble_inspect_v2(&request, &snapshot)
            .expect("v2 inspect must succeed for a zero-file error run");
        assert_eq!(v2.status, ai_daily_scanner_contract::InspectStatus::Ok);
        assert_eq!(v2.execution_metrics.discovery_observed_file_count, 1_000_001);
    }

    #[test]
    fn cache_miss_hit_source_profile_and_build_invalidation_are_explicit() {
        let mut harness = harness("00000000-0000-4000-8000-000000000010");
        let source = "mtime_ns=100:size=5";
        let route = RouteStackFingerprint::text("build-a").unwrap();
        let profile_hash = parse_profile_hash(1, &route, &harness.profile).unwrap();
        assert_eq!(
            harness
                .store
                .lookup_cache("file-a", source, "content_sha256_v1", &"0".repeat(64), &profile_hash, false)
                .unwrap(),
            CacheLookup::Miss(CacheMissReason::NewFile)
        );
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        harness
            .store
            .finalize(
                &active,
                &success_batch(&active, "file-a", source, &profile_hash, "hello"),
                2,
            )
            .unwrap();
        assert!(matches!(
            harness
                .store
                .lookup_cache("file-a", source, "content_sha256_v1", &"0".repeat(64), &profile_hash, false)
                .unwrap(),
            CacheLookup::Fresh(_)
        ));
        assert_eq!(
            harness
                .store
                .lookup_cache("file-a", "mtime_ns=101:size=6", "content_sha256_v1", &"0".repeat(64), &profile_hash, false)
                .unwrap(),
            CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
        );
        assert_eq!(
            harness
                .store
                .lookup_cache("file-a", "mtime_ns=100:size=6", "content_sha256_v1", &"0".repeat(64), &profile_hash, false)
                .unwrap(),
            CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
        );
        assert_eq!(
            harness
                .store
                .lookup_cache("file-a", "mtime_ns=101:size=5", "content_sha256_v1", &"0".repeat(64), &profile_hash, false)
                .unwrap(),
            CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
        );

        let changed_build = RouteStackFingerprint::text("build-b").unwrap();
        let changed_hash = parse_profile_hash(1, &changed_build, &harness.profile).unwrap();
        assert_eq!(
            harness
                .store
                .lookup_cache("file-a", source, "content_sha256_v1", &"0".repeat(64), &changed_hash, false)
                .unwrap(),
            CacheLookup::Miss(CacheMissReason::ParserIdentityChanged)
        );
    }

    #[test]
    fn parse_cache_key_is_source_guard_bound() {
        // spec R3-29: a same-size + preserved-mtime content replacement keeps the
        // legacy source_version unchanged, so the SourceGuardV2 must be part of the
        // parse cache key. Changing only the guard forces a miss, never a Fresh hit.
        let mut harness = harness("00000000-0000-4000-8000-000000000099");
        let source = "mtime_ns=100:size=5";
        let profile_hash = "b".repeat(64);
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        harness
            .store
            .finalize(
                &active,
                &success_batch(&active, "file-a", source, &profile_hash, "hello"),
                2,
            )
            .unwrap();
        // Same identity + source_version + profile, SAME guard -> Fresh.
        assert!(matches!(
            harness
                .store
                .lookup_cache(
                    "file-a",
                    source,
                    "content_sha256_v1",
                    &"0".repeat(64),
                    &profile_hash,
                    false,
                )
                .unwrap(),
            CacheLookup::Fresh(_)
        ));
        // Same identity + source_version + profile, DIFFERENT guard -> miss.
        assert_eq!(
            harness
                .store
                .lookup_cache(
                    "file-a",
                    source,
                    "content_sha256_v1",
                    &"1".repeat(64),
                    &profile_hash,
                    false,
                )
                .unwrap(),
            CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
        );
        // A fresh write with the new guard lands under its own key and hits.
        let mut second = cache_record("file-a", source, &profile_hash, "changed");
        second.source_guard_sha256 = "1".repeat(64);
        harness
            .store
            .write_success_parse_cache(&[second], 5)
            .unwrap();
        assert!(matches!(
            harness
                .store
                .lookup_cache(
                    "file-a",
                    source,
                    "content_sha256_v1",
                    &"1".repeat(64),
                    &profile_hash,
                    false,
                )
                .unwrap(),
            CacheLookup::Fresh(_)
        ));
    }

    #[test]
    fn one_file_source_change_invalidates_exactly_one_cache_entry() {
        let mut harness = harness("00000000-0000-4000-8000-000000000011");
        let source = "mtime_ns=100:size=5";
        let profile_hash = "a".repeat(64);
        let first = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &first, 1);
        harness
            .store
            .finalize(
                &first,
                &success_batch(&first, "file-a", source, &profile_hash, "hello"),
                2,
            )
            .unwrap();
        let second_request_id = "00000000-0000-4000-8000-000000000016";
        let second_canonical = canonical_for_request_id(&harness, second_request_id);
        let second = started(
            harness
                .store
                .begin_run(second_request_id, &second_canonical, &harness.runtime, 3)
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &second, 3);
        harness
            .store
            .finalize(
                &second,
                &success_batch(&second, "file-b", source, &profile_hash, "world"),
                4,
            )
            .unwrap();

        let lookups = [
            harness
                .store
                .lookup_cache("file-a", "mtime_ns=101:size=6", "content_sha256_v1", &"0".repeat(64), &profile_hash, false)
                .unwrap(),
            harness
                .store
                .lookup_cache("file-b", source, "content_sha256_v1", &"0".repeat(64), &profile_hash, false)
                .unwrap(),
        ];
        assert_eq!(
            lookups
                .iter()
                .filter(|lookup| matches!(lookup, CacheLookup::Miss(_)))
                .count(),
            1
        );
        assert_eq!(
            lookups
                .iter()
                .filter(|lookup| matches!(lookup, CacheLookup::Fresh(_)))
                .count(),
            1
        );
    }

    #[test]
    fn error_results_are_never_success_cache_hits() {
        let mut harness = harness("00000000-0000-4000-8000-000000000012");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        let source = "mtime_ns=100:size=5";
        let profile_hash = "a".repeat(64);
        let diagnostic = Diagnostic {
            error_code: ErrorCode::ParserFailed,
            message: "parse failed".to_string(),
            retryable: false,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some("C:\\fixture\\evidence.txt".to_string())),
            backend: Nullable(Some("light_text_v1".to_string())),
        };
        let mut batch = error_batch(&active);
        batch.inventory.push(inventory_record("file-a", source));
        batch.file_results.push(FileResultRecord {
            file_identity: "file-a".to_string(),
            relative_path: "evidence.txt".to_string(),
            source_version: source.to_string(),
            parse_profile_hash: profile_hash.clone(),
            cache_status: CacheStatus::Miss,
            cache_miss_reason: CacheMissReason::NewFile,
            parse_status: ParseStatus::Error,
            parser_backend: "light_text_v1".to_string(),
            worker_lane: AuditWorkerLane::RustCore,
            truncated: false,
            content_sha256: sha256_hex(b""),
            primary_duration_ms: 1,
            fallback_duration_ms: 0,
            parse_duration_ms: 1,
            failure_class: "parser_failed".to_string(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ParseTransport::RustInProcess,
            parse_attempt_count: 1,
            pdf_classification: None,
            error: Some(diagnostic),
        });
        harness.store.finalize(&active, &batch, 2).unwrap();
        // v2 spec Part 4: no negative cache — an Error result row is per-run
        // audit, not a cache entry, so the guard-bound cache lookup reports a
        // new-file miss rather than the legacy `error_cache` literal.
        assert_eq!(
            harness
                .store
                .lookup_cache("file-a", source, "content_sha256_v1", &"0".repeat(64), &profile_hash, false)
                .unwrap(),
            CacheLookup::Miss(CacheMissReason::NewFile)
        );
    }

    #[test]
    fn finalize_persists_inventory_source_guard_columns() {
        let mut harness = harness("00000000-0000-4000-8000-000000000021");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        let source = "mtime_ns=100:size=5";
        let mut batch = error_batch(&active);
        let mut record = inventory_record("file-a", source);
        record.source_guard_kind = Some("content_sha256_v1".to_string());
        record.source_guard_sha256 = Some("a".repeat(64));
        batch.inventory = vec![record];
        harness.store.finalize(&active, &batch, 2).unwrap();

        let connection = rusqlite::Connection::open(&harness.db_path).unwrap();
        let (kind, hash): (Option<String>, Option<String>) = connection
            .query_row(
                "SELECT source_guard_kind, source_guard_sha256
                 FROM file_inventory WHERE file_identity=?1",
                ["file-a"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind.as_deref(), Some("content_sha256_v1"));
        assert_eq!(hash.as_deref(), Some("a".repeat(64).as_str()));
    }

    #[test]
    fn finalize_persists_unavailable_inventory_guard_with_null_hash() {
        let mut harness = harness("00000000-0000-4000-8000-000000000022");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        let source = "mtime_ns=100:size=5";
        let mut batch = error_batch(&active);
        let mut record = inventory_record("file-b", source);
        record.source_guard_kind = Some("unavailable".to_string());
        record.source_guard_sha256 = None;
        batch.inventory = vec![record];
        harness.store.finalize(&active, &batch, 2).unwrap();

        let connection = rusqlite::Connection::open(&harness.db_path).unwrap();
        let (kind, hash): (Option<String>, Option<String>) = connection
            .query_row(
                "SELECT source_guard_kind, source_guard_sha256
                 FROM file_inventory WHERE file_identity=?1",
                ["file-b"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind.as_deref(), Some("unavailable"));
        assert_eq!(hash, None, "unavailable guard must persist a null hash");
    }

    #[test]
    fn not_parsed_rows_write_not_applicable_with_empty_miss_reason() {
        // spec Part 5.2/5.3 regression (real-corpus RUN_CORRUPT): a NotParsed row
        // that performed a lookup (cache_status=Miss, reason=new_file) must be
        // persisted with parse_cache_status=not_applicable and an EMPTY
        // cache_miss_reason. FileAuditV2::validate fail-closes any non-miss row
        // that carries a non-empty reason.
        let mut harness = harness("00000000-0000-4000-8000-000000000023");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        let source = "mtime_ns=100:size=5";
        let profile_hash = "a".repeat(64);
        let mut batch = error_batch(&active);
        batch.inventory.push(inventory_record("file-a", source));
        batch.file_results.push(FileResultRecord {
            file_identity: "file-a".to_string(),
            relative_path: "evidence.txt".to_string(),
            source_version: source.to_string(),
            parse_profile_hash: profile_hash.clone(),
            cache_status: CacheStatus::Miss,
            cache_miss_reason: CacheMissReason::NewFile,
            parse_status: ParseStatus::NotParsed,
            parser_backend: "not_parsed".to_string(),
            worker_lane: AuditWorkerLane::NotParsed,
            truncated: false,
            content_sha256: sha256_hex(b""),
            primary_duration_ms: 0,
            fallback_duration_ms: 0,
            parse_duration_ms: 0,
            failure_class: String::new(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ParseTransport::NotApplicable,
            parse_attempt_count: 0,
            pdf_classification: None,
            error: None,
        });
        harness.store.finalize(&active, &batch, 2).unwrap();

        let connection = rusqlite::Connection::open(&harness.db_path).unwrap();
        let (parse_cache_status, cache_miss_reason): (Option<String>, String) = connection
            .query_row(
                "SELECT parse_cache_status, cache_miss_reason
                 FROM scan_file_results WHERE file_identity=?1",
                ["file-a"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(parse_cache_status.as_deref(), Some("not_applicable"));
        assert_eq!(
            cache_miss_reason, "",
            "not_applicable rows must carry an empty miss reason"
        );
    }

    #[test]
    fn failed_final_transaction_leaves_no_cache_inventory_or_false_success() {
        let mut harness = harness("00000000-0000-4000-8000-000000000013");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1);
        let source = "mtime_ns=100:size=5";
        let profile_hash = "a".repeat(64);
        let batch = success_batch(&active, "file-a", source, &profile_hash, "hello");
        harness
            .store
            .connection
            .execute_batch(
                "CREATE TRIGGER inject_finalization_failure
                 BEFORE INSERT ON context_runs
                 BEGIN
                    SELECT RAISE(ABORT, 'injected finalization failure');
                 END;",
            )
            .unwrap();

        assert!(matches!(
            harness.store.finalize(&active, &batch, 2),
            Err(StoreError::CacheWrite { .. })
        ));
        for table in [
            "file_inventory",
            "parse_cache",
            "scan_file_results",
            "context_runs",
        ] {
            let count: i64 = harness
                .store
                .connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
        let status: String = harness
            .store
            .connection
            .query_row(
                "SELECT status FROM scan_runs WHERE scan_run_id=?1",
                [active.scan_run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");
    }

    #[test]
    fn successful_finalization_requires_both_preflight_fingerprints() {
        let mut harness = harness("00000000-0000-4000-8000-000000000017");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        let batch = success_batch(
            &active,
            "file-a",
            "mtime_ns=100:size=5",
            &"a".repeat(64),
            "hello",
        );

        assert!(matches!(
            harness.store.finalize(&active, &batch, 2),
            Err(StoreError::RunCorrupt(_))
        ));
    }

    #[test]
    fn final_envelope_cannot_change_engine_identity_after_run_start() {
        let mut harness = harness("00000000-0000-4000-8000-000000000020");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        let mut batch = error_batch(&active);
        let mut envelope: ContextEnvelope = serde_json::from_str(&batch.envelope_json).unwrap();
        envelope.engine_build = "changed-after-start".to_string();
        batch.envelope_json = canonical_envelope_json(&envelope).unwrap();

        assert!(matches!(
            harness.store.finalize(&active, &batch, 2),
            Err(StoreError::RunCorrupt(_))
        ));
    }

    #[test]
    fn database_lock_maps_to_a_structured_retryable_error() {
        let harness = harness("00000000-0000-4000-8000-000000000014");
        let mut second = ScannerStore::open(&harness.db_path).unwrap();
        harness
            .store
            .connection
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap();
        let error = second
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap_err();
        harness.store.connection.execute_batch("ROLLBACK").unwrap();

        assert!(matches!(error, StoreError::CacheWrite { .. }));
        assert_eq!(error.error_code(), ErrorCode::CacheWriteFailed);
        assert!(error.retryable());
    }

    #[test]
    fn worker_fingerprints_and_run_diagnostics_round_trip() {
        let mut harness = harness("00000000-0000-4000-8000-000000000015");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        let office = WorkerFingerprint {
            contract: "ai_daily_worker_v1".to_string(),
            version: "0.1.0".to_string(),
            build: "office-build".to_string(),
        };
        harness
            .store
            .record_worker_fingerprints(&active, Some(&office), None, 2)
            .unwrap();
        let batch = error_batch(&active);
        let expected_diagnostics = batch.diagnostics.clone();
        harness.store.finalize(&active, &batch, 3).unwrap();

        let fingerprints: (Option<String>, Option<String>) = harness
            .store
            .connection
            .query_row(
                "SELECT office_worker_build, python_worker_build FROM scan_run_attempts
                 WHERE scan_run_id=?1 AND attempt_number=1",
                [active.scan_run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(fingerprints, (Some("office-build".to_string()), None));
        assert_eq!(
            harness
                .store
                .load_diagnostics(active.scan_run_id())
                .unwrap(),
            expected_diagnostics
        );
        assert_eq!(
            harness
                .store
                .load_terminal_envelope(active.scan_run_id())
                .unwrap()
                .envelope_json,
            batch.envelope_json
        );
    }

    #[test]
    fn inspect_run_returns_stable_rows_and_guards_fixture_content_mode() {
        let mut harness = harness("00000000-0000-4000-8000-000000000019");
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1_000,
                )
                .unwrap(),
        );
        record_both_workers(&mut harness.store, &active, 1_001);
        let batch = success_batch(
            &active,
            "file-a",
            "mtime_ns=100:size=5",
            &"a".repeat(64),
            "hello",
        );
        harness.store.finalize(&active, &batch, 1_010).unwrap();

        let snapshot = harness
            .store
            .inspect_run(active.scan_run_id(), false)
            .expect("metadata-only inspect");
        assert_eq!(snapshot.run_status, RunStatus::Success);
        assert_eq!(snapshot.context_run_id, Some(active.context_run_id()));
        assert_eq!(snapshot.summary.source_file_count, 1);
        assert_eq!(snapshot.stage_metrics.len(), 4);
        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.decisions.len(), 1);

        let restricted = harness
            .store
            .inspect_run(active.scan_run_id(), true)
            .expect_err("production databases reject content mode");
        assert!(matches!(
            restricted.error,
            crate::context_audit::InspectAuditError::ContentForbidden
        ));
        assert_eq!(restricted.run_status, Some(RunStatus::Success));

        harness
            .store
            .connection
            .pragma_update(
                None,
                "application_id",
                crate::context_audit::SANITIZED_FIXTURE_APPLICATION_ID,
            )
            .unwrap();
        harness
            .store
            .inspect_run(active.scan_run_id(), true)
            .expect("sanitized fixture content mode");

        harness
            .store
            .connection
            .execute(
                "UPDATE context_decisions SET relative_path='tampered.txt'
                 WHERE context_run_id=?1",
                [active.context_run_id() as i64],
            )
            .unwrap();
        let corrupt = harness
            .store
            .inspect_run(active.scan_run_id(), false)
            .expect_err("decision identity/path mismatch is corrupt");
        assert!(matches!(
            corrupt.error,
            crate::context_audit::InspectAuditError::RunCorrupt(_)
        ));
    }

    #[test]
    fn db_filename_guard_never_opens_the_legacy_database() {
        let directory = tempfile::tempdir().unwrap();
        let legacy = directory.path().join("scan_index.sqlite3");
        assert!(matches!(
            ScannerStore::open(&legacy),
            Err(StoreError::InvalidRequest(_))
        ));
        assert!(!legacy.exists());
    }

    #[test]
    fn minimal_profile_fixture_remains_deserializable_for_store_tests() {
        let raw: RawScannerProfileV1 = serde_json::from_value(serde_json::json!({
            "schema_version": "scanner_profile_v1"
        }))
        .unwrap();
        assert!(normalize_scanner_profile(&raw, ReportMode::Daily).is_ok());
    }

    const UPGRADE_TEST_REQUEST_ID: &str = "123e4567-e89b-42d3-a456-426614174000";
    const UPGRADE_TEST_RUN_REQUEST_ID: &str = "323e4567-e89b-42d3-a456-426614174002";

    /// A valid v1 error envelope: `file_context` empty, `error` present, and the
    /// request id matching the scan_runs row seeded by `v1_upgrade_fixture`.
    const UPGRADE_TEST_ERROR_ENVELOPE: &str = r#"{
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "323e4567-e89b-42d3-a456-426614174002",
        "engine_version": "test",
        "engine_build": "test-build",
        "status": "error",
        "file_context": "",
        "summary": {
            "source_file_count": 0, "success_count": 0, "timeout_count": 0,
            "included_file_count": 0, "omitted_file_count": 0, "error_file_count": 0,
            "input_chars": 0, "output_chars": 0, "total_duration_ms": 1,
            "discovery_duration_ms": 0, "parse_duration_ms": 0, "compression_duration_ms": 0
        },
        "scan_run_id": 1,
        "context_run_id": null,
        "warnings": [],
        "error": {
            "error_code": "PARSER_FAILED",
            "message": "scanner could not start",
            "retryable": false,
            "stage": "parse",
            "file_path": null,
            "backend": null
        }
    }"#;

    fn v1_upgrade_fixture(directory: &TempDir) -> PathBuf {
        let db_path = directory.path().join(SCAN_DB_FILENAME);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection.execute_batch(schema::V1_DDL).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        connection
            .execute(
                "INSERT INTO scan_runs(
                    request_id, canonical_request_json, request_hash_algorithm, request_hash,
                    owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                    finished_at_ms, final_envelope_json
                 ) VALUES (?1, '{}', 'sha256-request-v1', ?2, 'owner', 'error', 1, 1, 1, 1, ?3)",
                params![
                    UPGRADE_TEST_RUN_REQUEST_ID,
                    "0".repeat(64),
                    UPGRADE_TEST_ERROR_ENVELOPE,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO file_inventory(
                    file_identity, absolute_path, relative_path, file_type, source_version,
                    size_bytes, mtime_ns, last_seen_run_id, last_seen_at_ms
                 ) VALUES (
                    'C:\\work\\a.txt', 'C:\\work\\a.txt', 'a.txt', '.txt',
                    'mtime_ns=1:size=5', 5, 1, 1, 1
                 )",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO parse_cache(
                    file_identity, source_version, parse_profile_hash, content, content_sha256,
                    parser_backend, worker_lane, truncated, worker_contract_version,
                    worker_version, worker_build, cached_at_ms
                 ) VALUES (
                    'C:\\work\\a.txt', 'mtime_ns=1:size=5', ?1, 'hello', ?2,
                    'pdf_text_v1', 'python_document_process', 0,
                    'ai_daily_worker_v1', '1.0', 'legacy-build', 1
                 )",
                params!["0".repeat(64), "1".repeat(64)],
            )
            .unwrap();
        drop(connection);
        db_path
    }

    fn upgrade_request(path: &Path, apply: bool) -> ai_daily_scanner_contract::UpgradeDatabaseRequestV1 {
        ai_daily_scanner_contract::UpgradeDatabaseRequestV1 {
            contract: "ai_daily_scanner_upgrade".to_string(),
            protocol_version: 1,
            request_id: UPGRADE_TEST_REQUEST_ID.to_string(),
            scan_db_path: path.to_string_lossy().to_string(),
            apply,
        }
    }

    #[test]
    fn business_open_fails_closed_on_committed_v1_database() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = v1_upgrade_fixture(&directory);

        assert_eq!(
            ScannerStore::open(&db_path).unwrap_err(),
            StoreError::SchemaUpgradeRequired
        );
        assert_eq!(
            ScannerStore::open_existing(&db_path).unwrap_err(),
            StoreError::SchemaUpgradeRequired
        );
    }

    #[test]
    fn business_open_fails_closed_on_too_new_database() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join(SCAN_DB_FILENAME);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection.execute_batch(schema::V1_DDL).unwrap();
        connection.pragma_update(None, "user_version", 3).unwrap();
        drop(connection);

        assert!(matches!(
            ScannerStore::open(&db_path),
            Err(StoreError::SchemaTooNew)
        ));
    }

    #[test]
    fn upgrade_database_audit_reports_legacy_cache_without_mutating() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = v1_upgrade_fixture(&directory);
        let before = std::fs::read(&db_path).unwrap();

        let response = ScannerStore::upgrade_database(&upgrade_request(&db_path, false));
        response.validate().expect("audit response must validate");

        assert_eq!(response.status, UpgradeStatus::Ok);
        assert_eq!(response.apply, false);
        assert_eq!(response.source_user_version.0, Some(1));
        assert_eq!(response.schema_migrated, false);
        assert_eq!(response.auto_vacuum_converted, false);
        assert_eq!(response.legacy_parse_cache_rows_detected, 1);
        assert_eq!(response.invalidated_parse_cache_rows, 0);
        assert_eq!(response.post_integrity_check, UpgradeIntegrityCheck::NotRun);
        assert!(response.error.0.is_none());
        assert_eq!(std::fs::read(&db_path).unwrap(), before);
    }

    #[test]
    fn upgrade_database_apply_migrates_and_invalidates_legacy_cache() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = v1_upgrade_fixture(&directory);

        let response = ScannerStore::upgrade_database(&upgrade_request(&db_path, true));
        response.validate().expect("apply response must validate");

        assert_eq!(response.status, UpgradeStatus::Ok);
        assert_eq!(response.apply, true);
        assert_eq!(response.source_user_version.0, Some(1));
        assert_eq!(response.schema_migrated, true);
        assert_eq!(response.auto_vacuum_converted, true);
        assert_eq!(response.legacy_parse_cache_rows_detected, 1);
        assert_eq!(response.invalidated_parse_cache_rows, 1);
        assert_eq!(response.post_integrity_check, UpgradeIntegrityCheck::Ok);
        assert!(response.error.0.is_none());

        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let origin: String = connection
            .query_row(
                "SELECT origin FROM schema_migration_history WHERE user_version=2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(origin, "upgraded_v1");
        let cache_count: i64 = connection
            .query_row("SELECT count(*) FROM parse_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cache_count, 0);
    }

    #[test]
    fn upgrade_database_apply_reports_partial_when_vacuum_conversion_fails() {
        let directory = tempfile::tempdir().unwrap();
        let db_path = v1_upgrade_fixture(&directory);
        TEST_FORCE_AUTO_VACUUM_FAILURE.with(|flag| flag.set(true));

        let response = ScannerStore::upgrade_database(&upgrade_request(&db_path, true));
        TEST_FORCE_AUTO_VACUUM_FAILURE.with(|flag| flag.set(false));
        response.validate().expect("partial response must validate");

        assert_eq!(response.status, UpgradeStatus::Partial);
        assert_eq!(response.apply, true);
        assert_eq!(response.source_user_version.0, Some(1));
        assert_eq!(response.schema_migrated, true);
        assert_eq!(response.auto_vacuum_converted, false);
        assert_eq!(response.legacy_parse_cache_rows_detected, 1);
        assert_eq!(response.invalidated_parse_cache_rows, 1);
        assert_eq!(response.post_integrity_check, UpgradeIntegrityCheck::Ok);
        assert!(response.error.0.is_none(), "partial must not carry an error");
        assert!(
            !response.warnings.is_empty(),
            "partial must carry a maintenance warning"
        );
        assert_eq!(
            response.warnings[0].error_code,
            ErrorCode::MaintenanceModeUnavailable
        );
        assert_eq!(response.warnings[0].stage, DiagnosticStage::Maintenance);

        // The v2 business schema is still valid and committed.
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    // -----------------------------------------------------------------------
    // maintenance command（spec Part 4/5.3）
    // -----------------------------------------------------------------------

    fn maintenance_request(
        db_path: &Path,
        mode: MaintenanceMode,
        dry_run: bool,
    ) -> MaintenanceRequestV1 {
        MaintenanceRequestV1 {
            contract: "ai_daily_scanner_maintenance".to_string(),
            protocol_version: 1,
            request_id: "123e4567-e89b-42d3-a456-426614174100".to_string(),
            scan_db_path: db_path.to_string_lossy().to_string(),
            mode,
            dry_run,
        }
    }

    #[test]
    fn maintenance_dry_run_ok_on_fresh_v2_db() {
        let harness = harness("00000000-0000-4000-8000-000000000101");
        let response = ScannerStore::maintenance(&maintenance_request(
            &harness.db_path,
            MaintenanceMode::Gc,
            true,
        ));
        response.validate().expect("maintenance response validates");
        assert_eq!(response.status, MaintenanceStatus::Ok);
        assert_eq!(response.deleted.parse_cache_rows, 0);
        assert_eq!(response.deleted.scan_runs_rows, 0);
        assert_eq!(response.before, response.after);
        assert!(response.after_complete);
        assert_eq!(
            response.pre_integrity_check,
            MaintenancePreIntegrityCheck::Ok
        );
        assert_eq!(
            response.post_integrity_check,
            MaintenancePostIntegrityCheck::NotRun
        );
        assert_eq!(response.vacuum.status, MaintenanceVacuumStatus::SkippedDryRun);
        assert!(response.error.0.is_none());
    }

    #[test]
    fn maintenance_gc_deletes_aged_terminal_runs() {
        let harness = harness("00000000-0000-4000-8000-000000000102");
        // Insert an aged terminal run directly.
        let connection = rusqlite::Connection::open(&harness.db_path).unwrap();
        connection
            .execute(
                "INSERT INTO scan_runs(
                    request_id, canonical_request_json, request_hash_algorithm, request_hash,
                    owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                    finished_at_ms, final_envelope_json, audit_provenance_version, audit_size_bytes
                 ) VALUES ('aged', '{}', 'sha256-request-v1', ?, 'owner', 'error',
                            1, 1, 1, 1, '{}', 'full_v2', 10)",
                params![format!("{:0>64}", "0")],
            )
            .unwrap();
        drop(connection);

        let response = ScannerStore::maintenance(&maintenance_request(
            &harness.db_path,
            MaintenanceMode::Gc,
            false,
        ));
        response.validate().expect("maintenance response validates");
        assert_eq!(response.status, MaintenanceStatus::Ok);
        assert_eq!(response.deleted.scan_runs_rows, 1);
        assert_eq!(
            response.vacuum.status,
            MaintenanceVacuumStatus::NotRequested
        );
        assert_eq!(response.post_integrity_check, MaintenancePostIntegrityCheck::Ok);
        assert!(response.error.0.is_none());
    }

    #[test]
    fn maintenance_incremental_vacuum_on_auto_vacuum_none_fails_cleanly() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let db_path = directory.path().join(SCAN_DB_FILENAME);
        // Build a v2 DB WITHOUT auto_vacuum=INCREMENTAL (default none).
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection.execute_batch(schema::V2_DDL).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migration_history(
                    user_version, origin, upgrade_request_id, engine_build, committed_at_ms
                 ) VALUES (2, 'created_empty', NULL, 'build', 1)",
                [],
            )
            .unwrap();
        drop(connection);

        let response = ScannerStore::maintenance(&maintenance_request(
            &db_path,
            MaintenanceMode::IncrementalVacuum,
            false,
        ));
        response.validate().expect("maintenance response validates");
        assert_eq!(response.status, MaintenanceStatus::Error);
        let error = response.error.0.as_ref().expect("error diagnostic");
        assert_eq!(error.error_code, ErrorCode::MaintenanceModeUnavailable);
        assert_eq!(error.stage, DiagnosticStage::Maintenance);
        assert_eq!(response.vacuum.status, MaintenanceVacuumStatus::Error);
        assert_eq!(response.deleted.scan_runs_rows, 0);
        assert_eq!(response.before, response.after);
    }

    #[test]
    fn retention_gc_for_current_run_trims_to_run_count_cap() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let db_path = directory.path().join(SCAN_DB_FILENAME);
        let mut connection = rusqlite::Connection::open(&db_path).unwrap();
        connection.execute_batch(schema::V2_DDL).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migration_history(
                    user_version, origin, upgrade_request_id, engine_build, committed_at_ms
                 ) VALUES (2, 'created_empty', NULL, 'build', 1)",
                [],
            )
            .unwrap();
        connection.pragma_update(None, "foreign_keys", true).unwrap();
        // 520 aged terminal runs (each tiny audit).
        for index in 0..520 {
            connection
                .execute(
                    "INSERT INTO scan_runs(
                        request_id, canonical_request_json, request_hash_algorithm, request_hash,
                        owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                        finished_at_ms, final_envelope_json, audit_provenance_version, audit_size_bytes
                     ) VALUES (?1, '{}', 'sha256-request-v1', ?2, 'owner', 'error',
                                1, 1, 1, ?3, '{}', 'full_v2', 100)",
                    params![
                        format!("aged-{index}"),
                        "0".repeat(64),
                        1_000 + index as i64,
                    ],
                )
                .unwrap();
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        // 当前 run 已 terminal（由 finalize 先写）；此处直接调用 retention GC。
        let protected_runs = std::collections::HashSet::from([1]);
        retention_gc_for_current_run(&transaction, &protected_runs, 1_000_000, 100)
            .expect("retention gc succeeds");
        let count: i64 = transaction
            .query_row(
                "SELECT count(*) FROM scan_runs
                 WHERE status IN ('success', 'partial', 'error', 'abandoned')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, cache::TERMINAL_RUN_MAX_COUNT);
        transaction.commit().unwrap();
    }

    #[test]
    fn retention_gc_for_current_run_fails_closed_when_record_exceeds_cap() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let db_path = directory.path().join(SCAN_DB_FILENAME);
        let mut connection = rusqlite::Connection::open(&db_path).unwrap();
        connection.execute_batch(schema::V2_DDL).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migration_history(
                    user_version, origin, upgrade_request_id, engine_build, committed_at_ms
                 ) VALUES (2, 'created_empty', NULL, 'build', 1)",
                [],
            )
            .unwrap();
        connection.pragma_update(None, "foreign_keys", true).unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        // 当前 record 自身超 2 GiB → fail closed。
        let protected_runs = std::collections::HashSet::from([1]);
        let error = retention_gc_for_current_run(
            &transaction,
            &protected_runs,
            1_000_000,
            cache::TERMINAL_AUDIT_MAX_BYTES + 1,
        )
        .expect_err("current audit over cap must fail closed");
        assert!(matches!(error, StoreError::RunCorrupt(_)));
        transaction.commit().unwrap();
    }
}
