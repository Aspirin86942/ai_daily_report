//! Long-lived streaming Python worker session（spec Part 7）。
//!
//! `ai_daily_python_session_v1` 是独立于共享 `ai_daily_worker_v1` 的 NDJSON
//! 流式契约：首帧 hello，每行一个请求 envelope、每行一个 typed response，
//! 单 in-flight、逐请求 deadline。每个 session child 拥有独立 Windows Job
//! Object（见 [`crate::windows_job::SessionJob`]），杀一个超时请求不会连带
//! 杀掉 pool 中其他 session。生命周期计数唯一为 `max_requests_per_session`；
//! `batch_size` 已删除。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use ai_daily_scanner_contract::{
    NormalizedScannerProfileV2, PdfClassifierRequestV1, PdfClassifierResultV1,
    PythonOperationDiagnosticV1, PythonSessionHelloV1, PythonSessionOperation,
    PythonSessionRequestV1, PythonSessionResponseStatus, PythonSessionResponseV1,
    PythonSessionResultV1, PythonSessionVersionResponseV1, Validate, WorkerParseRequest,
    WorkerParseResponse,
};

use crate::fallback::ParseFailure;
use crate::parsers::{OneShotExecution, RegisteredSession, RegisteredWorker, WorkerCommand};
use crate::process::WorkerRssTracker;

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
        Self {
            concurrency: usize::try_from(max_workers.min(4)).unwrap_or(4).max(1),
            ..Self::default()
        }
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
                file_path: ai_daily_scanner_contract::Nullable(file_path.map(str::to_string)),
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
        SessionError::Rejected(diagnostic)
            if diagnostic.error_code
                == ai_daily_scanner_contract::PythonOperationErrorCode::SourceVersionChanged =>
        {
            // The request embeds the stale source version. Rebuilding a
            // session and replaying the same request cannot make it valid.
            RetryAction::GiveUp
        }
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
    stderr_budget: Arc<StderrBudget>,
    rss_tracker: WorkerRssTracker,
    #[cfg(windows)]
    job: Option<crate::windows_job::SessionJob>,
    /// 上次请求完成时刻；idle TTL 按“空闲时间”回收（spec 7.3/8.1），不是
    /// 进程年龄——持续忙碌的 session 不应每 30s 被误回收。
    last_activity: Instant,
}

