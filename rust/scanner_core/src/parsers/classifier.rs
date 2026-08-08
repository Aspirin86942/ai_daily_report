//! Local PDF classifier port（spec Part 7.1）。
//!
//! 生产实现通过 ``classify-pdf`` one-shot 子进程调用 Python worker；测试
//! adapter 返回内存结果。trait 返回 typed ``PdfClassifierResultV1``（含
//! text/no-text/unknown/error 四态），``Err(ParseFailure)`` 只表示进程/传输
//! 层失败（timeout/crash/坏响应/外层 error），由调度器映射为 ``unknown``。

use std::time::Duration;

use ai_daily_scanner_contract::{
    ClassifierResponseStatus, Diagnostic, DiagnosticStage, ErrorCode, Nullable,
    PdfClassifierRequestV1, PdfClassifierResponseV1, PdfClassifierResultV1,
    PythonOperationDiagnosticV1, PythonOperationErrorCode, PythonOperationStage, Validate,
};

use crate::fallback::{FailureClass, ParseFailure};
use crate::process::{run_process, ProcessError, ProcessSpec};

use super::{contract_failure, current_source, WorkerCommand};

pub const CLASSIFIER_CAPTURE_LIMIT: usize = 1024 * 1024;

/// 本地可替换 PDF 分类 adapter。生产为进程 one-shot，测试为内存结果。
pub trait PdfClassifierPort: Send + Sync {
    fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        timeout: Duration,
    ) -> Result<PdfClassifierResultV1, ParseFailure>;
}

/// 生产实现：调用 Python worker 的 ``classify-pdf`` one-shot。
#[derive(Debug, Clone)]
pub struct ClassifierPort {
    command: WorkerCommand,
}

impl ClassifierPort {
    pub fn new(command: WorkerCommand) -> Self {
        Self { command }
    }
}

impl PdfClassifierPort for ClassifierPort {
    fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        timeout: Duration,
    ) -> Result<PdfClassifierResultV1, ParseFailure> {
        classify_pdf_oneshot(&self.command, request, timeout)
    }
}

/// 一次严格 ``classify-pdf`` one-shot 进程调用（spec Part 7.1/7.3）。
pub fn classify_pdf_oneshot(
    command: &WorkerCommand,
    request: &PdfClassifierRequestV1,
    timeout: Duration,
) -> Result<PdfClassifierResultV1, ParseFailure> {
    request.validate().map_err(|_| {
        contract_failure(
            ErrorCode::ParserInvalidPayload,
            "classifier request violates the strict contract",
            Some(&request.file_path),
            None,
            DiagnosticStage::Parse,
        )
    })?;
    validate_classifier_source_before(request)?;
    let stdin = serde_json::to_vec(request).map_err(|_| {
        contract_failure(
            ErrorCode::ParserInvalidPayload,
            "classifier request could not be serialized",
            Some(&request.file_path),
            None,
            DiagnosticStage::Parse,
        )
    })?;
    let spec = ProcessSpec {
        program: command.program.clone(),
        args: command.args_for("classify-pdf"),
        current_dir: command.current_dir.clone(),
        stdin,
        timeout,
        capture_limit: CLASSIFIER_CAPTURE_LIMIT,
    };
    let output = run_process(&spec)
        .map_err(|error| classifier_process_failure(error, &request.file_path))?;
    if output.exit_code > 2 {
        return Err(contract_failure(
            ErrorCode::ParserFailed,
            "classifier process crashed before completing its response",
            Some(&request.file_path),
            None,
            DiagnosticStage::Process,
        ));
    }
    if output.exit_code == 2 {
        return Err(contract_failure(
            ErrorCode::ParserInvalidPayload,
            "classifier rejected a validated request",
            Some(&request.file_path),
            None,
            DiagnosticStage::Process,
        ));
    }
    let response: PdfClassifierResponseV1 = serde_json::from_slice(&output.stdout).map_err(|_| {
        contract_failure(
            ErrorCode::ParserInvalidPayload,
            "classifier stdout is not one strict JSON response",
            Some(&request.file_path),
            None,
            DiagnosticStage::Process,
        )
    })?;
    response.validate().map_err(|_| {
        contract_failure(
            ErrorCode::ParserInvalidPayload,
            "classifier response violates the strict contract",
            Some(&request.file_path),
            None,
            DiagnosticStage::Process,
        )
    })?;
    let expected_exit = if response.status == ClassifierResponseStatus::Ok {
        0
    } else {
        1
    };
    if output.exit_code != expected_exit || response.request_id != request.request_id {
        return Err(contract_failure(
            ErrorCode::ParserInvalidPayload,
            "classifier response identity or exit status mismatch",
            Some(&request.file_path),
            None,
            DiagnosticStage::Process,
        ));
    }
    validate_classifier_source_after(request)?;
    match response.status {
        ClassifierResponseStatus::Ok => response.result.0.ok_or_else(|| {
            contract_failure(
                ErrorCode::ParserInvalidPayload,
                "ok classifier response is missing its typed result",
                Some(&request.file_path),
                None,
                DiagnosticStage::Process,
            )
        }),
        ClassifierResponseStatus::Error => {
            let diagnostic = response.error.0.ok_or_else(|| {
                contract_failure(
                    ErrorCode::ParserInvalidPayload,
                    "error classifier response is missing its diagnostic",
                    Some(&request.file_path),
                    None,
                    DiagnosticStage::Process,
                )
            })?;
            Err(classifier_diagnostic_failure(
                &diagnostic,
                &request.file_path,
            ))
        }
    }
}

