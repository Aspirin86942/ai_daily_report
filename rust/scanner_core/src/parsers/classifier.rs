//! Local PDF classifier port（spec Part 7.1）。
//!
//! 生产实现通过 Rust PDF worker-v4 pool 调用；测试
//! adapter 返回内存结果。trait 返回 typed ``ClassifyResult``（含
//! text/no-text/unknown/error 四态），``Err(ParseFailure)`` 只表示进程/传输
//! 层失败（timeout/crash/坏响应/外层 error），由调度器映射为 ``unknown``。

use std::sync::Arc;
use std::time::Duration;

use ai_daily_scanner_contract::{
    ClassificationTransport, Diagnostic, DiagnosticStage, ErrorCode, Nullable,
};
use ai_daily_worker_contract::{pdf_sample_pages, ClassifyRequest, ClassifyResult, ClassifyStatus};

use super::{contract_failure, current_source};
use crate::fallback::{FailureClass, ParseFailure};
use crate::session::WorkerTransport;

pub const CLASSIFIER_CAPTURE_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfOpenFailure {
    InvalidFormat,
    PasswordRequired,
}

impl PdfOpenFailure {
    pub(crate) fn confirmed(result: &ClassifyResult, path: &str) -> Option<Self> {
        let d = result.diagnostic.as_ref()?;
        if result.status != ClassifyStatus::Error
            || result.page_audit.is_some()
            || result.page_count.is_some()
            || result.result_examined_pages.is_some()
            || d.retryable
            || d.stage != "parse"
            || d.backend.as_deref() != Some("rust_pdfium_v3")
            || d.file_path.as_deref() != Some(path)
        {
            return None;
        }
        match (d.error_code.as_str(), d.message.as_str()) {
            ("PARSER_FAILED", "FPDF_ERR_FORMAT") => Some(Self::InvalidFormat),
            ("PDF_PROTECTED", "FPDF_ERR_PASSWORD") => Some(Self::PasswordRequired),
            _ => None,
        }
    }

    pub(crate) const fn error_code(self) -> &'static str {
        match self {
            Self::InvalidFormat => "PARSER_FAILED",
            Self::PasswordRequired => "PDF_PROTECTED",
        }
    }
}

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

/// 本地可替换 PDF 分类 adapter。生产使用 worker-v4 pool，测试使用内存结果。
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

/// 生产实现：调用 Rust PDF worker-v4 pool。
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
        let transport = |value| match value {
            WorkerTransport::Session => ClassificationTransport::Session,
            WorkerTransport::NotApplicable => ClassificationTransport::NotApplicable,
        };
        match self.session.classify_pdf(request, timeout) {
            Ok(op) => PdfClassifierExecution {
                outcome: validate_classifier_result_for_request(request, &op.value)
                    .map(|()| op.value),
                transport: transport(op.transport),
                attempt_count: op.attempt_count,
                duration_ms: op.duration_ms,
            },
            Err(op) => PdfClassifierExecution {
                outcome: Err(op.failure),
                transport: transport(op.transport),
                attempt_count: op.attempt_count,
                duration_ms: op.duration_ms,
            },
        }
    }
}

fn validate_classifier_result_for_request(
    request: &ClassifyOperation,
    result: &ClassifyResult,
) -> Result<(), ParseFailure> {
    let invalid = |message| {
        contract_failure(
            ErrorCode::ParserInvalidPayload,
            message,
            Some(&request.file_path),
            None,
            DiagnosticStage::Process,
        )
    };
    if request.policy_version == "pdf_text_presence_v2"
        && result.page_audit.is_none()
        && matches!(
            result.status,
            ClassifyStatus::TextInParseWindow | ClassifyStatus::NoTextInParseWindow
        )
    {
        return Err(invalid("classifier v2 requires observed page provenance"));
    }
    if let Some(audit) = &result.page_audit {
        let expected = pdf_sample_pages(
            result.page_count.unwrap_or(0),
            request.max_pages,
            &request.policy_version,
        );
        if audit.policy_version != request.policy_version
            || !expected.starts_with(&audit.checked_pages)
        {
            return Err(invalid(
                "classifier observed pages violate the selected policy",
            ));
        }
        if let Some(observations) = &audit.observations {
            let text = result.status == ClassifyStatus::TextInParseWindow;
            if observations
                .iter()
                .enumerate()
                .any(|(index, page)| page.has_text != (text && index + 1 == observations.len()))
            {
                return Err(invalid("classifier text observations violate its status"));
            }
        }
    }
    result
        .validate_for_max_pages(request.max_pages)
        .map_err(|_| invalid("classifier result violates the request page window"))
}

pub(crate) fn validate_classifier_source_before(
    request: &ClassifyOperation,
) -> Result<(), ParseFailure> {
    validate_classifier_source(
        request,
        "file metadata is unavailable before classifier start",
        "file source version changed before classifier start",
    )
}

pub(crate) fn validate_classifier_source_after(
    request: &ClassifyOperation,
) -> Result<(), ParseFailure> {
    validate_classifier_source(
        request,
        "file source became unavailable during classification",
        "file source version changed during classification",
    )
}

fn validate_classifier_source(
    request: &ClassifyOperation,
    unavailable: &str,
    changed: &str,
) -> Result<(), ParseFailure> {
    let message = match current_source(&request.file_path) {
        Ok((version, _)) if version == request.source_version => return Ok(()),
        Ok(_) => changed,
        Err(()) => unavailable,
    };
    Err(ParseFailure {
        class: FailureClass::RecoverableParserFailure,
        diagnostic: Diagnostic {
            error_code: ErrorCode::SourceVersionChanged,
            message: message.into(),
            retryable: true,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some(request.file_path.clone())),
            backend: Nullable(None),
        },
    })
}