impl SessionChild {
    fn spawn(
        command: &WorkerCommand,
        rss_tracker: &WorkerRssTracker,
    ) -> Result<Self, SessionError> {
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
                    rss_tracker.observe_started_child(None);
                    return Err(SessionError::StartFailed);
                }
            }
        };
        let stdin = child.stdin.take().ok_or(SessionError::IoFailed)?;
        let stdout = child.stdout.take().ok_or(SessionError::IoFailed)?;
        let stderr = child.stderr.take().ok_or(SessionError::IoFailed)?;
        let (sender, receiver) = mpsc::channel();
        let stdout_sender = sender.clone();
        let stdout_thread = thread::spawn(move || read_stdout_lines(stdout, stdout_sender));
        let stderr_budget = Arc::new(StderrBudget::default());
        let stderr_reader_budget = stderr_budget.clone();
        let stderr_thread =
            thread::spawn(move || drain_stderr(stderr, sender, stderr_reader_budget));
        Ok(Self {
            child,
            stdin,
            lines: receiver,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr_budget,
            rss_tracker: rss_tracker.clone(),
            #[cfg(windows)]
            job,
            last_activity: Instant::now(),
        })
    }

    fn read_line(&self, timeout: Duration) -> Result<Vec<u8>, SessionError> {
        if self.stderr_budget.overflowed() {
            return Err(stderr_limit_error());
        }
        match self.lines.recv_timeout(timeout) {
            Ok(LineEvent::Line(_)) if self.stderr_budget.overflowed() => Err(stderr_limit_error()),
            Ok(LineEvent::Line(line)) => Ok(line),
            Ok(LineEvent::Eof) => Err(SessionError::Eof),
            Ok(LineEvent::Error(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Err(SessionError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(SessionError::Eof),
        }
    }

    fn write_line(&mut self, frame: &[u8]) -> Result<(), SessionError> {
        // stderr accounting is per in-flight request. Hello uses the initial
        // zeroed budget; every subsequent request starts a fresh 1 MiB window.
        self.stderr_budget.reset();
        self.stdin.write_all(frame).map_err(|_| SessionError::Eof)?;
        self.stdin.write_all(b"\n").map_err(|_| SessionError::Eof)?;
        self.stdin.flush().map_err(|_| SessionError::Eof)
    }

    fn recycle_due(&self, params: &SessionParams) -> bool {
        // Idle TTL measures elapsed time since the last completed request, not
        // process age. Peak Job memory is monotonic for one session generation,
        // which makes it a deterministic recycle signal after a full response.
        self.last_activity.elapsed() >= params.idle_ttl
            || self
                .peak_rss_bytes()
                .is_some_and(|rss| rss >= params.rss_limit_bytes)
    }

    fn peak_rss_bytes(&self) -> Option<u64> {
        #[cfg(windows)]
        {
            self.job.as_ref()?.peak_memory_bytes()
        }
        #[cfg(not(windows))]
        {
            None
        }
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
        self.rss_tracker
            .observe_started_child(self.peak_rss_bytes());
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

#[derive(Default)]
struct StderrBudget {
    bytes: AtomicUsize,
    overflowed: AtomicBool,
}

impl StderrBudget {
    fn reset(&self) {
        self.bytes.store(0, Ordering::Release);
        self.overflowed.store(false, Ordering::Release);
    }

    /// Returns true exactly once for the request that crosses the hard limit.
    fn observe(&self, count: usize) -> bool {
        let previous = self.bytes.fetch_add(count, Ordering::AcqRel);
        previous.saturating_add(count) > SESSION_STDERR_LIMIT
            && !self.overflowed.swap(true, Ordering::AcqRel)
    }

    fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }
}

fn stderr_limit_error() -> SessionError {
    SessionError::ProtocolCorruption(
        "worker session stderr exceeded the per-request 1 MiB limit".to_string(),
    )
}

fn drain_stderr(
    stderr: std::process::ChildStderr,
    sender: Sender<LineEvent>,
    budget: Arc<StderrBudget>,
) {
    let mut reader = stderr;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                if budget.observe(count) {
                    // Wake the in-flight reader immediately. Continue draining
                    // (discarding) until the pool receives this event and kills
                    // the Job child, so the worker cannot block on a full pipe
                    // and masquerade as a timeout.
                    let _ = sender.send(LineEvent::Error(stderr_limit_error()));
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
        Self::start_observed(
            command,
            expected,
            params,
            &WorkerRssTracker::default(),
            timeout,
        )
    }

    pub(crate) fn start_observed(
        command: &WorkerCommand,
        expected: &PythonSessionVersionResponseV1,
        params: SessionParams,
        rss_tracker: &WorkerRssTracker,
        timeout: Duration,
    ) -> Result<Self, SessionError> {
        let child = SessionChild::spawn(command, rss_tracker)?;
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
        let hello: PythonSessionHelloV1 = serde_json::from_slice(&line).map_err(|_| {
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

    /// Peak memory for this session generation's contained process tree.
    pub fn peak_rss_bytes(&self) -> Option<u64> {
        self.child.as_ref().and_then(SessionChild::peak_rss_bytes)
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
                result:
                    ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Classify(result))),
                ..
            } => {
                result
                    .validate_for_max_pages(request.max_pages)
                    .map_err(|message| {
                        SessionError::ProtocolCorruption(format!(
                            "session classifier result violates the request page window: {message}"
                        ))
                    })?;
                Ok(result)
            }
            PythonSessionResponseV1 {
                status: PythonSessionResponseStatus::Error,
                error: ai_daily_scanner_contract::Nullable(Some(diagnostic)),
                ..
            } => Err(SessionError::Rejected(diagnostic)),
            PythonSessionResponseV1 {
                result: ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Parse(_))),
                ..
            } => Err(SessionError::ProtocolCorruption(
                "classify response carried a parse result".to_string(),
            )),
            PythonSessionResponseV1 {
                result: ai_daily_scanner_contract::Nullable(None),
                ..
            } => Err(SessionError::ProtocolCorruption(
                "ok classify response is missing its typed result".to_string(),
            )),
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
                result:
                    ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Parse(result))),
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
                Ok(*result)
            }
            PythonSessionResponseV1 {
                status: PythonSessionResponseStatus::Error,
                error: ai_daily_scanner_contract::Nullable(Some(diagnostic)),
                ..
            } => Err(SessionError::Rejected(diagnostic)),
            PythonSessionResponseV1 {
                result:
                    ai_daily_scanner_contract::Nullable(Some(PythonSessionResultV1::Classify(_))),
                ..
            } => Err(SessionError::ProtocolCorruption(
                "parse response carried a classify result".to_string(),
            )),
            PythonSessionResponseV1 {
                result: ai_daily_scanner_contract::Nullable(None),
                ..
            } => Err(SessionError::ProtocolCorruption(
                "ok parse response is missing its typed result".to_string(),
            )),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonSessionTransport {
    Session,
    OneShot,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOperationOutcome<T> {
    pub value: T,
    pub transport: PythonSessionTransport,
    pub attempt_count: u64,
    pub duration_ms: u64,
}

#[derive(Debug)]
pub struct SessionOperationFailure {
    pub failure: ParseFailure,
    pub transport: PythonSessionTransport,
    pub attempt_count: u64,
    pub duration_ms: u64,
}

/// Run-level lifecycle counters owned by the shared Python session pool.
/// Operation attempts stay on each outcome/failure so scheduler metrics and
/// per-file provenance share one exact source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPoolStats {
    pub session_restart_count: u64,
    pub session_fallback_count: u64,
    /// `Some(0)` means no session child was started; `None` means Windows Job
    /// accounting was attempted and failed for at least one child.
    pub peak_worker_rss_bytes: Option<u64>,
}

