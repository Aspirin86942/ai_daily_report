use super::*;

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
    #[error("scanner database schema mismatch: expected user_version=3, found {found}")]
    SchemaMismatch { found: i32 },
    #[error("artifact is not dedup-compatible with the stored snapshot: {0}")]
    ArtifactMismatch(String),
    #[error("absolute deadline exhausted before terminal commit")]
    AbsoluteDeadlineExhausted,
    #[error("work deadline exhausted before cache commit")]
    WorkDeadlineExhausted,
    #[error("scanner database busy timeout could not be restored")]
    BusyTimeoutRestore { detail: String },
}

impl StoreError {
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::CacheOpen { .. } => ErrorCode::CacheOpenFailed,
            Self::CacheWrite { .. } | Self::BusyTimeoutRestore { .. } | Self::LeaseLost => {
                ErrorCode::CacheWriteFailed
            }
            Self::ScanAlreadyRunning => ErrorCode::ScanAlreadyRunning,
            Self::RequestInProgress => ErrorCode::RequestInProgress,
            Self::RequestIdConflict => ErrorCode::RequestIdConflict,
            Self::RunCorrupt(_) => ErrorCode::RunCorrupt,
            Self::RunNotFound => ErrorCode::RunNotFound,
            Self::SchemaMismatch { .. } => ErrorCode::ScannerDbSchemaMismatch,
            Self::ArtifactMismatch(_) => ErrorCode::BudgetModelMismatch,
            Self::AbsoluteDeadlineExhausted | Self::WorkDeadlineExhausted => {
                ErrorCode::StageDeadlineExhausted
            }
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::CacheOpen { .. }
                | Self::CacheWrite { .. }
                | Self::BusyTimeoutRestore { .. }
                | Self::ScanAlreadyRunning
                | Self::RequestInProgress
                | Self::LeaseLost
                | Self::AbsoluteDeadlineExhausted
                | Self::WorkDeadlineExhausted
        )
    }

    pub fn diagnostic(&self, stage: DiagnosticStage) -> Diagnostic {
        let message = match self {
            Self::CacheWrite { detail } => {
                let reason = if detail
                    .contains("UNIQUE constraint failed: context_artifacts.snapshot_key_sha256")
                {
                    "snapshot_key_unique"
                } else if detail.contains("UNIQUE constraint failed") {
                    "unique_constraint"
                } else if detail.contains("FOREIGN KEY constraint failed") {
                    "foreign_key_constraint"
                } else if detail.contains("CHECK constraint failed") {
                    "check_constraint"
                } else if detail.contains("NOT NULL constraint failed") {
                    "not_null_constraint"
                } else if detail.contains("database is locked")
                    || detail.contains("database is busy")
                {
                    "database_busy"
                } else if detail.contains("database or disk is full") {
                    "disk_full"
                } else if detail.contains("Invalid column type") {
                    "stored_column_type"
                } else {
                    "unclassified"
                };
                format!("scanner database transaction failed ({reason})")
            }
            _ => self.to_string(),
        };
        Diagnostic {
            error_code: self.error_code(),
            message,
            retryable: self.retryable(),
            stage,
            file_path: Nullable(None),
            backend: Nullable(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRequest {
    pub(super) request_id: String,
    pub(super) json: String,
    pub(super) hash_algorithm: &'static str,
    pub(super) hash: String,
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
pub(super) struct CanonicalLogicalRequest<'a> {
    pub(super) contract: &'a str,
    pub(super) protocol_version: u64,
    pub(super) work_dir: &'a str,
    pub(super) start_date: &'a str,
    pub(super) end_date: &'a str,
    pub(super) report_mode: ai_daily_scanner_contract::ReportMode,
    pub(super) scanner_settings: CanonicalScannerSettings<'a>,
    pub(super) context_profile: &'a ai_daily_scanner_contract::ContextProfile,
}

#[derive(Serialize)]
pub(super) struct CanonicalScannerSettings<'a> {
    pub(super) core_build_identity: &'a str,
    pub(super) discovery: &'a ai_daily_scanner_contract::DiscoveryProfile,
    pub(super) execution: &'a ai_daily_scanner_contract::ExecutionProfile,
    pub(super) parse: &'a ai_daily_scanner_contract::ParseProfile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRuntime {
    pub(super) normalized_scan_db_path: String,
    pub(super) normalized_office_worker_path: String,
    pub(super) normalized_python_executable: String,
    pub(super) normalized_python_module_root: String,
    pub(super) python_document_worker_module: String,
    pub(super) engine_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EngineFingerprint {
    pub(super) contract: String,
    pub(super) protocol_version: u64,
    pub(super) binary_name: String,
    pub(super) engine_version: String,
    pub(super) engine_build: String,
    pub(super) target_triple: String,
}

impl AttemptRuntime {
    pub fn from_request(request: &BuildContextRequest) -> Result<Self, StoreError> {
        Self::from_request_with_identity(request, &engine_identity())
    }

    pub(crate) fn from_request_with_identity(
        request: &BuildContextRequest,
        identity: &EngineIdentity,
    ) -> Result<Self, StoreError> {
        let engine_fingerprint = serde_json::to_string(&EngineFingerprint {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            binary_name: "ai_daily_scanner_native".to_string(),
            engine_version: identity.engine_version.clone(),
            engine_build: identity.engine_build.clone(),
            target_triple: identity.target_triple.clone(),
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

    pub(super) fn validate(&self) -> Result<(), StoreError> {
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

pub(super) fn normalize_runtime_path(value: &str) -> String {
    let path = Path::new(value);
    if let Ok(canonical) = fs::canonicalize(path) {
        return normalize_contract_path_text(&canonical.to_string_lossy());
    }

    // Runtime targets may not exist when the attempt identity is created. Resolve
    // their existing parent so Windows case and 8.3 aliases stay stable after creation.
    if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) {
        if let Ok(canonical_parent) = fs::canonicalize(parent) {
            let canonical = canonical_parent.join(file_name);
            return normalize_contract_path_text(&canonical.to_string_lossy());
        }
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
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

pub(super) fn is_contract_absolute_text(value: &str) -> bool {
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
    pub(super) fn validate(&self) -> Result<(), StoreError> {
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
    pub(super) scan_run_id: i64,
    pub(super) attempt_number: i64,
    pub(super) owner_id: String,
    pub(super) request_id: String,
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
    pub(super) fn as_str(self) -> &'static str {
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
    /// returned with current-run evidence. `None` only for test-only batches.
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
    /// The terminal COMMIT is authoritative even if restoring the connection's
    /// default busy timeout subsequently fails. The caller must surface this
    /// redacted runtime warning and skip any post-commit use of the connection.
    pub busy_timeout_restore_failed: bool,
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
pub(super) enum PersistedRunStatus {
    Running,
    Success,
    Partial,
    Error,
    Abandoned,
}

impl PersistedRunStatus {
    pub(super) fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "partial" => Ok(Self::Partial),
            "error" => Ok(Self::Error),
            "abandoned" => Ok(Self::Abandoned),
            _ => Err(StoreError::RunCorrupt("unknown run status".to_string())),
        }
    }

    pub(super) fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Partial | Self::Error)
    }
}

#[derive(Debug)]
pub(super) struct ExistingRun {
    pub(super) scan_run_id: i64,
    pub(super) canonical_request_json: String,
    pub(super) request_hash: String,
    pub(super) status: PersistedRunStatus,
    /// Body-free `final_envelope_metadata_json` (spec Part 5.1): the full
    /// `ContextEnvelope` is rebuilt from metadata + summary + artifact.
    pub(super) final_envelope_metadata_json: Option<String>,
}

#[derive(Debug)]
pub(super) struct ExistingLease {
    pub(super) owner_id: String,
    pub(super) heartbeat_at_ms: i64,
    pub(super) expires_at_ms: i64,
}

#[derive(Debug)]
pub(super) struct RawDiagnosticRow {
    pub(super) severity: String,
    pub(super) error_code: String,
    pub(super) message: String,
    pub(super) retryable: bool,
    pub(super) stage: String,
    pub(super) file_path: Option<String>,
    pub(super) backend: Option<String>,
}
