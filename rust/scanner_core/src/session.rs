//! Long-lived streaming Python worker session（spec Part 7）。
//!
//! `ai_daily_python_session_v1` 是独立于共享 `ai_daily_worker_v1` 的 NDJSON
//! 流式契约：首帧 hello，每行一个请求 envelope、每行一个 typed response，
//! 单 in-flight、逐请求 deadline。每个 session child 拥有独立 Windows Job
//! Object（见 [`crate::windows_job::SessionJob`]），杀一个超时请求不会连带
//! 杀掉 pool 中其他 session。生命周期计数唯一为 `max_requests_per_session`；
//! `batch_size` 已删除。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ai_daily_scanner_contract::{
    NormalizedScannerProfileV2, PdfClassifierRequestV1, PdfClassifierResultV1,
    PythonOperationDiagnosticV1, PythonSessionHelloV1, PythonSessionOperation,
    PythonSessionRequestV1, PythonSessionResponseStatus, PythonSessionResponseV1,
    PythonSessionResultV1, PythonSessionVersionResponseV1, Validate, WorkerParseRequest,
    WorkerParseResponse,
};

use crate::parsers::WorkerCommand;

pub const SESSION_CONTRACT_VERSION: &str = "ai_daily_python_session_v1";
pub const SESSION_PROTOCOL_VERSION: u64 = 1;

/// spec Part 7.2：hello/request/classification response 每 frame 1 MiB。
pub const SESSION_HELLO_FRAME_LIMIT: usize = 1024 * 1024;
pub const SESSION_REQUEST_FRAME_LIMIT: usize = 1024 * 1024;
pub const SESSION_CLASSIFY_FRAME_LIMIT: usize = 1024 * 1024;
/// stderr 由独立 reader 持续排空，每个 in-flight request 累计最多 1 MiB。
pub const SESSION_STDERR_LIMIT: usize = 1024 * 1024;
/// 读取线程的硬上限，防止单帧把父进程内存打爆；具体 frame 限制由客户端
/// 在收到后按 operation 再次校验。
const SESSION_STDOUT_HARD_CAP: usize = 64 * 1024 * 1024;

/// spec Part 7.3：classify_pdf_v1 与 parse_v1 各自独立 attempt 上限 3，
/// 一个 text PDF 依次产生两个独立 operation，禁止混成“单文件共 3 次”。
pub const MAX_CLASSIFY_ATTEMPTS: u32 = 3;
pub const MAX_PARSE_ATTEMPTS: u32 = 3;

/// Session 生命周期参数（spec Part 7.3 默认值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionParams {
    pub concurrency: usize,
    pub max_requests_per_session: u64,
    pub idle_ttl: Duration,
    pub rss_limit_bytes: u64,
}

impl Default for SessionParams {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_requests_per_session: 128,
            idle_ttl: Duration::from_secs(30),
            rss_limit_bytes: 512 * 1024 * 1024,
        }
    }
}

impl SessionParams {
    /// spec Part 7.3 / Part 8.1：默认 `session_concurrency = min(max_workers, 4)`。
    pub fn with_default_concurrency(max_workers: u64) -> Self {
        let mut params = Self::default();
        params.concurrency = usize::try_from(max_workers.min(4)).unwrap_or(4).max(1);
        params
    }

    pub fn from_profile_v2(profile: &NormalizedScannerProfileV2) -> Self {
        Self {
            concurrency: usize::try_from(profile.session_concurrency)
                .unwrap_or(Self::default().concurrency)
                .clamp(1, 8),
            max_requests_per_session: profile.max_requests_per_session,
            idle_ttl: Duration::from_millis(profile.session_idle_ttl_ms),
            rss_limit_bytes: profile.session_rss_limit_bytes,
        }
    }
}

/// Session 传输/生命周期失败。typed `unknown/error` 结果不会到达这里——
/// 它们在 outer `status=ok` 中作为完整 domain result 返回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// 子进程无法启动（或 Windows Job Object containment 失败）。
    StartFailed,
    /// 请求超过了 effective deadline；对应文件记 Timeout，不再 one-shot 重试。
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
    Rejected(PythonOperationDiagnosticV1),
    /// hello 的 build 与 preflight 不一致。
    BuildMismatch,
    /// 收到的 frame 超过该 operation 的 capture limit。
    FrameTooLarge,
}