#[derive(Default)]
struct PoolCounters {
    session_restart_count: AtomicU64,
    session_fallback_count: AtomicU64,
    peak_worker_rss_bytes: AtomicU64,
    rss_observation_attempted: AtomicBool,
    rss_observation_failed: AtomicBool,
}

impl PoolCounters {
    fn observe_rss(&self, value: Option<u64>) {
        self.rss_observation_attempted
            .store(true, Ordering::Relaxed);
        match value {
            Some(value) => {
                self.peak_worker_rss_bytes
                    .fetch_max(value, Ordering::Relaxed);
            }
            None => self.rss_observation_failed.store(true, Ordering::Relaxed),
        }
    }

    fn snapshot(&self) -> SessionPoolStats {
        let attempted = self.rss_observation_attempted.load(Ordering::Relaxed);
        let failed = self.rss_observation_failed.load(Ordering::Relaxed);
        SessionPoolStats {
            session_restart_count: self.session_restart_count.load(Ordering::Relaxed),
            session_fallback_count: self.session_fallback_count.load(Ordering::Relaxed),
            peak_worker_rss_bytes: if failed {
                None
            } else if attempted {
                Some(self.peak_worker_rss_bytes.load(Ordering::Relaxed))
            } else {
                Some(0)
            },
        }
    }
}

#[derive(Default)]
struct SessionSlot {
    session: Option<PythonSession>,
    /// A prior generation was retired or failed and the next successful start
    /// must be counted as one actual replacement.
    replacement_pending: bool,
}

/// Shared, bounded pool used by both PDF classification and PDF body parsing.
/// A slot is checked out for one complete logical operation, preserving the
/// frozen single-in-flight session contract while classifier/parser waves may
/// run concurrently across different slots.
pub struct PythonSessionPool {
    command: WorkerCommand,
    expected: PythonSessionVersionResponseV1,
    python_worker: RegisteredWorker,
    params: SessionParams,
    slots: Vec<Mutex<SessionSlot>>,
    available: Mutex<Vec<usize>>,
    available_changed: Condvar,
    counters: PoolCounters,
    rss_tracker: WorkerRssTracker,
}

impl PythonSessionPool {
    pub fn new(
        registered: RegisteredSession,
        python_worker: RegisteredWorker,
        params: SessionParams,
        rss_tracker: WorkerRssTracker,
    ) -> Arc<Self> {
        let concurrency = params.concurrency.max(1);
        Arc::new(Self {
            command: registered.command,
            expected: registered.identity,
            python_worker,
            params,
            slots: (0..concurrency)
                .map(|_| Mutex::new(SessionSlot::default()))
                .collect(),
            available: Mutex::new((0..concurrency).rev().collect()),
            available_changed: Condvar::new(),
            counters: PoolCounters::default(),
            rss_tracker,
        })
    }

    pub fn stats(&self) -> SessionPoolStats {
        for slot in &self.slots {
            if let Some(session) = lock_unpoison(slot).session.as_ref() {
                self.observe_rss(session.peak_rss_bytes());
            }
        }
        self.counters.snapshot()
    }