/// 把 Python operation diagnostic 翻译为 scanner-side Diagnostic（spec Part 7.1
/// adapter seam；PythonOperationErrorCode/Stage 不会扩散到共享 Diagnostic）。
fn classifier_diagnostic_failure(
    diagnostic: &PythonOperationDiagnosticV1,
    file_path: &str,
) -> ParseFailure {
    let code = match diagnostic.error_code {
        PythonOperationErrorCode::InvalidRequest => ErrorCode::InvalidRequest,
        PythonOperationErrorCode::ParserStartFailed => ErrorCode::ParserStartFailed,
        PythonOperationErrorCode::ParserTimeout => ErrorCode::ParserTimeout,
        PythonOperationErrorCode::ParserInvalidPayload => ErrorCode::ParserInvalidPayload,
        PythonOperationErrorCode::ParserFailed => ErrorCode::ParserFailed,
        PythonOperationErrorCode::SourceVersionChanged => ErrorCode::SourceVersionChanged,
        PythonOperationErrorCode::InternalError => ErrorCode::InternalError,
    };
    let stage = match diagnostic.stage {
        PythonOperationStage::Request => DiagnosticStage::Request,
        PythonOperationStage::Parse => DiagnosticStage::Parse,
        PythonOperationStage::Process => DiagnosticStage::Process,
    };
    ParseFailure {
        class: if diagnostic.retryable {
            FailureClass::RecoverableParserFailure
        } else {
            FailureClass::Deterministic
        },
        diagnostic: Diagnostic {
            error_code: code,
            message: diagnostic.message.clone(),
            retryable: diagnostic.retryable,
            stage,
            file_path: diagnostic
                .file_path
                .0
                .clone()
                .map_or(Nullable(Some(file_path.to_string())), |value| {
                    Nullable(Some(value))
                }),
            backend: diagnostic
                .backend
                .0
                .clone()
                .map_or(Nullable(None), |value| Nullable(Some(value))),
        },
    }
}

fn classifier_process_failure(error: ProcessError, file_path: &str) -> ParseFailure {
    let (class, code, message, retryable) = match error {
        ProcessError::StartFailed | ProcessError::ContainmentFailed => (
            FailureClass::EnvironmentUnavailable,
            ErrorCode::ParserStartFailed,
            if error == ProcessError::ContainmentFailed {
                "classifier process-tree containment failed"
            } else {
                "classifier process could not be started"
            },
            true,
        ),
        // spec Part 3.2: a classifier killed by the per-file timeout or the
        // remaining work-deadline is an `unknown` classification with
        // retryable=true; the scheduler maps that unknown to Timeout/Error.
        ProcessError::TimedOut => (
            FailureClass::RecoverableParserFailure,
            ErrorCode::ParserTimeout,
            "classifier process exceeded its deadline",
            true,
        ),
        ProcessError::IoFailed | ProcessError::OutputTooLarge => (
            FailureClass::ContractFailure,
            ErrorCode::ParserInvalidPayload,
            "classifier process transport failed",
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
            file_path: Nullable(Some(file_path.to_string())),
            backend: Nullable(None),
        },
    }
}

fn validate_classifier_source_before(
    request: &PdfClassifierRequestV1,
) -> Result<(), ParseFailure> {
    let (source_version, _) = current_source(&request.file_path).map_err(|_| {
        classifier_source_failure(
            request,
            "file metadata is unavailable before classifier start",
        )
    })?;
    if source_version != request.source_version {
        return Err(classifier_source_failure(
            request,
            "file source version changed before classifier start",
        ));
    }
    Ok(())
}

fn validate_classifier_source_after(request: &PdfClassifierRequestV1) -> Result<(), ParseFailure> {
    let (source_version, _) = current_source(&request.file_path).map_err(|_| {
        classifier_source_failure(request, "file source became unavailable during classification")
    })?;
    if source_version != request.source_version {
        return Err(classifier_source_failure(
            request,
            "file source version changed during classification",
        ));
    }
    Ok(())
}

fn classifier_source_failure(request: &PdfClassifierRequestV1, message: &str) -> ParseFailure {
    ParseFailure {
        class: FailureClass::Deterministic,
        diagnostic: Diagnostic {
            error_code: ErrorCode::SourceVersionChanged,
            message: message.to_string(),
            retryable: false,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some(request.file_path.clone())),
            backend: Nullable(None),
        },
    }
}
