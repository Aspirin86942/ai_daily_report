//! Rust-owned scanner inventory, successful parse cache, and run audit store.

pub mod cache;
pub mod inventory;
pub mod schema;

use ai_daily_discovery::normalize_contract_path_text;
use ai_daily_scanner_contract::{
    BuildContextRequest, ContextAction, ContextDecision, ContextEnvelope, ContextSummary,
    Diagnostic, DiagnosticStage, EngineStatus, ErrorCode, ExtensionMetric,
    NormalizedScannerProfileV1, Nullable, ParseStatus, RunStatus, StageMetric, StageName,
    UpgradeDatabaseRequestV1, UpgradeDatabaseResponseV1, UpgradeIntegrityCheck, UpgradeStatus,
    Validate, VersionResponse,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

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
    final_envelope_json: Option<String>,
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
                return load_stored_envelope(existing, request_id);
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
                let stored = load_stored_envelope_ref(value, request_id)?;
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
    ) -> Result<(), StoreError> {
        let envelope = validate_finalization(active, batch)?;
        let now_ms = checked_i64(now_ms, "finalization timestamp")?;
        schema::require_durable_finalization(&self.connection)
            .map_err(|error| cache_write(error.to_string()))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
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

        inventory::upsert_inventory(&transaction, active.scan_run_id, now_ms, &batch.inventory)
            .map_err(cache_write)?;
        cache::write_success_cache(&transaction, now_ms, &batch.cache_writes)
            .map_err(cache_write)?;
        inventory::insert_file_results(&transaction, active.scan_run_id, &batch.file_results)
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
        if let Some(context) = &batch.context {
            insert_context(&transaction, active.scan_run_id, context, now_ms)
                .map_err(cache_write)?;
        }

        let status = terminal_status_text(batch.status)?;
        let updated = transaction
            .execute(
                "UPDATE scan_runs
                 SET status=?1, updated_at_ms=?2, finished_at_ms=?2, final_envelope_json=?3
                 WHERE scan_run_id=?4 AND owner_id=?5 AND status='running'",
                params![
                    status,
                    now_ms,
                    batch.envelope_json,
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
        Ok(())
    }

    pub fn load_terminal_envelope(&self, scan_run_id: u64) -> Result<StoredEnvelope, StoreError> {
        let scan_run_id = checked_i64(scan_run_id, "scan run id")?;
        let row: (String, String, String, String, Option<String>) = self
            .connection
            .query_row(
                "SELECT request_id, canonical_request_json, request_hash, status, final_envelope_json
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
            final_envelope_json: row.4,
        };
        load_stored_envelope_ref(&existing, &row.0)
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
            "SELECT scan_run_id, canonical_request_json, request_hash, status, final_envelope_json
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
            final_envelope_json: row.4,
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
    existing: ExistingRun,
    request_id: &str,
) -> Result<BeginRunOutcome, StoreError> {
    load_stored_envelope_ref(&existing, request_id)
        .map(Box::new)
        .map(BeginRunOutcome::Stored)
}

fn load_stored_envelope_ref(
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
    let envelope_json = existing
        .final_envelope_json
        .clone()
        .ok_or_else(|| StoreError::RunCorrupt("terminal run has no final envelope".to_string()))?;
    let envelope: ContextEnvelope = serde_json::from_str(&envelope_json)
        .map_err(|_| StoreError::RunCorrupt("final envelope JSON is invalid".to_string()))?;
    envelope
        .validate()
        .map_err(|_| StoreError::RunCorrupt("final envelope violates the contract".to_string()))?;
    if canonical_envelope_json(&envelope).as_deref() != Ok(envelope_json.as_str()) {
        return Err(StoreError::RunCorrupt(
            "final envelope JSON is not canonical".to_string(),
        ));
    }
    if envelope.request_id != request_id
        || envelope.scan_run_id.0 != Some(existing.scan_run_id as u64)
        || !envelope_status_matches(existing.status, envelope.status)
    {
        return Err(StoreError::RunCorrupt(
            "final envelope does not match its run".to_string(),
        ));
    }
    Ok(StoredEnvelope {
        scan_run_id: existing.scan_run_id as u64,
        envelope_json,
        envelope,
    })
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
        record.validate().map_err(StoreError::InvalidRequest)?;
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

fn insert_context(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
    context: &ContextRunRecord,
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
            created_at_ms
         ) VALUES (
            ?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::{ClassificationError, ParserRoute};
    use crate::config::{normalize_scanner_profile, normalize_scanner_profile_for_request};
    use crate::planner::{PlanAction, PlannedFile};
    use ai_daily_discovery::DiscoveredFileOut;
    use ai_daily_scanner_contract::{
        AdapterPaths, AuditWorkerLane, CacheMissReason, CacheStatus, ContextAction,
        ContextDecision, DiagnosticStage, ParseStatus, RawScannerProfileV1, ReportMode,
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
            error: None,
        }
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
            CacheLookup::Miss(CacheMissReason::ParserProfileChanged)
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
}