    fn observe_rss(&self, value: Option<u64>) {
        self.counters.observe_rss(value);
        self.rss_tracker.observe_started_child(value);
    }

    pub fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        timeout: Duration,
    ) -> Result<SessionOperationOutcome<PdfClassifierResultV1>, SessionOperationFailure> {
        if let Err(failure) = crate::parsers::classifier::validate_classifier_source_before(request)
        {
            return Err(SessionOperationFailure {
                failure,
                transport: PythonSessionTransport::NotApplicable,
                attempt_count: 0,
                duration_ms: 0,
            });
        }
        let result = self.execute_operation(
            &request.file_path,
            timeout,
            |session, remaining| session_classify(session, request, remaining),
            |remaining| {
                crate::parsers::classifier::classify_pdf_oneshot_observed(
                    &self.command,
                    request,
                    remaining,
                    Some(&self.rss_tracker),
                )
            },
        )?;
        if let Err(failure) = crate::parsers::classifier::validate_classifier_source_after(request)
        {
            return Err(SessionOperationFailure {
                failure,
                transport: result.transport,
                attempt_count: result.attempt_count,
                duration_ms: result.duration_ms,
            });
        }
        Ok(result)
    }

    pub fn parse_pdf(
        &self,
        request: &WorkerParseRequest,
        timeout: Duration,
    ) -> Result<SessionOperationOutcome<WorkerParseResponse>, SessionOperationFailure> {
        if let Err(failure) = crate::parsers::validate_worker_request(&self.python_worker, request)
        {
            return Err(SessionOperationFailure {
                failure,
                transport: PythonSessionTransport::NotApplicable,
                attempt_count: 0,
                duration_ms: 0,
            });
        }
        let response = self.execute_operation(
            &request.file_path,
            timeout,
            |session, remaining| {
                let mut attempt_request = request.clone();
                attempt_request.remaining_timeout_ms = duration_ms(remaining);
                session_parse(session, &attempt_request, remaining)
            },
            |remaining| {
                let mut attempt_request = request.clone();
                attempt_request.remaining_timeout_ms = duration_ms(remaining);
                crate::parsers::execute_worker_request_observed(
                    &self.python_worker,
                    &attempt_request,
                )
            },
        )?;
        let value = match crate::parsers::validate_session_worker_response(
            &self.python_worker,
            request,
            response.value,
        ) {
            Ok(value) => value,
            Err(failure) => {
                return Err(SessionOperationFailure {
                    failure,
                    transport: response.transport,
                    attempt_count: response.attempt_count,
                    duration_ms: response.duration_ms,
                });
            }
        };
        Ok(SessionOperationOutcome { value, ..response })
    }

    fn execute_operation<T, SessionCall, OneShotCall>(
        &self,
        file_path: &str,
        timeout: Duration,
        mut session_call: SessionCall,
        one_shot_call: OneShotCall,
    ) -> Result<SessionOperationOutcome<T>, SessionOperationFailure>
    where
        SessionCall: FnMut(&mut PythonSession, Duration) -> Result<T, SessionError>,
        OneShotCall: FnOnce(Duration) -> OneShotExecution<T>,
    {
        let deadline_origin = Instant::now();
        let permit = self.checkout();
        let mut slot = lock_unpoison(&self.slots[permit.index]);
        let mut transport_failure_index = 0_u32;
        let mut session_attempt_count = 0_u64;
        let mut session_duration_ms = 0_u64;
        let mut one_shot_call = Some(one_shot_call);

        loop {
            let remaining = timeout.saturating_sub(deadline_origin.elapsed());
            if remaining.is_zero() {
                if let Some(session) = slot.session.as_mut() {
                    self.observe_rss(session.peak_rss_bytes());
                    session.kill();
                    slot.replacement_pending = true;
                }
                return Err(operation_failure(
                    SessionError::Timeout.diagnostic(Some(file_path)),
                    transport_for_attempts(session_attempt_count, 0),
                    session_attempt_count,
                    session_duration_ms,
                ));
            }

            if slot
                .session
                .as_ref()
                .is_some_and(PythonSession::recycle_due)
            {
                if let Some(session) = slot.session.as_mut() {
                    self.observe_rss(session.peak_rss_bytes());
                    session.kill();
                }
                slot.session = None;
                slot.replacement_pending = true;
            }

            if slot.session.is_none() {
                match PythonSession::start_observed(
                    &self.command,
                    &self.expected,
                    self.params,
                    &self.rss_tracker,
                    remaining,
                ) {
                    Ok(session) => {
                        self.observe_rss(session.peak_rss_bytes());
                        if slot.replacement_pending {
                            self.counters
                                .session_restart_count
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        slot.replacement_pending = false;
                        slot.session = Some(session);
                    }
                    Err(error) => {
                        // A failed initial spawn/hello is not a replacement and
                        // did not start the file's logical operation. Preserve
                        // an existing replacement_pending bit only when a
                        // previously validated session was actually retired.
                        match retry_action(&error, transport_failure_index) {
                            RetryAction::RetrySession => {
                                transport_failure_index += 1;
                                continue;
                            }
                            RetryAction::OneShot => {
                                let remaining = timeout.saturating_sub(deadline_origin.elapsed());
                                if remaining.is_zero() {
                                    return Err(operation_failure(
                                        SessionError::Timeout.diagnostic(Some(file_path)),
                                        transport_for_attempts(session_attempt_count, 0),
                                        session_attempt_count,
                                        session_duration_ms,
                                    ));
                                }
                                let one_shot = one_shot_call
                                    .take()
                                    .expect("one-shot fallback is attempted at most once")(
                                    remaining,
                                );
                                return self.finish_one_shot_fallback(
                                    session_attempt_count,
                                    session_duration_ms,
                                    one_shot,
                                );
                            }
                            RetryAction::GiveUp => {
                                return Err(operation_failure(
                                    error.diagnostic(Some(file_path)),
                                    transport_for_attempts(session_attempt_count, 0),
                                    session_attempt_count,
                                    session_duration_ms,
                                ));
                            }
                        }
                    }
                }
            }

            let remaining = timeout.saturating_sub(deadline_origin.elapsed());
            if remaining.is_zero() {
                if let Some(session) = slot.session.as_mut() {
                    self.observe_rss(session.peak_rss_bytes());
                    session.kill();
                    slot.replacement_pending = true;
                }
                return Err(operation_failure(
                    SessionError::Timeout.diagnostic(Some(file_path)),
                    transport_for_attempts(session_attempt_count, 0),
                    session_attempt_count,
                    session_duration_ms,
                ));
            }
            let attempt_started = Instant::now();
            let outcome = session_call(
                slot.session.as_mut().expect("session was started above"),
                remaining,
            );
            session_attempt_count = session_attempt_count.saturating_add(1);
            session_duration_ms =
                session_duration_ms.saturating_add(observed_duration_ms(attempt_started.elapsed()));
            match outcome {
                Ok(result) => {
                    if let Some(session) = slot.session.as_ref() {
                        self.observe_rss(session.peak_rss_bytes());
                    }
                    return Ok(SessionOperationOutcome {
                        value: result,
                        transport: PythonSessionTransport::Session,
                        attempt_count: session_attempt_count,
                        duration_ms: session_duration_ms,
                    });
                }
                Err(error) => {
                    if let Some(session) = slot.session.as_mut() {
                        self.observe_rss(session.peak_rss_bytes());
                        session.kill();
                    }
                    slot.session = None;
                    slot.replacement_pending = true;
                    match retry_action(&error, transport_failure_index) {
                        RetryAction::RetrySession => {
                            transport_failure_index += 1;
                        }
                        RetryAction::OneShot => {
                            let remaining = timeout.saturating_sub(deadline_origin.elapsed());
                            if remaining.is_zero() {
                                return Err(operation_failure(
                                    SessionError::Timeout.diagnostic(Some(file_path)),
                                    PythonSessionTransport::Session,
                                    session_attempt_count,
                                    session_duration_ms,
                                ));
                            }
                            let one_shot = one_shot_call
                                .take()
                                .expect("one-shot fallback is attempted at most once")(
                                remaining
                            );
                            return self.finish_one_shot_fallback(
                                session_attempt_count,
                                session_duration_ms,
                                one_shot,
                            );
                        }
                        RetryAction::GiveUp => {
                            return Err(operation_failure(
                                error.diagnostic(Some(file_path)),
                                PythonSessionTransport::Session,
                                session_attempt_count,
                                session_duration_ms,
                            ));
                        }
                    }
                }
            }
        }
    }

    fn finish_one_shot_fallback<T>(
        &self,
        session_attempt_count: u64,
        session_duration_ms: u64,
        one_shot: OneShotExecution<T>,
    ) -> Result<SessionOperationOutcome<T>, SessionOperationFailure> {
        if one_shot.attempt_count > 0 {
            self.counters
                .session_fallback_count
                .fetch_add(1, Ordering::Relaxed);
        }
        let attempt_count = session_attempt_count.saturating_add(one_shot.attempt_count);
        let duration_ms = session_duration_ms.saturating_add(one_shot.duration_ms);
        let transport = transport_for_attempts(session_attempt_count, one_shot.attempt_count);
        match one_shot.outcome {
            Ok(value) => Ok(SessionOperationOutcome {
                value,
                transport,
                attempt_count,
                duration_ms,
            }),
            Err(failure) => Err(SessionOperationFailure {
                failure,
                transport,
                attempt_count,
                duration_ms,
            }),
        }
    }

    fn checkout(&self) -> SlotPermit<'_> {
        let mut available = lock_unpoison(&self.available);
        loop {
            if let Some(index) = available.pop() {
                return SlotPermit { pool: self, index };
            }
            available = self
                .available_changed
                .wait(available)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

fn operation_failure(
    failure: ParseFailure,
    transport: PythonSessionTransport,
    attempt_count: u64,
    duration_ms: u64,
) -> SessionOperationFailure {
    SessionOperationFailure {
        failure,
        transport,
        attempt_count,
        duration_ms,
    }
}

fn transport_for_attempts(
    session_attempt_count: u64,
    one_shot_attempt_count: u64,
) -> PythonSessionTransport {
    if one_shot_attempt_count > 0 {
        PythonSessionTransport::OneShot
    } else if session_attempt_count > 0 {
        PythonSessionTransport::Session
    } else {
        PythonSessionTransport::NotApplicable
    }
}

fn observed_duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

struct SlotPermit<'a> {
    pool: &'a PythonSessionPool,
    index: usize,
}

impl Drop for SlotPermit<'_> {
    fn drop(&mut self) {
        let mut available = lock_unpoison(&self.pool.available);
        available.push(self.index);
        self.pool.available_changed.notify_one();
    }
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().max(1).min(u64::MAX as u128) as u64
}

