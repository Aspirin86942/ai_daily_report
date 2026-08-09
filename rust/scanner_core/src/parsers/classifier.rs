//! Local PDF classifier port（spec Part 7.1）。
//!
//! 生产实现通过 ``classify-pdf`` one-shot 子进程调用 Python worker；测试
//! adapter 返回内存结果。trait 返回 typed ``PdfClassifierResultV1``（含
//! text/no-text/unknown/error 四态），``Err(ParseFailure)`` 只表示进程/传输
//! 层失败（timeout/crash/坏响应/外层 error），由调度器映射为 ``unknown``。

use std::sync::Arc;
use std::time::Duration;

use ai_daily_scanner_contract::{
    ClassificationTransport, ClassifierResponseStatus, Diagnostic, DiagnosticStage, ErrorCode,
    Nullable, PdfClassifierRequestV1, PdfClassifierResponseV1, PdfClassifierResultV1,
    PythonOperationDiagnosticV1, PythonOperationErrorCode, PythonOperationStage, Validate,
};

use crate::fallback::{FailureClass, ParseFailure};
use crate::process::{run_process_observed, ProcessError, ProcessSpec, WorkerRssTracker};

use super::{contract_failure, current_source, OneShotExecution, WorkerCommand};

pub const CLASSIFIER_CAPTURE_LIMIT: usize = 1024 * 1024;

/// 本地可替换 PDF 分类 adapter。生产为进程 one-shot，测试为内存结果。
pub trait PdfClassifierPort: Send + Sync {
    fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        timeout: Duration,
    ) -> PdfClassifierExecution;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfClassifierExecution {
    pub outcome: Result<PdfClassifierResultV1, ParseFailure>,
    pub transport: ClassificationTransport,
    pub attempt_count: u64,
    pub duration_ms: u64,
}

impl PdfClassifierExecution {
    pub fn test_oneshot(outcome: Result<PdfClassifierResultV1, ParseFailure>) -> Self {
        Self {
            outcome,
            transport: ClassificationTransport::OneShot,
            attempt_count: 1,
            duration_ms: 0,
        }
    }
}

/// 生产实现：调用 Python worker 的 ``classify-pdf`` one-shot。
#[derive(Clone)]
pub struct ClassifierPort {
    command: WorkerCommand,
    session: Option<Arc<crate::session::PythonSessionPool>>,
    rss_tracker: Option<WorkerRssTracker>,
}

impl ClassifierPort {
    pub fn new(command: WorkerCommand) -> Self {
        Self {
            command,
            session: None,
            rss_tracker: None,
        }
    }

    pub fn with_rss_tracker(command: WorkerCommand, rss_tracker: WorkerRssTracker) -> Self {
        Self {
            command,
            session: None,
            rss_tracker: Some(rss_tracker),
        }
    }

    pub fn with_session(
        command: WorkerCommand,
        session: Arc<crate::session::PythonSessionPool>,
    ) -> Self {
        Self {
            command,
            session: Some(session),
            rss_tracker: None,
        }
    }
}

impl PdfClassifierPort for ClassifierPort {
    fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        timeout: Duration,
    ) -> PdfClassifierExecution {
        match &self.session {
            Some(session) => match session.classify_pdf(request, timeout) {
                Ok(outcome) => {
                    let transport = match outcome.transport {
                        crate::session::PythonSessionTransport::Session => {
                            ClassificationTransport::Session
                        }
                        crate::session::PythonSessionTransport::OneShot => {
                            ClassificationTransport::OneShot
                        }
                        crate::session::PythonSessionTransport::NotApplicable => {
                            ClassificationTransport::NotApplicable
                        }
                    };
                    let validated = validate_classifier_result_for_request(request, &outcome.value)
                        .map(|()| outcome.value);
                    PdfClassifierExecution {
                        outcome: validated,
                        transport,
                        attempt_count: outcome.attempt_count,
                        duration_ms: outcome.duration_ms,
                    }
                }
                Err(failure) => PdfClassifierExecution {
                    outcome: Err(failure.failure),
                    transport: match failure.transport {
                        crate::session::PythonSessionTransport::Session => {
                            ClassificationTransport::Session
                        }
                        crate::session::PythonSessionTransport::OneShot => {
                            ClassificationTransport::OneShot
                        }
                        crate::session::PythonSessionTransport::NotApplicable => {
                            ClassificationTransport::NotApplicable
                        }
                    },
                    attempt_count: failure.attempt_count,
                    duration_ms: failure.duration_ms,
                },
            },
            None => {
                let execution = classify_pdf_oneshot_observed(
                    &self.command,
                    request,
                    timeout,
                    self.rss_tracker.as_ref(),
                );
                PdfClassifierExecution {
                    outcome: execution.outcome,
                    transport: if execution.attempt_count == 0 {
                        ClassificationTransport::NotApplicable
                    } else {
                        ClassificationTransport::OneShot
                    },
                    attempt_count: execution.attempt_count,
                    duration_ms: execution.duration_ms,
                }
            }
        }
    }
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn validate_classifier_result_for_request(
    request: &PdfClassifierRequestV1,
    result: &PdfClassifierResultV1,
) -> Result<(), ParseFailure> {
    result
        .validate_for_max_pages(request.max_pages)
        .map_err(|_| {
            contract_failure(
                ErrorCode::ParserInvalidPayload,
                "classifier result violates the request page window",
                Some(&request.file_path),
                None,
                DiagnosticStage::Process,
            )
        })
}

