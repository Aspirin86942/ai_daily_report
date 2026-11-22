use super::*;

/// Error code spellings consumed by existing scanner diagnostics and cache records.
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
    pub file_path: Option<String>,
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    Success,
    Error,
    Timeout,
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
