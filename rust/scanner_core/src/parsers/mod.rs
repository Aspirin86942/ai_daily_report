pub mod classifier;
pub mod document;
pub mod light_text;
pub mod office;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::classifier::ParserRoute;
use crate::fallback::{FailureClass, ParseFailure};
use crate::process::{run_process, ProcessError, ProcessSpec, WorkerRssTracker};
use ai_daily_scanner_contract::{
    AdapterPaths, Diagnostic, DiagnosticStage, ErrorCode, NormalizedScannerProfileV1, Nullable,
    Validate, WorkerBackend, WorkerDiagnosticV1, WorkerDiagnosticV1ErrorCode,
    WorkerDiagnosticV1Stage, WorkerKind, WorkerParseRequest, WorkerParseResponse,
    WorkerParserLimits, WorkerStatus, WorkerVersionResponse,
};

pub const WORKER_CONTRACT_VERSION: &str = ai_daily_worker_contract::CONTRACT_VERSION;
pub const WORKER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_HANDSHAKE_CAPTURE_LIMIT: usize = 4 * 1024 * 1024;
const MAX_JSON_BYTES_PER_SCALAR: u64 = 12;
const MAX_DIAGNOSTIC_COUNT: u64 = 257;
const MAX_DIAGNOSTIC_MESSAGE_CHARS: u64 = 4_096;
const MAX_WORKER_IDENTITY_CHARS: u64 = 3 * 1_024;
const RESPONSE_JSON_FIXED_ALLOWANCE: u64 = 64 * 1024;