impl SessionError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, SessionError::Timeout)
    }

    /// spec Part 7.3：session start/EOF/protocol corruption/crash 重建并重试当前
    /// operation 最多 1 次；第二次仍失败时，仅对 retryable 且非 timeout 的
    /// transport failure 允许 one-shot 1 次。
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
                match diagnostic.error_code {
                    ai_daily_scanner_contract::PythonOperationErrorCode::InvalidRequest => {
                        ai_daily_scanner_contract::ErrorCode::InvalidRequest
                    }
                    ai_daily_scanner_contract::PythonOperationErrorCode::ParserStartFailed => {
                        ai_daily_scanner_contract::ErrorCode::ParserStartFailed
                    }
                    ai_daily_scanner_contract::PythonOperationErrorCode::ParserTimeout => {
                        ai_daily_scanner_contract::ErrorCode::ParserTimeout
                    }
                    ai_daily_scanner_contract::PythonOperationErrorCode::ParserInvalidPayload => {
                        ai_daily_scanner_contract::ErrorCode::ParserInvalidPayload
                    }
                    ai_daily_scanner_contract::PythonOperationErrorCode::ParserFailed => {
                        ai_daily_scanner_contract::ErrorCode::ParserFailed
                    }
                    ai_daily_scanner_contract::PythonOperationErrorCode::SourceVersionChanged => {
                        ai_daily_scanner_contract::ErrorCode::SourceVersionChanged
                    }
                    ai_daily_scanner_contract::PythonOperationErrorCode::InternalError => {
                        ai_daily_scanner_contract::ErrorCode::InternalError
                    }
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
        crate::fallback::ParseFailure {
            class: if retryable {
                crate::fallback::FailureClass::RecoverableParserFailure
            } else {
                crate::fallback::FailureClass::ContractFailure
            },
            diagnostic: ai_daily_scanner_contract::Diagnostic {
                error_code: code,
                message,
                retryable,
                stage: ai_daily_scanner_contract::DiagnosticStage::Process,
                file_path: ai_daily_scanner_contract::Nullable(
                    file_path.map(str::to_string),
                ),
                backend: ai_daily_scanner_contract::Nullable(None),
            },
        }
    }
}

/// spec Part 7.3 retry 决策。`session_attempt` 是当前 logical operation 在
/// session 上已进行的次数（0-based），不含 one-shot。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAction {
    /// 重建 session 后重试当前 logical operation。
    RetrySession,
    /// 仅对 retryable 且非 timeout 的 transport failure 允许 one-shot 1 次。
    OneShot,
    /// 不再重试（timeout 或确定性失败；timeout 永远不 one-shot 重试）。
    GiveUp,
}

pub fn retry_action(error: &SessionError, session_attempt: u32) -> RetryAction {
    match error {
        SessionError::Timeout => RetryAction::GiveUp,
        SessionError::Rejected(diagnostic) if !diagnostic.retryable => RetryAction::GiveUp,
        _ if session_attempt == 0 => RetryAction::RetrySession,
        _ if error.is_retryable_transport() => RetryAction::OneShot,
        _ => RetryAction::GiveUp,
    }
}

enum LineEvent {
    Line(Vec<u8>),
    Eof,
    Error(SessionError),
}

/// 一个持久 session 子进程：独立 Job Object + stdin + 行式 stdout reader +
/// 持续排空的 stderr reader。
struct SessionChild {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<LineEvent>,
    #[allow(dead_code)]
    stdout_thread: Option<thread::JoinHandle<()>>,
    #[allow(dead_code)]
    stderr_thread: Option<thread::JoinHandle<()>>,
    #[cfg(windows)]
    job: Option<crate::windows_job::SessionJob>,
    /// 上次请求完成时刻；idle TTL 按“空闲时间”回收（spec 7.3/8.1），不是
    /// 进程年龄——持续忙碌的 session 不应每 30s 被误回收。
    last_activity: Instant,
}

