use super::*;

/// Session 传输/生命周期失败。typed `unknown/error` 结果不会到达这里——
/// 它们在 outer `status=ok` 中作为完整 domain result 返回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// 子进程无法启动（或 Windows Job Object containment 失败）。
    StartFailed,
    /// 请求超过了 effective deadline；对应文件记 Timeout，不再重试。
    Timeout,
    /// 进程崩溃/被外部杀死。
    Crashed,
    /// EOF：stdin/stdout 提前关闭，或读到了半帧。
    Eof,
    /// stdout/stderr 超限、非 UTF-8、非预期帧、错配 request_id 等。
    ProtocolCorruption(String),
    /// 读取线程 I/O 失败。
    IoFailed,
    /// outer `status=error`：worker 拒绝请求，携带可审计 diagnostic。
    Rejected(Box<WorkerDiagnostic>),
    /// hello 的 build 与 preflight 不一致。
    BuildMismatch,
    /// 收到的 frame 超过该 operation 的 capture limit。
    FrameTooLarge,
}

impl SessionError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, SessionError::Timeout)
    }

    /// session start/EOF/crash 最多重建一次；协议损坏和超时直接失败。
    pub fn is_retryable_transport(&self) -> bool {
        match self {
            SessionError::StartFailed | SessionError::Crashed | SessionError::Eof => true,
            SessionError::Rejected(diagnostic) => diagnostic.retryable,
            SessionError::IoFailed
            | SessionError::ProtocolCorruption(_)
            | SessionError::BuildMismatch
            | SessionError::FrameTooLarge => false,
            SessionError::Timeout => false,
        }
    }

    pub fn diagnostic(&self, file_path: Option<&str>) -> crate::fallback::ParseFailure {
        let (code, message, retryable) = match self {
            SessionError::Timeout => (
                ai_daily_scanner_contract::ErrorCode::ParserTimeout,
                "worker session exceeded its deadline".to_string(),
                false,
            ),
            SessionError::StartFailed => (
                ai_daily_scanner_contract::ErrorCode::ParserStartFailed,
                "worker session could not be started".to_string(),
                true,
            ),
            SessionError::Crashed => (
                ai_daily_scanner_contract::ErrorCode::ParserFailed,
                "worker session crashed before completing its response".to_string(),
                true,
            ),
            SessionError::Eof => (
                ai_daily_scanner_contract::ErrorCode::ParserFailed,
                "worker session reached EOF before completing its response".to_string(),
                true,
            ),
            SessionError::IoFailed => (
                ai_daily_scanner_contract::ErrorCode::ParserInvalidPayload,
                "worker session transport failed".to_string(),
                false,
            ),
            SessionError::ProtocolCorruption(message) => (
                ai_daily_scanner_contract::ErrorCode::ParserInvalidPayload,
                message.clone(),
                false,
            ),
            SessionError::Rejected(diagnostic) => (
                match diagnostic.error_code.as_str() {
                    "INVALID_REQUEST" => ai_daily_scanner_contract::ErrorCode::InvalidRequest,
                    "PARSER_START_FAILED" => {
                        ai_daily_scanner_contract::ErrorCode::ParserStartFailed
                    }
                    "PARSER_TIMEOUT" => ai_daily_scanner_contract::ErrorCode::ParserTimeout,
                    "FILE_TOO_LARGE" => ai_daily_scanner_contract::ErrorCode::FileTooLarge,
                    "PDF_PROTECTED" => ai_daily_scanner_contract::ErrorCode::PdfProtected,
                    "PARSER_INVALID_PAYLOAD" => {
                        ai_daily_scanner_contract::ErrorCode::ParserInvalidPayload
                    }
                    "SOURCE_VERSION_CHANGED" => {
                        ai_daily_scanner_contract::ErrorCode::SourceVersionChanged
                    }
                    "INTERNAL_ERROR" => ai_daily_scanner_contract::ErrorCode::InternalError,
                    _ => ai_daily_scanner_contract::ErrorCode::ParserFailed,
                },
                diagnostic.message.clone(),
                diagnostic.retryable,
            ),
            SessionError::BuildMismatch => (
                ai_daily_scanner_contract::ErrorCode::WorkerBuildChanged,
                "session hello build does not match the preflight identity".to_string(),
                false,
            ),
            SessionError::FrameTooLarge => (
                ai_daily_scanner_contract::ErrorCode::ParserInvalidPayload,
                "worker session response exceeded its capture limit".to_string(),
                false,
            ),
        };
        let document_error = matches!(self, Self::Rejected(error)
            if matches!(error.error_code.as_str(), "FILE_TOO_LARGE" | "PDF_PROTECTED")
                || error.message == ai_daily_worker_contract::OFFICE_CONTENT_MISSING_MESSAGE);
        crate::fallback::ParseFailure {
            class: if document_error {
                crate::fallback::FailureClass::Deterministic
            } else if retryable {
                crate::fallback::FailureClass::RecoverableParserFailure
            } else {
                crate::fallback::FailureClass::ContractFailure
            },
            diagnostic: ai_daily_scanner_contract::Diagnostic {
                error_code: code,
                message,
                retryable,
                stage: if document_error {
                    ai_daily_scanner_contract::DiagnosticStage::Parse
                } else {
                    ai_daily_scanner_contract::DiagnosticStage::Process
                },
                file_path: ai_daily_scanner_contract::Nullable(file_path.map(str::to_string)),
                backend: ai_daily_scanner_contract::Nullable(None),
            },
        }
    }
}

/// Worker v2 transport retry decision for one logical operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAction {
    RetrySession,
    GiveUp,
}

pub fn retry_action(error: &SessionError, session_attempt: u32) -> RetryAction {
    match error {
        SessionError::Timeout => RetryAction::GiveUp,
        SessionError::Rejected(diagnostic) if diagnostic.error_code == "SOURCE_VERSION_CHANGED" => {
            // The request embeds the stale source version. Rebuilding a
            // session and replaying the same request cannot make it valid.
            RetryAction::GiveUp
        }
        SessionError::Rejected(diagnostic) if !diagnostic.retryable => RetryAction::GiveUp,
        _ if session_attempt == 0 => RetryAction::RetrySession,
        _ => RetryAction::GiveUp,
    }
}