/// 一次严格 ``classify-pdf`` one-shot 进程调用（spec Part 7.1/7.3）。
pub fn classify_pdf_oneshot(
    command: &WorkerCommand,
    request: &PdfClassifierRequestV1,
    timeout: Duration,
) -> Result<PdfClassifierResultV1, ParseFailure> {
    classify_pdf_oneshot_observed(command, request, timeout, None).outcome
}

pub(crate) fn classify_pdf_oneshot_observed(
    command: &WorkerCommand,
    request: &PdfClassifierRequestV1,
    timeout: Duration,
    rss_tracker: Option<&WorkerRssTracker>,
) -> OneShotExecution<PdfClassifierResultV1> {
    let prepared = (|| {
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
        serde_json::to_vec(request).map_err(|_| {
            contract_failure(
                ErrorCode::ParserInvalidPayload,
                "classifier request could not be serialized",
                Some(&request.file_path),
                None,
                DiagnosticStage::Parse,
            )
        })
    })();
    let stdin = match prepared {
        Ok(stdin) => stdin,
        Err(failure) => {
            return OneShotExecution {
                outcome: Err(failure),
                attempt_count: 0,
                duration_ms: 0,
            };
        }
    };
    let spec = ProcessSpec {
        program: command.program.clone(),
        args: command.args_for("classify-pdf"),
        current_dir: command.current_dir.clone(),
        stdin,
        timeout,
        capture_limit: CLASSIFIER_CAPTURE_LIMIT,
        rss_tracker: rss_tracker.cloned(),
    };
    let started = std::time::Instant::now();
    let output = match run_process_observed(&spec) {
        Ok(output) => output,
        Err(process) => {
            return OneShotExecution {
                outcome: Err(classifier_process_failure(
                    process.error,
                    &request.file_path,
                )),
                attempt_count: u64::from(process.child_started),
                duration_ms: if process.child_started {
                    elapsed_ms(started)
                } else {
                    0
                },
            };
        }
    };
    let outcome = (|| {
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
        let response: PdfClassifierResponseV1 =
            serde_json::from_slice(&output.stdout).map_err(|_| {
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
            ClassifierResponseStatus::Ok => {
                let result = response.result.0.ok_or_else(|| {
                    contract_failure(
                        ErrorCode::ParserInvalidPayload,
                        "ok classifier response is missing its typed result",
                        Some(&request.file_path),
                        None,
                        DiagnosticStage::Process,
                    )
                })?;
                validate_classifier_result_for_request(request, &result)?;
                Ok(result)
            }
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
    })();
    OneShotExecution {
        outcome,
        attempt_count: 1,
        duration_ms: elapsed_ms(started),
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

pub(crate) fn validate_classifier_source_before(
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

pub(crate) fn validate_classifier_source_after(
    request: &PdfClassifierRequestV1,
) -> Result<(), ParseFailure> {
    let (source_version, _) = current_source(&request.file_path).map_err(|_| {
        classifier_source_failure(
            request,
            "file source became unavailable during classification",
        )
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
    // spec Part 3.2 matrix: classifier 后验 source-version 不一致 →
    // SOURCE_VERSION_CHANGED, retryable=true（结果丢弃，可重建后重试）。
    ParseFailure {
        class: FailureClass::RecoverableParserFailure,
        diagnostic: Diagnostic {
            error_code: ErrorCode::SourceVersionChanged,
            message: message.to_string(),
            retryable: true,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some(request.file_path.clone())),
            backend: Nullable(None),
        },
    }
}