/// 构造一次 ``classify_pdf_v1`` 的 strict request（与 one-shot 逐字段相同）。
pub fn build_classify_request(
    request_id: String,
    file_path: &Path,
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
        assert_eq!(SessionParams::with_default_concurrency(8).concurrency, 4);
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

        let source_changed = SessionError::Rejected(PythonOperationDiagnosticV1 {
            error_code: ai_daily_scanner_contract::PythonOperationErrorCode::SourceVersionChanged,
            message: "source changed".to_string(),
            retryable: true,
            stage: ai_daily_scanner_contract::PythonOperationStage::Parse,
            file_path: ai_daily_scanner_contract::Nullable(None),
            backend: ai_daily_scanner_contract::Nullable(None),
        });
        assert_eq!(retry_action(&source_changed, 0), RetryAction::GiveUp);
        assert_eq!(retry_action(&source_changed, 1), RetryAction::GiveUp);

        // EOF/start/crash: rebuild once, then one-shot if retryable.
        assert_eq!(
            retry_action(&SessionError::Eof, 0),
            RetryAction::RetrySession
        );
        assert_eq!(retry_action(&SessionError::Eof, 1), RetryAction::OneShot);
        assert_eq!(
            retry_action(&SessionError::StartFailed, 0),
            RetryAction::RetrySession
        );
        assert_eq!(
            retry_action(&SessionError::Crashed, 0),
            RetryAction::RetrySession
        );

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
    fn start_failures_and_unstarted_oneshot_do_not_fabricate_attempts() {
        let directory = tempfile::tempdir().expect("temporary session root");
        let command = WorkerCommand {
            program: directory.path().join("missing-worker.exe"),
            base_args: Vec::new(),
            current_dir: None,
            expected_kind: ai_daily_scanner_contract::WorkerKind::PythonDocument,
            required_backends: vec!["pdf_text_v1".to_string()],
            required_extensions: vec![".pdf".to_string()],
        };
        let worker_build = "a".repeat(64);
        let classifier_build = "b".repeat(64);
        let registered = RegisteredSession {
            command: command.clone(),
            identity: PythonSessionVersionResponseV1 {
                contract: "ai_daily_python_session".to_string(),
                protocol_version: 1,
                session_contract_version: SESSION_CONTRACT_VERSION.to_string(),
                worker_build: worker_build.clone(),
                classifier_build,
                supported_operations: vec!["classify_pdf_v1".to_string(), "parse_v1".to_string()],
            },
        };
        let python_worker = RegisteredWorker {
            command,
            identity: ai_daily_scanner_contract::WorkerVersionResponse {
                contract: "ai_daily_worker".to_string(),
                protocol_version: 1,
                worker_kind: ai_daily_scanner_contract::WorkerKind::PythonDocument,
                worker_contract_version: "ai_daily_worker_v1".to_string(),
                worker_version: "0.1.0".to_string(),
                worker_build,
                supported_backends: vec!["pdf_text_v1".to_string()],
                supported_extensions: vec![".pdf".to_string()],
            },
            rss_tracker: None,
        };
        let pool = PythonSessionPool::new(
            registered,
            python_worker,
            SessionParams {
                concurrency: 1,
                ..SessionParams::default()
            },
            WorkerRssTracker::default(),
        );
        let fallback_failure = SessionError::StartFailed.diagnostic(Some("C:\\fixture.pdf"));

        let failure = pool
            .execute_operation(
                "C:\\fixture.pdf",
                Duration::from_secs(1),
                |_, _| Ok(()),
                |_| OneShotExecution {
                    outcome: Err(fallback_failure),
                    attempt_count: 0,
                    duration_ms: 0,
                },
            )
            .expect_err("both transports fail before a logical operation starts");

        assert_eq!(failure.transport, PythonSessionTransport::NotApplicable);
        assert_eq!(failure.attempt_count, 0);
        assert_eq!(failure.duration_ms, 0);
        assert_eq!(pool.stats().session_restart_count, 0);
        assert_eq!(pool.stats().session_fallback_count, 0);
    }

    #[test]
    fn stderr_budget_is_per_request_and_reports_the_first_overflow() {
        let budget = StderrBudget::default();
        assert!(!budget.observe(SESSION_STDERR_LIMIT));
        assert!(budget.observe(1));
        assert!(budget.overflowed());
        assert!(!budget.observe(1));

        budget.reset();
        assert!(!budget.overflowed());
        assert!(!budget.observe(SESSION_STDERR_LIMIT));
    }

    #[test]
    fn hello_stderr_overflow_is_reported_without_pipe_deadlock() {
        let python = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join(if cfg!(windows) {
                ".venv/Scripts/python.exe"
            } else {
                ".venv/bin/python"
            });
        if !python.is_file() {
            return;
        }
        let worker_build = "a".repeat(64);
        let classifier_build = "b".repeat(64);
        let script = format!(
            "import json,sys,time; sys.stderr.write('x'*{}); sys.stderr.flush(); time.sleep(0.1); print(json.dumps({{'contract':'ai_daily_python_session','protocol_version':1,'frame':'hello','session_contract_version':'ai_daily_python_session_v1','worker_build':'{}','classifier_build':'{}','supported_operations':['classify_pdf_v1','parse_v1']}}), flush=True); time.sleep(30)",
            SESSION_STDERR_LIMIT + 1,
            worker_build,
            classifier_build,
        );
        let command = WorkerCommand {
            program: python,
            base_args: vec!["-c".into(), script.into()],
            current_dir: None,
            expected_kind: ai_daily_scanner_contract::WorkerKind::PythonDocument,
            required_backends: Vec::new(),
            required_extensions: Vec::new(),
        };
        let expected = PythonSessionVersionResponseV1 {
            contract: "ai_daily_python_session".to_string(),
            protocol_version: 1,
            session_contract_version: SESSION_CONTRACT_VERSION.to_string(),
            worker_build,
            classifier_build,
            supported_operations: vec!["classify_pdf_v1".to_string(), "parse_v1".to_string()],
        };
        let started = Instant::now();

        let error = match PythonSession::start(
            &command,
            &expected,
            SessionParams::default(),
            Duration::from_secs(10),
        ) {
            Ok(_) => panic!("stderr overflow must terminate the session handshake"),
            Err(error) => error,
        };

        assert!(matches!(error, SessionError::ProtocolCorruption(_)));
        assert!(started.elapsed() < Duration::from_secs(5));
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
        assert_eq!(failure.diagnostic.file_path.0.as_deref(), Some("C:\\x.pdf"));
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