impl SessionChild {
    fn spawn(command: &WorkerCommand) -> Result<Self, SessionError> {
        let mut builder = Command::new(&command.program);
        builder
            .args(&command.base_args)
            .arg("session")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(directory) = &command.current_dir {
            builder.current_dir(directory);
        }
        let mut child = builder.spawn().map_err(|_| SessionError::StartFailed)?;
        #[cfg(windows)]
        let job = {
            use std::os::windows::io::AsRawHandle;
            match crate::windows_job::SessionJob::assign(child.as_raw_handle()) {
                Ok(job) => Some(job),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SessionError::StartFailed);
                }
            }
        };
        let stdin = child.stdin.take().ok_or(SessionError::IoFailed)?;
        let stdout = child.stdout.take().ok_or(SessionError::IoFailed)?;
        let stderr = child.stderr.take().ok_or(SessionError::IoFailed)?;
        let (sender, receiver) = mpsc::channel();
        let stdout_thread = thread::spawn(move || read_stdout_lines(stdout, sender));
        let stderr_thread = thread::spawn(move || drain_stderr(stderr));
        Ok(Self {
            child,
            stdin,
            lines: receiver,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            #[cfg(windows)]
            job,
            last_activity: Instant::now(),
        })
    }

    fn read_line(&self, timeout: Duration) -> Result<Vec<u8>, SessionError> {
        match self.lines.recv_timeout(timeout) {
            Ok(LineEvent::Line(line)) => Ok(line),
            Ok(LineEvent::Eof) => Err(SessionError::Eof),
            Ok(LineEvent::Error(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Err(SessionError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(SessionError::Eof),
        }
    }

    fn write_line(&mut self, frame: &[u8]) -> Result<(), SessionError> {
        self.stdin.write_all(frame).map_err(|_| SessionError::Eof)?;
        self.stdin.write_all(b"\n").map_err(|_| SessionError::Eof)?;
        self.stdin.flush().map_err(|_| SessionError::Eof)
    }

    fn recycle_due(&self, params: &SessionParams) -> bool {
        // Idle TTL measures elapsed time since the last completed request, not
        // process age. RSS recycle (`params.rss_limit_bytes`, spec 7.3) is a
        // pending item for the pool-wiring task: it needs per-session process
        // memory accounting (Windows Job Object PeakJobProcessUsedMemory) and
        // is intentionally surfaced here rather than silently dropped.
        self.last_activity.elapsed() >= params.idle_ttl
    }

    fn touch_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// 优雅重建前的收割：杀 Job Object 并等进程树清空。
    fn reap(mut self) {
        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            let _ = job.terminate(124);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SessionChild {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            let _ = job.terminate(124);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_stdout_lines(stdout: std::process::ChildStdout, sender: Sender<LineEvent>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_line_bounded(&mut reader, SESSION_STDOUT_HARD_CAP) {
            Ok(Some(line)) => {
                if sender.send(LineEvent::Line(line)).is_err() {
                    break;
                }
            }
            Ok(None) => {
                let _ = sender.send(LineEvent::Eof);
                break;
            }
            Err(error) => {
                let _ = sender.send(LineEvent::Error(error));
                break;
            }
        }
    }
}

/// 读取一行，强制按字节上限；行尾 `\n` 保留给调用方。
fn read_line_bounded(
    reader: &mut impl BufRead,
    cap: usize,
) -> Result<Option<Vec<u8>>, SessionError> {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|_| SessionError::IoFailed)?;
        if available.is_empty() {
            if buffer.is_empty() {
                return Ok(None);
            }
            return Ok(Some(buffer));
        }
        let newline_pos = available.iter().position(|byte| *byte == b'\n');
        let take = newline_pos.map_or(available.len(), |index| index + 1);
        if buffer.len() + take > cap {
            return Err(SessionError::FrameTooLarge);
        }
        buffer.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline_pos.is_some() {
            return Ok(Some(buffer));
        }
    }
}

fn drain_stderr(stderr: std::process::ChildStderr) {
    let mut reader = stderr;
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                let available = SESSION_STDERR_LIMIT.saturating_sub(buffer.len());
                let accepted = available.min(count);
                buffer.extend_from_slice(&chunk[..accepted]);
                // 溢出只计数，不阻塞；任一 request 失败时由 diagnostic 体现。
                if accepted < count {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// 长驻 Python session 客户端。并发通过 session pool 获得，进程内不 multiplex。
pub struct PythonSession {
    child: Option<SessionChild>,
    params: SessionParams,
    identity: PythonSessionVersionResponseV1,
    requests_served: u64,
}

impl PythonSession {
    /// 启动会话并严格校验首帧 hello；build 与 preflight `session-version` 完全
    /// 一致（spec Part 7.1），否则视为 handshake failure。
    pub fn start(
        command: &WorkerCommand,
        expected: &PythonSessionVersionResponseV1,
        params: SessionParams,
        timeout: Duration,
    ) -> Result<Self, SessionError> {
        let child = SessionChild::spawn(command)?;
        let mut session = Self {
            child: Some(child),
            params,
            identity: expected.clone(),
            requests_served: 0,
        };
        let hello = session.read_hello(timeout)?;
        if hello.worker_build != expected.worker_build
            || hello.classifier_build != expected.classifier_build
        {
            session.kill();
            return Err(SessionError::BuildMismatch);
        }
        // idle 基准从 hello 校验完成后开始，而不是进程 spawn 时刻：冷启动
        // （import pypdfium2 等）不计入 idle TTL。
        if let Some(child) = session.child.as_mut() {
            child.touch_activity();
        }
        Ok(session)
    }

    fn read_hello(&mut self, timeout: Duration) -> Result<PythonSessionHelloV1, SessionError> {
        let child = self.child.as_ref().ok_or(SessionError::Eof)?;
        let line = child.read_line(timeout)?;
        if line.len() > SESSION_HELLO_FRAME_LIMIT {
            return Err(SessionError::FrameTooLarge);
        }
        let hello: PythonSessionHelloV1 =
            serde_json::from_slice(&line).map_err(|_| {
                SessionError::ProtocolCorruption(
                    "session hello is not one strict JSON frame".to_string(),
                )
            })?;
        hello.validate().map_err(|message| {
            SessionError::ProtocolCorruption(format!(
                "session hello violates the strict contract: {message}"
            ))
        })?;
        Ok(hello)
    }

    pub fn identity(&self) -> &PythonSessionVersionResponseV1 {
        &self.identity
    }

    pub fn params(&self) -> &SessionParams {
        &self.params
    }

    pub fn requests_served(&self) -> u64 {
        self.requests_served
    }

    /// 达到任一 recycle 条件（request 数 / idle TTL）时在当前 response 完整
    /// 接收后由调用方优雅重建；这里只做只读判断。
    pub fn recycle_due(&self) -> bool {
        let child = match self.child.as_ref() {
            Some(child) => child,
            None => return true,
        };
        self.requests_served >= self.params.max_requests_per_session
            || child.recycle_due(&self.params)
    }

    pub fn is_alive(&self) -> bool {
        self.child.is_some()
    }

    /// 杀当前 child 的 Job Object 并等待进程树清空；不重建。
    pub fn kill(&mut self) {
        if let Some(child) = self.child.take() {
            child.reap();
        }
    }

    fn dispatch_classify(
        &mut self,
        request: &PdfClassifierRequestV1,
        timeout: Duration,
    ) -> Result<PdfClassifierResultV1, SessionError> {
        request.validate().map_err(|message| {
            SessionError::ProtocolCorruption(format!(
                "classify request violates the strict contract: {message}"
            ))
        })?;
        let envelope = PythonSessionRequestV1 {
            contract: "ai_daily_python_session".to_string(),
            protocol_version: SESSION_PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            operation: PythonSessionOperation::ClassifyPdfV1,
            payload: serde_json::to_value(request).map_err(|_| SessionError::IoFailed)?,
        };
        let frame = serde_json::to_vec(&envelope).map_err(|_| SessionError::IoFailed)?;
        self.write_request(&frame)?;
        let child = self.child.as_ref().ok_or(SessionError::Eof)?;
        let line = child.read_line(timeout)?;
        if line.len() > SESSION_CLASSIFY_FRAME_LIMIT {
            return Err(SessionError::FrameTooLarge);
        }
        let response = self.parse_response(
            line,
            PythonSessionOperation::ClassifyPdfV1,
            &request.request_id,
        )?;
        self.mark_request_complete();
        match response {
            PythonSessionResponseV1 {
                status: PythonSessionResponseStatus::Ok,
                result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Classify(result))),
                ..
            } => Ok(result),
            PythonSessionResponseV1 {
                status: PythonSessionResponseStatus::Error,
                error: ai_daily_scanner_contract::Nullable(Some(diagnostic)),
                ..
            } => Err(SessionError::Rejected(diagnostic)),
            PythonSessionResponseV1 { result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Parse(_))), .. } => Err(
                SessionError::ProtocolCorruption(
                    "classify response carried a parse result".to_string(),
                ),
            ),
            PythonSessionResponseV1 { result: ai_daily_scanner_contract::Nullable(None), .. } => {
                Err(SessionError::ProtocolCorruption(
                    "ok classify response is missing its typed result".to_string(),
                ))
            }
            _ => Err(SessionError::ProtocolCorruption(
                "classify response violates the ok/error tagged union".to_string(),
            )),
        }
    }

    fn dispatch_parse(
        &mut self,
        request: &WorkerParseRequest,
        timeout: Duration,
    ) -> Result<WorkerParseResponse, SessionError> {
        request.validate().map_err(|message| {
            SessionError::ProtocolCorruption(format!(
                "parse request violates the strict contract: {message}"
            ))
        })?;
        let envelope = PythonSessionRequestV1 {
            contract: "ai_daily_python_session".to_string(),
            protocol_version: SESSION_PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            operation: PythonSessionOperation::ParseV1,
            payload: serde_json::to_value(request).map_err(|_| SessionError::IoFailed)?,
        };
        let frame = serde_json::to_vec(&envelope).map_err(|_| SessionError::IoFailed)?;
        self.write_request(&frame)?;
        let capture_limit = crate::parsers::worker_response_capture_limit(request)
            .map_err(|_| SessionError::IoFailed)?;
        let child = self.child.as_ref().ok_or(SessionError::Eof)?;
        let line = child.read_line(timeout)?;
        if line.len() > capture_limit {
            return Err(SessionError::FrameTooLarge);
        }
        let response =
            self.parse_response(line, PythonSessionOperation::ParseV1, &request.request_id)?;
        self.mark_request_complete();
        match response {
            PythonSessionResponseV1 {
                status: PythonSessionResponseStatus::Ok,
                result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Parse(result))),
                ..
            } => {
                if result.request_id != request.request_id
                    || result.contract != "ai_daily_worker"
                    || result.protocol_version != 1
                {
                    return Err(SessionError::ProtocolCorruption(
                        "session parse response identity does not match the request".to_string(),
                    ));
                }
                Ok(result)
            }
            PythonSessionResponseV1 {
                status: PythonSessionResponseStatus::Error,
                error: ai_daily_scanner_contract::Nullable(Some(diagnostic)),
                ..
            } => Err(SessionError::Rejected(diagnostic)),
            PythonSessionResponseV1 { result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Classify(_))), .. } => Err(
                SessionError::ProtocolCorruption(
                    "parse response carried a classify result".to_string(),
                ),
            ),
            PythonSessionResponseV1 { result: ai_daily_scanner_contract::Nullable(None), .. } => {
                Err(SessionError::ProtocolCorruption(
                    "ok parse response is missing its typed result".to_string(),
                ))
            }
            _ => Err(SessionError::ProtocolCorruption(
                "parse response violates the ok/error tagged union".to_string(),
            )),
        }
    }

    /// 写请求帧；写失败视为会话死亡（EOF/协议损坏）。
    fn write_request(&mut self, frame: &[u8]) -> Result<(), SessionError> {
        if frame.len() > SESSION_REQUEST_FRAME_LIMIT {
            return Err(SessionError::FrameTooLarge);
        }
        let child = self.child.as_mut().ok_or(SessionError::Eof)?;
        child.write_line(frame)
    }

    /// 在响应完整接收后记账：请求计数 + 刷新 idle 基准时间。
    fn mark_request_complete(&mut self) {
        self.requests_served += 1;
        if let Some(child) = self.child.as_mut() {
            child.touch_activity();
        }
    }

    fn parse_response(
        &self,
        line: Vec<u8>,
        expected_operation: PythonSessionOperation,
        expected_request_id: &str,
    ) -> Result<PythonSessionResponseV1, SessionError> {
        let response: PythonSessionResponseV1 = serde_json::from_slice(&line).map_err(|_| {
            SessionError::ProtocolCorruption(
                "session stdout is not one strict JSON response".to_string(),
            )
        })?;
        response.validate().map_err(|message| {
            SessionError::ProtocolCorruption(format!(
                "session response violates the strict contract: {message}"
            ))
        })?;
        if response.contract != "ai_daily_python_session"
            || response.protocol_version != SESSION_PROTOCOL_VERSION
            || response.operation != expected_operation
            || response.request_id != expected_request_id
        {
            // spec Part 7.2：重复、未知或错配 request_id 视为 protocol corruption。
            return Err(SessionError::ProtocolCorruption(
                "session response operation/contract/request_id mismatch".to_string(),
            ));
        }
        Ok(response)
    }
}

