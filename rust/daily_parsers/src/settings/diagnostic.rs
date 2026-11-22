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
    pub file_path: Nullable<String>,
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
    RustOfficeInProcess,
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
            Self::RustOfficeInProcess => "rust_office_in_process",
            Self::RustPdfProcessV3 => "rust_pdf_process_v3",
            Self::LocalOcrPreflight => "local_ocr_preflight",
            Self::NotParsed => "not_parsed",
        }
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
