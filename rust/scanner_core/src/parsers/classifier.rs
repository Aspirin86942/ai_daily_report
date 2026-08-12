//! Local PDF classifier port（spec Part 7.1）。
//!
//! 生产实现通过 Python worker-v2 pool 调用；测试
//! adapter 返回内存结果。trait 返回 typed ``ClassifyResult``（含
//! text/no-text/unknown/error 四态），``Err(ParseFailure)`` 只表示进程/传输
//! 层失败（timeout/crash/坏响应/外层 error），由调度器映射为 ``unknown``。

use std::sync::Arc;
use std::time::Duration;

use ai_daily_scanner_contract::{
    ClassificationTransport, Diagnostic, DiagnosticStage, ErrorCode, Nullable,
};
use ai_daily_worker_contract::{ClassifyRequest, ClassifyResult};

use super::{contract_failure, current_source};
use crate::fallback::{FailureClass, ParseFailure};

pub const CLASSIFIER_CAPTURE_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifyOperation {
    pub request_id: String,
    pub payload: ClassifyRequest,
}

impl std::ops::Deref for ClassifyOperation {
    type Target = ClassifyRequest;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

/// 本地可替换 PDF 分类 adapter。生产使用 worker-v2 pool，测试使用内存结果。
pub trait PdfClassifierPort: Send + Sync {
    fn classify_pdf(
        &self,
        request: &ClassifyOperation,
        timeout: Duration,
    ) -> PdfClassifierExecution;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfClassifierExecution {
    pub outcome: Result<ClassifyResult, ParseFailure>,
    pub transport: ClassificationTransport,
    pub attempt_count: u64,
    pub duration_ms: u64,
}

impl PdfClassifierExecution {
    pub fn test_execution(outcome: Result<ClassifyResult, ParseFailure>) -> Self {
        Self {
            outcome,
            transport: ClassificationTransport::Session,
            attempt_count: 1,
            duration_ms: 0,
        }
    }
}

/// 生产实现：调用 Python worker-v2 pool。
#[derive(Clone)]
pub struct ClassifierPort {
    session: Arc<crate::session::WorkerPool>,
}

impl ClassifierPort {
    pub fn new(session: Arc<crate::session::WorkerPool>) -> Self {
        Self { session }
    }
}

impl PdfClassifierPort for ClassifierPort {
    fn classify_pdf(
        &self,
        request: &ClassifyOperation,
        timeout: Duration,
    ) -> PdfClassifierExecution {
        match self.session.classify_pdf(request, timeout) {
            Ok(outcome) => {
                let transport = match outcome.transport {
                    crate::session::WorkerTransport::Session => ClassificationTransport::Session,
                    crate::session::WorkerTransport::NotApplicable => {
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
                    crate::session::WorkerTransport::Session => ClassificationTransport::Session,
                    crate::session::WorkerTransport::NotApplicable => {
                        ClassificationTransport::NotApplicable
                    }
                },
                attempt_count: failure.attempt_count,
                duration_ms: failure.duration_ms,
            },
        }
    }
}

fn validate_classifier_result_for_request(
    request: &ClassifyOperation,
    result: &ClassifyResult,
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

pub(crate) fn validate_classifier_source_before(
    request: &ClassifyOperation,
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
    request: &ClassifyOperation,
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

fn classifier_source_failure(request: &ClassifyOperation, message: &str) -> ParseFailure {
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