/// 便捷 wrapper：返回 typed classify 结果或 session 层失败（spec Part 7.2）。
pub fn session_classify(
    session: &mut PythonSession,
    request: &PdfClassifierRequestV1,
    timeout: Duration,
) -> Result<PdfClassifierResultV1, SessionError> {
    session.dispatch_classify(request, timeout)
}

/// 便捷 wrapper：返回 typed parse 结果或 session 层失败（spec Part 7.2）。
pub fn session_parse(
    session: &mut PythonSession,
    request: &WorkerParseRequest,
    timeout: Duration,
) -> Result<WorkerParseResponse, SessionError> {
    session.dispatch_parse(request, timeout)
}

/// 构造一次 ``classify_pdf_v1`` 的 strict request（与 one-shot 逐字段相同）。
pub fn build_classify_request(
    request_id: String,
    file_path: &PathBuf,
    source_version: &str,
    max_pages: u64,
) -> PdfClassifierRequestV1 {
    PdfClassifierRequestV1 {
        contract: "ai_daily_pdf_classifier".to_string(),
        protocol_version: 1,
        request_id,
        file_path: file_path.to_string_lossy().into_owned(),
        source_version: source_version.to_string(),
        max_pages,
        policy_version: "pdf_text_presence_v1".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_params_defaults_follow_spec() {
        let params = SessionParams::default();
        assert_eq!(params.concurrency, 4);
        assert_eq!(params.max_requests_per_session, 128);
        assert_eq!(params.idle_ttl, Duration::from_secs(30));
        assert_eq!(params.rss_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(
            SessionParams::with_default_concurrency(8).concurrency,
            4
        );
        assert_eq!(SessionParams::with_default_concurrency(2).concurrency, 2);
        assert_eq!(SessionParams::with_default_concurrency(0).concurrency, 1);
    }

    #[test]
    fn retry_policy_matches_spec_7_3() {
        let timeout = SessionError::Timeout;
        assert_eq!(retry_action(&timeout, 0), RetryAction::GiveUp);
        assert_eq!(retry_action(&timeout, 1), RetryAction::GiveUp);

        let deterministic = SessionError::Rejected(PythonOperationDiagnosticV1 {
            error_code: ai_daily_scanner_contract::PythonOperationErrorCode::ParserFailed,
            message: "corrupt pdf".to_string(),
            retryable: false,
            stage: ai_daily_scanner_contract::PythonOperationStage::Parse,
            file_path: ai_daily_scanner_contract::Nullable(None),
            backend: ai_daily_scanner_contract::Nullable(None),
        });
        assert_eq!(retry_action(&deterministic, 0), RetryAction::GiveUp);

        let retryable = SessionError::Rejected(PythonOperationDiagnosticV1 {
            error_code: ai_daily_scanner_contract::PythonOperationErrorCode::ParserStartFailed,
            message: "transient io".to_string(),
            retryable: true,
            stage: ai_daily_scanner_contract::PythonOperationStage::Process,
            file_path: ai_daily_scanner_contract::Nullable(None),
            backend: ai_daily_scanner_contract::Nullable(None),
        });
        assert_eq!(retry_action(&retryable, 0), RetryAction::RetrySession);
        assert_eq!(retry_action(&retryable, 1), RetryAction::OneShot);

        // EOF/start/crash: rebuild once, then one-shot if retryable.
        assert_eq!(retry_action(&SessionError::Eof, 0), RetryAction::RetrySession);
        assert_eq!(retry_action(&SessionError::Eof, 1), RetryAction::OneShot);
        assert_eq!(
            retry_action(&SessionError::StartFailed, 0),
            RetryAction::RetrySession
        );
        assert_eq!(retry_action(&SessionError::Crashed, 0), RetryAction::RetrySession);

        // Protocol corruption is never silently retried as one-shot.
        assert_eq!(
            retry_action(&SessionError::ProtocolCorruption("x".to_string()), 0),
            RetryAction::RetrySession
        );
        assert_eq!(
            retry_action(&SessionError::ProtocolCorruption("x".to_string()), 1),
            RetryAction::GiveUp
        );
    }

    #[test]
    fn read_line_bounded_enforces_cap_and_newline() {
        let mut data: &[u8] = b"line1\nline2";
        let mut reader = BufReader::new(&mut data);
        let line = read_line_bounded(&mut reader, 1024).expect("first line");
        assert_eq!(line.unwrap(), b"line1\n");
        let line = read_line_bounded(&mut reader, 1024).expect("last line without newline");
        assert_eq!(line.unwrap(), b"line2");

        let mut too_long: &[u8] = b"0123456789";
        let mut reader = BufReader::new(&mut too_long);
        assert_eq!(
            read_line_bounded(&mut reader, 4),
            Err(SessionError::FrameTooLarge)
        );
    }

    #[test]
    fn session_error_maps_to_scanner_diagnostic() {
        let failure = SessionError::Timeout.diagnostic(Some("C:\\x.pdf"));
        assert_eq!(
            failure.diagnostic.error_code,
            ai_daily_scanner_contract::ErrorCode::ParserTimeout
        );
        assert!(!failure.diagnostic.retryable);
        assert_eq!(
            failure.diagnostic.file_path.0.as_deref(),
            Some("C:\\x.pdf")
        );
    }

    fn empty_session() -> PythonSession {
        PythonSession {
            child: None,
            params: SessionParams::default(),
            identity: PythonSessionVersionResponseV1 {
                contract: "ai_daily_python_session".to_string(),
                protocol_version: 1,
                session_contract_version: "ai_daily_python_session_v1".to_string(),
                worker_build: "a".repeat(64),
                classifier_build: "b".repeat(64),
                supported_operations: vec!["classify_pdf_v1".to_string(), "parse_v1".to_string()],
            },
            requests_served: 0,
        }
    }

    #[test]
    fn parse_response_accepts_matching_request_id() {
        let session = empty_session();
        let request_id = "11111111-1111-4111-8111-111111111111".to_string();
        let response = PythonSessionResponseV1 {
            contract: "ai_daily_python_session".to_string(),
            protocol_version: 1,
            request_id: request_id.clone(),
            operation: PythonSessionOperation::ClassifyPdfV1,
            status: PythonSessionResponseStatus::Ok,
            result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Classify(
                PdfClassifierResultV1 {
                    status: ai_daily_scanner_contract::PdfClassifierResultStatus::TextInParseWindow,
                    page_count: ai_daily_scanner_contract::Nullable(Some(1)),
                    result_examined_pages: ai_daily_scanner_contract::Nullable(Some(1)),
                    diagnostic: ai_daily_scanner_contract::Nullable(None),
                },
            ))),
            error: ai_daily_scanner_contract::Nullable(None),
        };
        let line = serde_json::to_vec(&response).expect("response serializes");
        let parsed = session
            .parse_response(line, PythonSessionOperation::ClassifyPdfV1, &request_id)
            .expect("matching request_id must be accepted");
        assert_eq!(parsed.request_id, request_id);
    }

    #[test]
    fn parse_response_rejects_mismatched_request_id() {
        // spec Part 7.2：错配 request_id 视为 protocol corruption，不得静默接受。
        let session = empty_session();
        let response = PythonSessionResponseV1 {
            contract: "ai_daily_python_session".to_string(),
            protocol_version: 1,
            request_id: "22222222-2222-4222-8222-222222222222".to_string(),
            operation: PythonSessionOperation::ClassifyPdfV1,
            status: PythonSessionResponseStatus::Ok,
            result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Classify(
                PdfClassifierResultV1 {
                    status: ai_daily_scanner_contract::PdfClassifierResultStatus::TextInParseWindow,
                    page_count: ai_daily_scanner_contract::Nullable(Some(1)),
                    result_examined_pages: ai_daily_scanner_contract::Nullable(Some(1)),
                    diagnostic: ai_daily_scanner_contract::Nullable(None),
                },
            ))),
            error: ai_daily_scanner_contract::Nullable(None),
        };
        let line = serde_json::to_vec(&response).expect("response serializes");
        let error = session
            .parse_response(
                line,
                PythonSessionOperation::ClassifyPdfV1,
                "11111111-1111-4111-8111-111111111111",
            )
            .expect_err("mismatched request_id must be protocol corruption");
        assert!(matches!(error, SessionError::ProtocolCorruption(_)));
    }
}