/// Adapter seam (spec Part 7.1): translate a frozen worker diagnostic to the
/// scanner-side extended `Diagnostic`. Every path that reads a
/// `WorkerDiagnosticV1` from the `ai_daily_worker_v1` / `ai_daily_transport`
/// wires goes through here; the frozen code maps onto the extended enum so new
/// scanner-side codes can never deserialize on the old worker wire.
pub fn worker_diagnostic_to_scanner(worker: &WorkerDiagnosticV1) -> Diagnostic {
    Diagnostic {
        error_code: match worker.error_code {
            WorkerDiagnosticV1ErrorCode::InvalidRequest => ErrorCode::InvalidRequest,
            WorkerDiagnosticV1ErrorCode::ContractVersionMismatch => {
                ErrorCode::ContractVersionMismatch
            }
            WorkerDiagnosticV1ErrorCode::WorkDirNotFound => ErrorCode::WorkDirNotFound,
            WorkerDiagnosticV1ErrorCode::WorkDirNotDirectory => ErrorCode::WorkDirNotDirectory,
            WorkerDiagnosticV1ErrorCode::DiscoveryEntryUnreadable => {
                ErrorCode::DiscoveryEntryUnreadable
            }
            WorkerDiagnosticV1ErrorCode::FileTooLarge => ErrorCode::FileTooLarge,
            WorkerDiagnosticV1ErrorCode::ParserStartFailed => ErrorCode::ParserStartFailed,
            WorkerDiagnosticV1ErrorCode::ParserTimeout => ErrorCode::ParserTimeout,
            WorkerDiagnosticV1ErrorCode::ParserInvalidPayload => ErrorCode::ParserInvalidPayload,
            WorkerDiagnosticV1ErrorCode::ParserFailed => ErrorCode::ParserFailed,
            WorkerDiagnosticV1ErrorCode::WorkerHandshakeFailed => ErrorCode::WorkerHandshakeFailed,
            WorkerDiagnosticV1ErrorCode::WorkerVersionMismatch => ErrorCode::WorkerVersionMismatch,
            WorkerDiagnosticV1ErrorCode::WorkerBuildChanged => ErrorCode::WorkerBuildChanged,
            WorkerDiagnosticV1ErrorCode::SourceVersionChanged => ErrorCode::SourceVersionChanged,
            WorkerDiagnosticV1ErrorCode::CacheOpenFailed => ErrorCode::CacheOpenFailed,
            WorkerDiagnosticV1ErrorCode::CacheWriteFailed => ErrorCode::CacheWriteFailed,
            WorkerDiagnosticV1ErrorCode::ScanAlreadyRunning => ErrorCode::ScanAlreadyRunning,
            WorkerDiagnosticV1ErrorCode::RequestInProgress => ErrorCode::RequestInProgress,
            WorkerDiagnosticV1ErrorCode::RequestIdConflict => ErrorCode::RequestIdConflict,
            WorkerDiagnosticV1ErrorCode::RunNotFound => ErrorCode::RunNotFound,
            WorkerDiagnosticV1ErrorCode::RunCorrupt => ErrorCode::RunCorrupt,
            WorkerDiagnosticV1ErrorCode::ContextBudgetInvalid => ErrorCode::ContextBudgetInvalid,
            WorkerDiagnosticV1ErrorCode::NotImplemented => ErrorCode::NotImplemented,
            WorkerDiagnosticV1ErrorCode::RustCoreCrashed => ErrorCode::RustCoreCrashed,
            WorkerDiagnosticV1ErrorCode::InternalError => ErrorCode::InternalError,
        },
        message: worker.message.clone(),
        retryable: worker.retryable,
        stage: match worker.stage {
            WorkerDiagnosticV1Stage::Request => DiagnosticStage::Request,
            WorkerDiagnosticV1Stage::Discovery => DiagnosticStage::Discovery,
            WorkerDiagnosticV1Stage::Cache => DiagnosticStage::Cache,
            WorkerDiagnosticV1Stage::Parse => DiagnosticStage::Parse,
            WorkerDiagnosticV1Stage::Context => DiagnosticStage::Context,
            WorkerDiagnosticV1Stage::Process => DiagnosticStage::Process,
            WorkerDiagnosticV1Stage::Doctor => DiagnosticStage::Doctor,
            WorkerDiagnosticV1Stage::Inspect => DiagnosticStage::Inspect,
            WorkerDiagnosticV1Stage::Internal => DiagnosticStage::Internal,
        },
        file_path: worker.file_path.clone(),
        backend: worker.backend.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub base_args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub expected_kind: WorkerKind,
    pub required_backends: Vec<String>,
    pub required_extensions: Vec<String>,
}

impl WorkerCommand {
    fn args_for(&self, operation: &str) -> Vec<OsString> {
        let mut args = Vec::with_capacity(self.base_args.len() + 2);
        if matches!(operation, "hello")
            && self.expected_kind == WorkerKind::PythonDocument
            && !self.base_args.iter().any(|value| value == "-S")
        {
            args.push(OsString::from("-S"));
        }
        args.extend(self.base_args.iter().cloned());
        args.push(OsString::from(operation));
        args
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredWorker {
    pub command: WorkerCommand,
    pub identity: WorkerVersionResponse,
    pub rss_tracker: Option<WorkerRssTracker>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerRegistry {
    pub office: Option<RegisteredWorker>,
    pub python_document: Option<RegisteredWorker>,
}

pub fn preflight_workers(
    profile: &NormalizedScannerProfileV1,
    adapters: &AdapterPaths,
    timeout: Duration,
) -> Result<WorkerRegistry, ParseFailure> {
    let needs = required_worker_set(profile);
    let office_command = needs.office.then(|| office::worker_command(adapters));
    let python_command = needs
        .python_document
        .then(|| document::worker_command(adapters));
    preflight_worker_commands(office_command.as_ref(), python_command.as_ref(), timeout)
}

/// Runs the continuation only after all route-relevant worker identities have
/// been frozen. Task 7 uses this boundary before its first cache lookup.
pub fn preflight_then<T>(
    profile: &NormalizedScannerProfileV1,
    adapters: &AdapterPaths,
    timeout: Duration,
    continuation: impl FnOnce(&WorkerRegistry) -> T,
) -> Result<T, ParseFailure> {
    let registry = preflight_workers(profile, adapters, timeout)?;
    Ok(continuation(&registry))
}

/// Injectable form used by fault tests and Task 7 cache-boundary tests.
pub fn preflight_commands_then<T>(
    office_command: Option<&WorkerCommand>,
    python_command: Option<&WorkerCommand>,
    timeout: Duration,
    continuation: impl FnOnce(&WorkerRegistry) -> T,
) -> Result<T, ParseFailure> {
    let registry = preflight_worker_commands(office_command, python_command, timeout)?;
    Ok(continuation(&registry))
}

fn preflight_worker_commands(
    office_command: Option<&WorkerCommand>,
    python_command: Option<&WorkerCommand>,
    timeout: Duration,
) -> Result<WorkerRegistry, ParseFailure> {
    let (office, python_document) = match (office_command, python_command) {
        (Some(office), Some(python)) => {
            let (office_result, python_result) = register_worker_pair(office, python, timeout);
            (Some(office_result?), Some(python_result?))
        }
        (office, python) => (
            office
                .map(|command| register_worker(command, timeout))
                .transpose()?,
            python
                .map(|command| register_worker(command, timeout))
                .transpose()?,
        ),
    };
    Ok(WorkerRegistry {
        office,
        python_document,
    })
}

/// Runs the independent Office and Python version handshakes concurrently.
pub fn register_worker_pair(
    office_command: &WorkerCommand,
    python_command: &WorkerCommand,
    timeout: Duration,
) -> (
    Result<RegisteredWorker, ParseFailure>,
    Result<RegisteredWorker, ParseFailure>,
) {
    register_worker_pair_inner(office_command, python_command, timeout, None)
}

pub(crate) fn register_worker_pair_observed(
    office_command: &WorkerCommand,
    python_command: &WorkerCommand,
    timeout: Duration,
    rss_tracker: &WorkerRssTracker,
) -> (
    Result<RegisteredWorker, ParseFailure>,
    Result<RegisteredWorker, ParseFailure>,
) {
    register_worker_pair_inner(office_command, python_command, timeout, Some(rss_tracker))
}

fn register_worker_pair_inner(
    office_command: &WorkerCommand,
    python_command: &WorkerCommand,
    timeout: Duration,
    rss_tracker: Option<&WorkerRssTracker>,
) -> (
    Result<RegisteredWorker, ParseFailure>,
    Result<RegisteredWorker, ParseFailure>,
) {
    std::thread::scope(|scope| {
        let office_task =
            scope.spawn(|| register_worker_inner(office_command, timeout, rss_tracker));
        let python_result = register_worker_inner(python_command, timeout, rss_tracker);
        let office_result = office_task.join().unwrap_or_else(|_| {
            Err(contract_failure(
                ErrorCode::WorkerHandshakeFailed,
                "Office worker handshake thread failed",
                None,
                None,
                DiagnosticStage::Process,
            ))
        });
        (office_result, python_result)
    })
}

pub fn register_worker(
    command: &WorkerCommand,
    timeout: Duration,
) -> Result<RegisteredWorker, ParseFailure> {
    register_worker_inner(command, timeout, None)
}

fn register_worker_inner(
    command: &WorkerCommand,
    timeout: Duration,
    rss_tracker: Option<&WorkerRssTracker>,
) -> Result<RegisteredWorker, ParseFailure> {
    let spec = ProcessSpec {
        program: command.program.clone(),
        args: command.args_for("hello"),
        current_dir: command.current_dir.clone(),
        stdin: Vec::new(),
        timeout,
        capture_limit: WORKER_HANDSHAKE_CAPTURE_LIMIT,
        rss_tracker: rss_tracker.cloned(),
    };
    let output = run_process(&spec).map_err(|error| process_failure(error, None, None, true))?;
    if output.exit_code != 0 {
        return Err(contract_failure(
            ErrorCode::WorkerHandshakeFailed,
            "worker version command returned nonzero",
            None,
            None,
            DiagnosticStage::Process,
        ));
    }
    let hello: ai_daily_worker_contract::WorkerHello = serde_json::from_slice(&output.stdout)
        .map_err(|_| {
            contract_failure(
                ErrorCode::WorkerHandshakeFailed,
                "worker version stdout is not one strict JSON response",
                None,
                None,
                DiagnosticStage::Process,
            )
        })?;
    hello.validate().map_err(|_| {
        contract_failure(
            ErrorCode::WorkerHandshakeFailed,
            "worker version response violates the strict contract",
            None,
            None,
            DiagnosticStage::Process,
        )
    })?;
    if hello.worker_kind != worker_kind_v2(command.expected_kind)
        || hello.worker_contract_version != WORKER_CONTRACT_VERSION
        || hello.worker_version != env!("CARGO_PKG_VERSION")
        || !supports_required_operations(&hello, command)
    {
        return Err(contract_failure(
            ErrorCode::WorkerVersionMismatch,
            "worker version identity or capabilities mismatch",
            None,
            None,
            DiagnosticStage::Process,
        ));
    }
    let identity = WorkerVersionResponse {
        contract: "ai_daily_worker".to_string(),
        protocol_version: 1,
        worker_kind: command.expected_kind,
        worker_contract_version: hello.worker_contract_version,
        worker_version: hello.worker_version,
        worker_build: hello.worker_build,
        supported_backends: command.required_backends.clone(),
        supported_extensions: command.required_extensions.clone(),
    };
    Ok(RegisteredWorker {
        command: command.clone(),
        identity,
        rss_tracker: rss_tracker.cloned(),
    })
}

/// Shared pre-dispatch validation for worker-v2 sessions.
pub(crate) fn validate_worker_request(
    worker: &RegisteredWorker,
    request: &WorkerParseRequest,
) -> Result<(), ParseFailure> {
    request.validate().map_err(|_| {
        contract_failure(
            ErrorCode::ParserInvalidPayload,
            "worker request violates the strict contract",
            Some(&request.file_path),
            Some(request.backend.as_str()),
            DiagnosticStage::Parse,
        )
    })?;
    if !worker
        .command
        .required_backends
        .iter()
        .any(|backend| backend == request.backend.as_str())
        || !worker
            .command
            .required_extensions
            .iter()
            .any(|extension| extension == &request.file_type)
    {
        return Err(contract_failure(
            ErrorCode::WorkerVersionMismatch,
            "preflight worker capabilities do not cover the parse route",
            Some(&request.file_path),
            Some(request.backend.as_str()),
            DiagnosticStage::Parse,
        ));
    }
    validate_worker_source_before(request)
}

fn worker_kind_v2(kind: WorkerKind) -> ai_daily_worker_contract::WorkerKind {
    match kind {
        WorkerKind::Office => ai_daily_worker_contract::WorkerKind::Office,
        WorkerKind::PythonDocument => ai_daily_worker_contract::WorkerKind::PythonDocument,
    }
}

fn supports_required_operations(
    identity: &ai_daily_worker_contract::WorkerHello,
    command: &WorkerCommand,
) -> bool {
    use ai_daily_worker_contract::WorkerOperation;
    let required: &[WorkerOperation] = match command.expected_kind {
        WorkerKind::Office => &[WorkerOperation::OfficeParse],
        WorkerKind::PythonDocument => &[
            WorkerOperation::PdfClassify,
            WorkerOperation::PdfParse,
            WorkerOperation::PythonOfficeParse,
            WorkerOperation::PythonSharepointParse,
        ],
    };
    required
        .iter()
        .all(|operation| identity.supported_operations.contains(operation))
}

pub fn worker_hello_from_registered(
    worker: &RegisteredWorker,
) -> ai_daily_worker_contract::WorkerHello {
    let supported_operations = match worker.command.expected_kind {
        WorkerKind::Office => vec![ai_daily_worker_contract::WorkerOperation::OfficeParse],
        WorkerKind::PythonDocument => vec![
            ai_daily_worker_contract::WorkerOperation::PdfClassify,
            ai_daily_worker_contract::WorkerOperation::PdfParse,
            ai_daily_worker_contract::WorkerOperation::PythonOfficeParse,
            ai_daily_worker_contract::WorkerOperation::PythonSharepointParse,
        ],
    };
    ai_daily_worker_contract::WorkerHello {
        contract: ai_daily_worker_contract::CONTRACT.to_string(),
        protocol_version: ai_daily_worker_contract::PROTOCOL_VERSION,
        frame: "hello".to_string(),
        worker_contract_version: worker.identity.worker_contract_version.clone(),
        worker_kind: worker_kind_v2(worker.command.expected_kind),
        worker_version: worker.identity.worker_version.clone(),
        worker_build: worker.identity.worker_build.clone(),
        supported_operations,
    }
}

/// Validates a worker-v2 session response and applies source, identity, and
/// domain-error mapping.
pub(crate) fn validate_session_worker_response(
    worker: &RegisteredWorker,
    request: &WorkerParseRequest,
    response: WorkerParseResponse,
) -> Result<WorkerParseResponse, ParseFailure> {
    response.validate().map_err(|_| {
        contract_failure(
            ErrorCode::ParserInvalidPayload,
            "worker response violates the strict contract",
            Some(&request.file_path),
            Some(request.backend.as_str()),
            DiagnosticStage::Parse,
        )
    })?;
    let synthetic_exit_code = if response.status == WorkerStatus::Ok {
        0
    } else {
        1
    };
    finish_worker_response(worker, request, response, synthetic_exit_code)
}

fn finish_worker_response(
    worker: &RegisteredWorker,
    request: &WorkerParseRequest,
    response: WorkerParseResponse,
    exit_code: u32,
) -> Result<WorkerParseResponse, ParseFailure> {
    validate_worker_source_after(request, &response)?;
    validate_response_identity(worker, request, &response, exit_code)?;

    if let Some(error) = response.error.0.clone() {
        let class = match error.error_code {
            WorkerDiagnosticV1ErrorCode::ParserInvalidPayload
            | WorkerDiagnosticV1ErrorCode::WorkerVersionMismatch
            | WorkerDiagnosticV1ErrorCode::WorkerBuildChanged => FailureClass::ContractFailure,
            WorkerDiagnosticV1ErrorCode::ParserStartFailed => FailureClass::EnvironmentUnavailable,
            WorkerDiagnosticV1ErrorCode::ParserTimeout
            | WorkerDiagnosticV1ErrorCode::FileTooLarge
            | WorkerDiagnosticV1ErrorCode::SourceVersionChanged => FailureClass::Deterministic,
            _ if error.retryable => FailureClass::RecoverableParserFailure,
            _ => FailureClass::Deterministic,
        };
        return Err(ParseFailure {
            class,
            // Adapter seam (spec Part 7.1): frozen worker diagnostic -> scanner Diagnostic.
            diagnostic: worker_diagnostic_to_scanner(&error),
        });
    }
    Ok(response)
}

fn validate_response_identity(
    worker: &RegisteredWorker,
    request: &WorkerParseRequest,
    response: &WorkerParseResponse,
    exit_code: u32,
) -> Result<(), ParseFailure> {
    let expected_exit = if response.status == WorkerStatus::Ok {
        0
    } else {
        1
    };
    let source_matches = response.observed_source_version == request.expected_source_version
        || response.error.0.as_ref().is_some_and(|error| {
            error.error_code == WorkerDiagnosticV1ErrorCode::SourceVersionChanged
        });
    let error_identity_matches = response.error.0.as_ref().is_none_or(|error| {
        error.stage == WorkerDiagnosticV1Stage::Parse
            && error.file_path.0.as_deref() == Some(request.file_path.as_str())
            && error.backend.0.as_deref() == Some(request.backend.as_str())
    });
    let warnings_match = response.warnings.iter().all(|warning| {
        warning.stage == WorkerDiagnosticV1Stage::Parse
            && warning
                .file_path
                .0
                .as_deref()
                .is_none_or(|path| path == request.file_path)
            && warning
                .backend
                .0
                .as_deref()
                .is_none_or(|backend| backend == request.backend.as_str())
    });
    let content_within_budget =
        response.content.chars().count() as u64 <= worker_response_character_budget(request);
    if exit_code != expected_exit
        || response.request_id != request.request_id
        || response.file_path != request.file_path
        || response.file_type != request.file_type
        || response.parser_backend != request.backend
        || response.worker_lane != request.backend.lane()
        || response.worker_contract_version != worker.identity.worker_contract_version
        || response.worker_version != worker.identity.worker_version
        || response.duration_ms > request.remaining_timeout_ms
        || !content_within_budget
        || !source_matches
        || !error_identity_matches
        || !warnings_match
    {
        return Err(contract_failure(
            ErrorCode::ParserInvalidPayload,
            "worker response identity, route, source, or exit status mismatch",
            Some(&request.file_path),
            Some(request.backend.as_str()),
            DiagnosticStage::Parse,
        ));
    }
    if response.worker_build != worker.identity.worker_build {
        return Err(contract_failure(
            ErrorCode::WorkerBuildChanged,
            "worker build changed after preflight",
            Some(&request.file_path),
            Some(request.backend.as_str()),
            DiagnosticStage::Parse,
        ));
    }
    Ok(())
}

fn worker_response_character_budget(request: &WorkerParseRequest) -> u64 {
    match &request.parser_limits {
        WorkerParserLimits::Office {
            document_excerpt_max_chars,
            ..
        } => *document_excerpt_max_chars,
        WorkerParserLimits::Pdf {
            excerpt_max_chars, ..
        }
        | WorkerParserLimits::SharepointText { excerpt_max_chars } => *excerpt_max_chars,
    }
}

pub(crate) fn worker_response_capture_limit(
    request: &WorkerParseRequest,
) -> Result<usize, ParseFailure> {
    // JSON permits one Unicode scalar to be represented by a surrogate pair
    // (12 ASCII bytes). Diagnostics may repeat only the request path because
    // response identity validation rejects any other path.
    let path_chars = request.file_path.chars().count() as u64;
    let maximum_scalar_count = worker_response_character_budget(request)
        .checked_add(path_chars.saturating_mul(MAX_DIAGNOSTIC_COUNT + 1))
        .and_then(|value| {
            value.checked_add(MAX_DIAGNOSTIC_MESSAGE_CHARS.saturating_mul(MAX_DIAGNOSTIC_COUNT))
        })
        .and_then(|value| value.checked_add(MAX_WORKER_IDENTITY_CHARS))
        .ok_or_else(|| {
            contract_failure(
                ErrorCode::ParserInvalidPayload,
                "worker response budget overflowed",
                Some(&request.file_path),
                Some(request.backend.as_str()),
                DiagnosticStage::Parse,
            )
        })?;
    let byte_limit = maximum_scalar_count
        .checked_mul(MAX_JSON_BYTES_PER_SCALAR)
        .and_then(|value| value.checked_add(RESPONSE_JSON_FIXED_ALLOWANCE))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            contract_failure(
                ErrorCode::ParserInvalidPayload,
                "worker response budget is unsupported on this platform",
                Some(&request.file_path),
                Some(request.backend.as_str()),
                DiagnosticStage::Parse,
            )
        })?;
    Ok(byte_limit)
}

fn process_failure(
    error: ProcessError,
    file_path: Option<&str>,
    backend: Option<&str>,
    handshake: bool,
) -> ParseFailure {
    let (class, code, message, retryable) = match error {
        ProcessError::StartFailed | ProcessError::ContainmentFailed => (
            FailureClass::EnvironmentUnavailable,
            if handshake {
                ErrorCode::WorkerHandshakeFailed
            } else {
                ErrorCode::ParserStartFailed
            },
            if error == ProcessError::ContainmentFailed {
                "worker process-tree containment failed"
            } else {
                "worker process could not be started"
            },
            true,
        ),
        ProcessError::TimedOut => (
            FailureClass::Deterministic,
            if handshake {
                ErrorCode::WorkerHandshakeFailed
            } else {
                ErrorCode::ParserTimeout
            },
            "worker process exceeded its deadline",
            false,
        ),
        ProcessError::IoFailed | ProcessError::OutputTooLarge => (
            FailureClass::ContractFailure,
            if handshake {
                ErrorCode::WorkerHandshakeFailed
            } else {
                ErrorCode::ParserInvalidPayload
            },
            "worker process transport failed",
            false,
        ),
    };
    ParseFailure {
        class,
        diagnostic: Diagnostic {
            error_code: code,
            message: message.to_string(),
            retryable,
            stage: DiagnosticStage::Process,
            file_path: Nullable(file_path.map(str::to_string)),
            backend: Nullable(backend.map(str::to_string)),
        },
    }
}

fn parser_failure(
    class: FailureClass,
    code: ErrorCode,
    message: &str,
    retryable: bool,
    request: &WorkerParseRequest,
) -> ParseFailure {
    ParseFailure {
        class,
        diagnostic: Diagnostic {
            error_code: code,
            message: message.to_string(),
            retryable,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some(request.file_path.clone())),
            backend: Nullable(Some(request.backend.as_str().to_string())),
        },
    }
}

fn validate_worker_source_before(request: &WorkerParseRequest) -> Result<(), ParseFailure> {
    let (source_version, size_bytes) = current_source(&request.file_path).map_err(|_| {
        parser_failure(
            FailureClass::Deterministic,
            ErrorCode::ParserFailed,
            "file metadata is unavailable before worker start",
            false,
            request,
        )
    })?;
    if size_bytes > request.max_file_size_bytes {
        return Err(parser_failure(
            FailureClass::Deterministic,
            ErrorCode::FileTooLarge,
            "file exceeds the configured size limit",
            false,
            request,
        ));
    }
    if source_version != request.expected_source_version {
        return Err(parser_failure(
            FailureClass::Deterministic,
            ErrorCode::SourceVersionChanged,
            "file source version changed before worker start",
            false,
            request,
        ));
    }
    Ok(())
}

fn validate_worker_source_after(
    request: &WorkerParseRequest,
    response: &WorkerParseResponse,
) -> Result<(), ParseFailure> {
    let (source_version, size_bytes) = current_source(&request.file_path).map_err(|_| {
        parser_failure(
            FailureClass::Deterministic,
            ErrorCode::SourceVersionChanged,
            "file source became unavailable during worker parsing",
            false,
            request,
        )
    })?;
    if size_bytes > request.max_file_size_bytes {
        return Err(parser_failure(
            FailureClass::Deterministic,
            ErrorCode::FileTooLarge,
            "file exceeded the configured size limit during worker parsing",
            false,
            request,
        ));
    }
    if source_version != request.expected_source_version {
        return Err(parser_failure(
            FailureClass::Deterministic,
            ErrorCode::SourceVersionChanged,
            "file source version changed during worker parsing",
            false,
            request,
        ));
    }
    if source_version != response.observed_source_version {
        return Err(parser_failure(
            FailureClass::ContractFailure,
            ErrorCode::ParserInvalidPayload,
            "worker observed source version does not match current metadata",
            false,
            request,
        ));
    }
    Ok(())
}

pub(crate) fn current_source(file_path: &str) -> Result<(String, u64), ()> {
    let metadata = std::fs::metadata(file_path).map_err(|_| ())?;
    let modified = metadata.modified().map_err(|_| ())?;
    let duration = modified.duration_since(UNIX_EPOCH).map_err(|_| ())?;
    Ok((
        ai_daily_discovery::build_source_version(duration.as_nanos(), metadata.len()),
        metadata.len(),
    ))
}

fn contract_failure(
    code: ErrorCode,
    message: &str,
    file_path: Option<&str>,
    backend: Option<&str>,
    stage: DiagnosticStage,
) -> ParseFailure {
    ParseFailure {
        class: FailureClass::ContractFailure,
        diagnostic: Diagnostic {
            error_code: code,
            message: message.to_string(),
            retryable: false,
            stage,
            file_path: Nullable(file_path.map(str::to_string)),
            backend: Nullable(backend.map(str::to_string)),
        },
    }
}

pub(crate) fn worker_request(
    file: &ai_daily_discovery::DiscoveredFileOut,
    route: ParserRoute,
    timeout_ms: u64,
    profile: &NormalizedScannerProfileV1,
) -> WorkerParseRequest {
    let backend = match route {
        ParserRoute::RustOffice => WorkerBackend::RustOfficeOxideV2,
        ParserRoute::RustXlsx => WorkerBackend::RustXlsxBoundedV2,
        ParserRoute::Pdf => WorkerBackend::PythonPdfTextV2,
        ParserRoute::PythonOffice => WorkerBackend::PythonOfficeV2,
        ParserRoute::PythonSharepointText => WorkerBackend::PythonSharepointTextV2,
        ParserRoute::LightText => unreachable!("light text does not use a worker request"),
    };
    let parser_limits = match route {
        ParserRoute::Pdf => WorkerParserLimits::Pdf {
            max_pages: profile.parse.pdf.max_pages,
            excerpt_max_chars: profile.parse.pdf.excerpt_max_chars,
        },
        ParserRoute::PythonSharepointText => WorkerParserLimits::SharepointText {
            excerpt_max_chars: profile.parse.office.document_excerpt_max_chars,
        },
        _ => WorkerParserLimits::Office {
            excel_max_sheets: profile.parse.office.excel_max_sheets,
            excel_max_rows: profile.parse.office.excel_max_rows,
            excel_max_columns: profile.parse.office.excel_max_columns,
            docx_max_paragraphs: profile.parse.office.docx_max_paragraphs,
            docx_max_tables: profile.parse.office.docx_max_tables,
            docx_table_max_rows: profile.parse.office.docx_table_max_rows,
            docx_table_max_cols: profile.parse.office.docx_table_max_cols,
            pptx_max_slides: profile.parse.office.pptx_max_slides,
            pptx_include_notes: profile.parse.office.pptx_include_notes,
            document_excerpt_max_chars: profile.parse.office.document_excerpt_max_chars,
        },
    };
    WorkerParseRequest {
        contract: "ai_daily_worker".to_string(),
        protocol_version: 1,
        request_id: next_request_id(),
        file_path: file.path.clone(),
        file_type: file.extension.clone(),
        backend,
        remaining_timeout_ms: timeout_ms,
        max_file_size_bytes: profile.execution.max_file_size_bytes,
        parser_limits,
        expected_source_version: file.source_version.clone(),
    }
}

fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let mut bytes = (nanos ^ (u128::from(std::process::id()) << 64) ^ counter).to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RequiredWorkerSet {
    office: bool,
    python_document: bool,
}

fn required_worker_set(profile: &NormalizedScannerProfileV1) -> RequiredWorkerSet {
    let allows = |extension: &str| {
        profile
            .discovery
            .allowed_extensions
            .iter()
            .any(|value| value == extension)
    };
    let modern_office = [".docx", ".pptx", ".xlsx"].into_iter().any(allows);
    let rust_primary = profile.parse.office.primary_backend == "rust_office_oxide_v2";
    let python_primary = profile.parse.office.primary_backend == "python_office_v2";
    let modern_python_fallback =
        rust_primary
            && profile.parse.office.fallback_enabled
            && profile.parse.office.fallback_order.iter().any(|backend| {
                *backend == ai_daily_scanner_contract::FallbackBackend::PythonOfficeV2
            });
    let legacy_document = allows(".xls")
        || (profile.parse.office.legacy_extensions_enabled && (allows(".doc") || allows(".ppt")));
    RequiredWorkerSet {
        office: modern_office && rust_primary,
        python_document: allows(".pdf")
            || legacy_document
            || (modern_office && (python_primary || modern_python_fallback)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::normalize_scanner_profile;
    use ai_daily_scanner_contract::{RawScannerProfileV1, ReportMode};

    #[test]
    fn frozen_default_requires_both_worker_fingerprints() {
        let raw: RawScannerProfileV1 =
            serde_json::from_str(r#"{"schema_version":"scanner_profile_v1"}"#)
                .expect("minimal profile should parse");
        let profile = normalize_scanner_profile(&raw, ReportMode::Daily)
            .expect("default profile should normalize");

        assert_eq!(
            required_worker_set(&profile),
            RequiredWorkerSet {
                office: true,
                python_document: true,
            }
        );
    }

    #[test]
    fn python_hello_skips_site_startup_but_session_uses_runtime_environment() {
        let command = WorkerCommand {
            program: PathBuf::from("python"),
            base_args: vec![
                OsString::from("-m"),
                OsString::from("src.workers.document_parser_worker"),
            ],
            current_dir: None,
            expected_kind: WorkerKind::PythonDocument,
            required_backends: Vec::new(),
            required_extensions: Vec::new(),
        };

        assert_eq!(
            command.args_for("hello").first(),
            Some(&OsString::from("-S"))
        );
        assert_ne!(
            command.args_for("session").first(),
            Some(&OsString::from("-S"))
        );
    }
}
