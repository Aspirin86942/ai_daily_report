use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidRequest,
    ContractVersionMismatch,
    WorkDirNotFound,
    WorkDirNotDirectory,
    DiscoveryEntryUnreadable,
    FileTooLarge,
    ParserStartFailed,
    ParserTimeout,
    ParserInvalidPayload,
    ParserFailed,
    PdfProtected,
    WorkerHandshakeFailed,
    WorkerVersionMismatch,
    WorkerBuildChanged,
    SourceVersionChanged,
    CacheOpenFailed,
    CacheWriteFailed,
    ScanAlreadyRunning,
    RequestInProgress,
    RequestIdConflict,
    RunNotFound,
    RunCorrupt,
    ContextBudgetInvalid,
    NotImplemented,
    RustCoreCrashed,
    StageDeadlineExhausted,
    BudgetModelMismatch,
    ContextFixedSectionsOverBudget,
    ProfileRouteInvariant,
    SourceFileLimitExceeded,
    SourceGuardUnavailable,
    ScannerDbSchemaMismatch,
    DiagnosticsAggregated,
    SnapshotReuseProjectedAsFresh,
    ParseCacheNotApplicableProjectedAsMiss,
    CacheMissReasonProjectedAsNewFile,
    SourceGuardNotProjected,
    WorkerRssUnavailable,
    InternalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStage {
    Request,
    Discovery,
    Cache,
    Parse,
    Context,
    Process,
    Doctor,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub error_code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub stage: DiagnosticStage,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub file_path: Nullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub backend: Nullable<String>,
}

impl Validate for Diagnostic {
    fn validate(&self) -> Result<(), String> {
        require_non_empty(&self.message, 4096, "diagnostic.message")?;
        if let Some(file_path) = &self.file_path.0 {
            require_absolute_path(file_path, "diagnostic.file_path")?;
        }
        if let Some(backend) = &self.backend.0 {
            require_non_empty(backend, 1024, "diagnostic.backend")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Success,
    Partial,
    Error,
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageName {
    Discovery,
    Cache,
    Parse,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageMetric {
    pub stage: StageName,
    pub item_count: u64,
    pub duration_ms: u64,
}

impl Validate for StageMetric {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionMetric {
    pub extension: String,
    pub file_count: u64,
    pub parse_duration_ms: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub timeout_count: u64,
}

impl Validate for ExtensionMetric {
    fn validate(&self) -> Result<(), String> {
        require_extension(&self.extension, "extension metric extension")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    Success,
    Error,
    Timeout,
    NotParsed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditWorkerLane {
    RustCore,
    RustOfficeProcessV3,
    RustPdfProcessV3,
    LocalOcrPreflight,
    NotParsed,
}

impl ParseStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::NotParsed => "not_parsed",
        }
    }
}

impl AuditWorkerLane {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RustCore => "rust_core",
            Self::RustOfficeProcessV3 => "rust_office_process_v3",
            Self::RustPdfProcessV3 => "rust_pdf_process_v3",
            Self::LocalOcrPreflight => "local_ocr_preflight",
            Self::NotParsed => "not_parsed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    Fresh,
    Miss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheMissReason {
    #[serde(rename = "")]
    None,
    #[serde(rename = "new_file")]
    NewFile,
    #[serde(rename = "error_cache")]
    ErrorCache,
    #[serde(rename = "source_version_changed")]
    SourceVersionChanged,
    #[serde(rename = "parser_profile_changed")]
    ParserProfileChanged,
    /// v2-only reason (spec Part 4): exact key absent but another identity row
    /// for the same source exists. Projected losslessly to `parser_profile_changed`
    /// in the v1 inspect (spec Part 5.3).
    #[serde(rename = "parser_identity_changed")]
    ParserIdentityChanged,
    /// v2-only reason (spec Part 4): inventory existed before this round but the
    /// exact cache entry is absent/evicted. Projected to `new_file` + warning in
    /// the v1 inspect (spec Part 5.3).
    #[serde(rename = "entry_absent_or_evicted")]
    EntryAbsentOrEvicted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAudit {
    pub relative_path: String,
    pub file_identity: String,
    pub source_version: String,
    pub parse_status: ParseStatus,
    pub parser_backend: String,
    pub worker_lane: AuditWorkerLane,
    pub cache_status: CacheStatus,
    pub cache_miss_reason: CacheMissReason,
    pub truncated: bool,
    pub content_sha256: String,
    pub parse_duration_ms: u64,
    pub failure_class: String,
    pub fallback_backend: String,
    pub fallback_reason_code: String,
}

pub(super) fn validate_file_identity(
    relative_path: &str,
    file_identity: &str,
    source_version: &str,
    parser_backend: &str,
) -> Result<(), String> {
    require_relative_path(relative_path, "file relative_path")?;
    require_non_empty(file_identity, 4096, "file_identity")?;
    require_source_version(source_version, "source_version")?;
    require_non_empty(parser_backend, 4096, "parser_backend")
}

impl Validate for FileAudit {
    fn validate(&self) -> Result<(), String> {
        validate_file_identity(
            &self.relative_path,
            &self.file_identity,
            &self.source_version,
            &self.parser_backend,
        )?;
        require_sha256_hex(&self.content_sha256, "content_sha256")
            .map_err(|_| "content_sha256 must be lowercase SHA-256".to_string())?;
        for (field, value) in [
            ("failure_class", self.failure_class.as_str()),
            ("fallback_backend", self.fallback_backend.as_str()),
            ("fallback_reason_code", self.fallback_reason_code.as_str()),
        ] {
            if value.chars().count() > 1024 {
                return Err(format!("{field} is too long"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAction {
    Keep,
    Compress,
    MetadataOnly,
    Omit,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDecision {
    pub relative_path: String,
    pub action: ContextAction,
    pub reason: String,
    pub priority: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub truncated: bool,
    pub error_code: String,
}

impl Validate for ContextDecision {
    fn validate(&self) -> Result<(), String> {
        require_relative_path(&self.relative_path, "decision relative_path")?;
        require_non_empty(&self.reason, 4096, "decision reason")?;
        if self.error_code.chars().count() <= 1024 {
            Ok(())
        } else {
            Err("decision error_code is too long".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Complete scanner evidence and worker contracts.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheRetentionPolicy {
    pub policy_version: String,
    pub parse_cache_max_bytes: u64,
    pub classification_cache_max_bytes: u64,
    pub context_artifacts_max_bytes: u64,
    pub terminal_audit_max_bytes: u64,
    pub terminal_run_max_count: u64,
    pub terminal_run_max_age_days: u64,
    pub opportunistic_gc_budget_ms: u64,
}

impl Validate for CacheRetentionPolicy {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.policy_version, "cache_retention_v1", "policy_version")?;
        for (field, limit) in [
            ("parse_cache_max_bytes", self.parse_cache_max_bytes),
            (
                "classification_cache_max_bytes",
                self.classification_cache_max_bytes,
            ),
            (
                "context_artifacts_max_bytes",
                self.context_artifacts_max_bytes,
            ),
            ("terminal_audit_max_bytes", self.terminal_audit_max_bytes),
            ("terminal_run_max_count", self.terminal_run_max_count),
            ("terminal_run_max_age_days", self.terminal_run_max_age_days),
            (
                "opportunistic_gc_budget_ms",
                self.opportunistic_gc_budget_ms,
            ),
        ] {
            require_range(limit, 1, u64::MAX, field)?;
        }
        Ok(())
    }
}
