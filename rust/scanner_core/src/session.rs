//! Long-lived streaming worker pool for crash-isolated parsers.
//!
//! Office/PDF worker 共用 NDJSON 流式契约：首帧
//! hello，每行一个 request/response envelope，
//! 单 in-flight、逐请求 deadline。每个 session child 拥有独立 Windows Job
//! Object（见 [`crate::windows_job::SessionJob`]），杀一个超时请求不会连带
//! 杀掉 pool 中其他 session。生命周期计数唯一为 `max_requests_per_session`；
//! `batch_size` 已删除。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use ai_daily_scanner_contract::NormalizedScannerSettings;
use ai_daily_worker_contract::{
    ClassifyRequest, ClassifyResult, ParseResult, ParserBackend, WorkerDiagnostic, WorkerHello,
    WorkerOperation, WorkerRequest, WorkerResponse, WorkerResponseStatus, CONTRACT,
    CONTRACT_VERSION, PROTOCOL_VERSION,
};

use crate::fallback::ParseFailure;
use crate::parsers::classifier::ClassifyOperation;
use crate::parsers::{ParseOperation, RegisteredWorker, WorkerCommand};
use crate::process::WorkerRssTracker;

pub const SESSION_CONTRACT_VERSION: &str = CONTRACT_VERSION;
pub const SESSION_PROTOCOL_VERSION: u64 = PROTOCOL_VERSION;

/// spec Part 7.2：hello/request/classification response 每 frame 1 MiB。
pub const SESSION_HELLO_FRAME_LIMIT: usize = 1024 * 1024;
pub const SESSION_REQUEST_FRAME_LIMIT: usize = 1024 * 1024;
pub const SESSION_CLASSIFY_FRAME_LIMIT: usize = 1024 * 1024;
/// stderr 由独立 reader 持续排空，每个 in-flight request 累计最多 1 MiB。
pub const SESSION_STDERR_LIMIT: usize = 1024 * 1024;
/// 读取线程的硬上限，防止单帧把父进程内存打爆；具体 frame 限制由客户端
/// 在收到后按 operation 再次校验。
const SESSION_STDOUT_HARD_CAP: usize = 64 * 1024 * 1024;

/// spec Part 7.3：PDF classify 与 PDF parse 各自独立 attempt 上限 3，
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
            concurrency: 2,
            max_requests_per_session: 128,
            idle_ttl: Duration::from_secs(30),
            rss_limit_bytes: 512 * 1024 * 1024,
        }
    }
}

impl SessionParams {
    /// Worker pool concurrency is derived from the single max_workers setting.
    pub fn with_default_concurrency(max_workers: u64) -> Self {
        Self {
            concurrency: max_workers.clamp(1, 2) as usize,
            ..Self::default()
        }
    }

    pub fn from_settings(profile: &NormalizedScannerSettings) -> Self {
        Self {
            max_requests_per_session: profile.worker_max_requests,
            idle_ttl: Duration::from_millis(profile.worker_idle_ttl_ms),
            rss_limit_bytes: profile.worker_rss_limit_bytes,
            ..Self::with_default_concurrency(profile.execution.max_workers)
        }
    }
}

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

enum LineEvent {
    Line(Vec<u8>),
    Eof,
    Error(SessionError),
}

/// 一个持久 session 子进程：独立 Job Object + stdin + 行式 stdout reader +
/// 持续排空的 stderr reader。
struct SessionChild {
    child: Child,
    requests: Sender<Vec<u8>>,
    lines: Receiver<LineEvent>,
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
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // The protocol uses redirected pipes. A detached worker needs no console host.
            builder.creation_flags(windows_sys::Win32::System::Threading::DETACHED_PROCESS);
        }
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
        let mut stdin = child.stdin.take().ok_or(SessionError::IoFailed)?;
        let stdout = child.stdout.take().ok_or(SessionError::IoFailed)?;
        let stderr = child.stderr.take().ok_or(SessionError::IoFailed)?;
        let (sender, receiver) = mpsc::channel();
        // Pipe writes can block if a worker stops reading. The caller waits on
        // the response/error channel under the same deadline as the write.
        let (requests, frames) = mpsc::channel::<Vec<u8>>();
        let write_errors = sender.clone();
        thread::spawn(move || {
            for frame in frames {
                if stdin.write_all(&frame).and_then(|_| stdin.flush()).is_err() {
                    let _ = write_errors.send(LineEvent::Error(SessionError::Eof));
                    break;
                }
            }
        });
        let stdout_sender = sender.clone();
        thread::spawn(move || read_stdout_lines(stdout, stdout_sender));
        let stderr_budget = Arc::new(StderrBudget::default());
        let stderr_reader_budget = stderr_budget.clone();
        thread::spawn(move || drain_stderr(stderr, sender, stderr_reader_budget));
        Ok(Self {
            child,
            requests,
            lines: receiver,
            stderr_budget,
            rss_tracker: rss_tracker.clone(),
            #[cfg(windows)]
            job,
            last_activity: Instant::now(),
        })
    }

    fn read_line(&self, timeout: Duration) -> Result<Vec<u8>, SessionError> {
        if timeout.is_zero() {
            return Err(SessionError::Timeout);
        }
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

    fn write_line(&mut self, mut frame: Vec<u8>) -> Result<(), SessionError> {
        // stderr accounting is per in-flight request. Hello uses the initial
        // zeroed budget; every subsequent request starts a fresh 1 MiB window.
        self.stderr_budget.reset();
        frame.push(b'\n');
        self.requests.send(frame).map_err(|_| SessionError::Eof)
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
pub struct WorkerSession {
    child: Option<SessionChild>,
    params: SessionParams,
    identity: WorkerHello,
    requests_served: u64,
}

impl WorkerSession {
    pub fn limit_memory(&self, bytes: u64) -> Result<(), SessionError> {
        #[cfg(windows)]
        {
            self.child
                .as_ref()
                .and_then(|c| c.job.as_ref())
                .ok_or(SessionError::StartFailed)?
                .limit_memory(bytes)
                .map_err(|_| SessionError::StartFailed)?;
        }
        #[cfg(not(windows))]
        {
            let _ = bytes;
        }
        Ok(())
    }
    pub fn archive_text(
        &mut self,
        request_id: &str,
        request: &ai_daily_worker_contract::archive::ArchiveTextRequest,
        timeout: Duration,
    ) -> Result<ai_daily_worker_contract::archive::ArchiveArtifact, SessionError> {
        request
            .validate()
            .map_err(SessionError::ProtocolCorruption)?;
        self.dispatch_archive(
            request_id,
            WorkerOperation::ArchiveTextParse,
            serde_json::to_value(request).map_err(|_| SessionError::IoFailed)?,
            timeout,
        )
    }
    pub fn archive_parse(
        &mut self,
        request_id: &str,
        request: &ai_daily_worker_contract::archive::ArchiveRequest,
        timeout: Duration,
    ) -> Result<ai_daily_worker_contract::archive::ArchiveArtifact, SessionError> {
        request
            .validate()
            .map_err(SessionError::ProtocolCorruption)?;
        let operation = if request.source.backend == ParserBackend::RustPdfiumV3 {
            WorkerOperation::ArchivePdfParse
        } else {
            WorkerOperation::ArchiveOfficeParse
        };
        self.dispatch_archive(
            request_id,
            operation,
            serde_json::to_value(request).map_err(|_| SessionError::IoFailed)?,
            timeout,
        )
    }
    fn dispatch_archive(
        &mut self,
        request_id: &str,
        operation: WorkerOperation,
        payload: serde_json::Value,
        timeout: Duration,
    ) -> Result<ai_daily_worker_contract::archive::ArchiveArtifact, SessionError> {
        let started = Instant::now();
        let envelope = WorkerRequest {
            contract: CONTRACT.into(),
            protocol_version: SESSION_PROTOCOL_VERSION,
            request_id: request_id.into(),
            operation,
            payload,
        };
        envelope
            .validate()
            .map_err(SessionError::ProtocolCorruption)?;
        self.write_request(serde_json::to_vec(&envelope).map_err(|_| SessionError::IoFailed)?)?;
        let line = self
            .child
            .as_ref()
            .ok_or(SessionError::Eof)?
            .read_line(timeout.saturating_sub(started.elapsed()))?;
        if line.len() > 1024 * 1024 {
            return Err(SessionError::FrameTooLarge);
        }
        let response = self.parse_response(line, operation, request_id)?;
        self.mark_request_complete();
        match response {
            WorkerResponse {
                status: WorkerResponseStatus::Ok,
                result: Some(value),
                ..
            } => serde_json::from_value(value)
                .map_err(|_| SessionError::ProtocolCorruption("archive result is invalid".into())),
            WorkerResponse {
                error: Some(value), ..
            } => Err(SessionError::Rejected(Box::new(value))),
            _ => Err(SessionError::ProtocolCorruption(
                "archive result is missing".into(),
            )),
        }
    }

    /// 启动会话并严格校验首帧 hello；build 必须与 preflight 完全一致。
    pub fn start(
        command: &WorkerCommand,
        expected: &WorkerHello,
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
        expected: &WorkerHello,
        params: SessionParams,
        rss_tracker: &WorkerRssTracker,
        timeout: Duration,
    ) -> Result<Self, SessionError> {
        let started = Instant::now();
        let child = SessionChild::spawn(command, rss_tracker)?;
        let mut session = Self {
            child: Some(child),
            params,
            identity: expected.clone(),
            requests_served: 0,
        };
        let hello = session.read_hello(timeout.saturating_sub(started.elapsed()))?;
        if hello != *expected {
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

    fn read_hello(&mut self, timeout: Duration) -> Result<WorkerHello, SessionError> {
        let child = self.child.as_ref().ok_or(SessionError::Eof)?;
        let line = child.read_line(timeout)?;
        if line.len() > SESSION_HELLO_FRAME_LIMIT {
            return Err(SessionError::FrameTooLarge);
        }
        let hello: WorkerHello = serde_json::from_slice(&line).map_err(|_| {
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

    pub fn identity(&self) -> &WorkerHello {
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
        drop(self.child.take());
    }

    fn dispatch_classify(
        &mut self,
        request: &ClassifyOperation,
        timeout: Duration,
    ) -> Result<ClassifyResult, SessionError> {
        let started = Instant::now();
        request.validate().map_err(|message| {
            SessionError::ProtocolCorruption(format!(
                "classify request violates the strict contract: {message}"
            ))
        })?;
        let envelope = WorkerRequest {
            contract: CONTRACT.to_string(),
            protocol_version: SESSION_PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            operation: WorkerOperation::PdfClassify,
            payload: serde_json::to_value(&request.payload).map_err(|_| SessionError::IoFailed)?,
        };
        let frame = serde_json::to_vec(&envelope).map_err(|_| SessionError::IoFailed)?;
        self.write_request(frame)?;
        let child = self.child.as_ref().ok_or(SessionError::Eof)?;
        let line = child.read_line(timeout.saturating_sub(started.elapsed()))?;
        if line.len() > SESSION_CLASSIFY_FRAME_LIMIT {
            return Err(SessionError::FrameTooLarge);
        }
        let response =
            self.parse_response(line, WorkerOperation::PdfClassify, &request.request_id)?;
        self.mark_request_complete();
        match response {
            WorkerResponse {
                status: WorkerResponseStatus::Ok,
                result: Some(result),
                ..
            } => {
                let result: ClassifyResult = serde_json::from_value(result).map_err(|_| {
                    SessionError::ProtocolCorruption(
                        "classify response result is not a classifier result".to_string(),
                    )
                })?;
                result
                    .validate_for_max_pages(request.max_pages)
                    .map_err(|message| {
                        SessionError::ProtocolCorruption(format!(
                            "session classifier result violates the request page window: {message}"
                        ))
                    })?;
                Ok(result)
            }
            WorkerResponse {
                status: WorkerResponseStatus::Error,
                error: Some(diagnostic),
                ..
            } => Err(SessionError::Rejected(Box::new(diagnostic))),
            _ => Err(SessionError::ProtocolCorruption(
                "classify response violates the ok/error tagged union".to_string(),
            )),
        }
    }

    fn dispatch_parse(
        &mut self,
        request: &ParseOperation,
        timeout: Duration,
    ) -> Result<ParseResult, SessionError> {
        let started = Instant::now();
        request.validate().map_err(|message| {
            SessionError::ProtocolCorruption(format!(
                "parse request violates the strict contract: {message}"
            ))
        })?;
        let envelope = WorkerRequest {
            contract: CONTRACT.to_string(),
            protocol_version: SESSION_PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            operation: worker_operation(request.backend),
            payload: serde_json::to_value(&request.payload).map_err(|_| SessionError::IoFailed)?,
        };
        let frame = serde_json::to_vec(&envelope).map_err(|_| SessionError::IoFailed)?;
        self.write_request(frame)?;
        let capture_limit = crate::parsers::worker_response_capture_limit(request)
            .map_err(|_| SessionError::IoFailed)?;
        let child = self.child.as_ref().ok_or(SessionError::Eof)?;
        let line = child.read_line(timeout.saturating_sub(started.elapsed()))?;
        if line.len() > capture_limit {
            return Err(SessionError::FrameTooLarge);
        }
        let response =
            self.parse_response(line, worker_operation(request.backend), &request.request_id)?;
        self.mark_request_complete();
        match response {
            WorkerResponse {
                status: WorkerResponseStatus::Ok,
                result: Some(result),
                ..
            } => {
                let result: ParseResult = serde_json::from_value(result).map_err(|_| {
                    SessionError::ProtocolCorruption(
                        "parse response result is not a parse result".to_string(),
                    )
                })?;
                result.validate().map_err(|message| {
                    SessionError::ProtocolCorruption(format!(
                        "session parse result violates the domain contract: {message}"
                    ))
                })?;
                Ok(result)
            }
            WorkerResponse {
                status: WorkerResponseStatus::Error,
                error: Some(diagnostic),
                ..
            } => Err(SessionError::Rejected(Box::new(diagnostic))),
            _ => Err(SessionError::ProtocolCorruption(
                "parse response violates the ok/error tagged union".to_string(),
            )),
        }
    }

    /// 写请求帧；写失败视为会话死亡（EOF/协议损坏）。
    fn write_request(&mut self, frame: Vec<u8>) -> Result<(), SessionError> {
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
        expected_operation: WorkerOperation,
        expected_request_id: &str,
    ) -> Result<WorkerResponse, SessionError> {
        let response: WorkerResponse = serde_json::from_slice(&line).map_err(|_| {
            SessionError::ProtocolCorruption(
                "session stdout is not one strict JSON response".to_string(),
            )
        })?;
        response.validate().map_err(|message| {
            SessionError::ProtocolCorruption(format!(
                "session response violates the strict contract: {message}"
            ))
        })?;
        if response.contract != CONTRACT
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
    session: &mut WorkerSession,
    request: &ClassifyOperation,
    timeout: Duration,
) -> Result<ClassifyResult, SessionError> {
    session.dispatch_classify(request, timeout)
}

/// 便捷 wrapper：返回 typed parse 结果或 session 层失败（spec Part 7.2）。
pub fn session_parse(
    session: &mut WorkerSession,
    request: &ParseOperation,
    timeout: Duration,
) -> Result<ParseResult, SessionError> {
    session.dispatch_parse(request, timeout)
}

fn worker_operation(backend: ParserBackend) -> WorkerOperation {
    match backend {
        ParserBackend::RustPdfiumV3 => WorkerOperation::PdfParse,
        _ => WorkerOperation::OfficeParse,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTransport {
    Session,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOperationOutcome<T> {
    pub value: T,
    pub transport: WorkerTransport,
    pub attempt_count: u64,
    pub duration_ms: u64,
}

#[derive(Debug)]
pub struct SessionOperationFailure {
    pub failure: ParseFailure,
    pub transport: WorkerTransport,
    pub attempt_count: u64,
    pub duration_ms: u64,
}

/// Lifecycle counters for the worker sessions.
/// Operation attempts stay on each outcome/failure so scheduler metrics and
/// per-file provenance share one exact source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPoolStats {
    pub session_restart_count: u64,
    /// `Some(0)` means no session child was started; `None` means Windows Job
    /// accounting was attempted and failed for at least one child.
    pub peak_worker_rss_bytes: Option<u64>,
}

impl SessionPoolStats {
    pub fn combine(left: Self, right: Self) -> Self {
        Self {
            session_restart_count: left
                .session_restart_count
                .saturating_add(right.session_restart_count),
            peak_worker_rss_bytes: match (left.peak_worker_rss_bytes, right.peak_worker_rss_bytes) {
                (Some(left), Some(right)) => Some(left.max(right)),
                _ => None,
            },
        }
    }
}

#[derive(Default)]
struct SessionSlot {
    session: Option<WorkerSession>,
    /// A prior generation was retired or failed and the next successful start
    /// must be counted as one actual replacement.
    replacement_pending: bool,
}

/// A slot owns at most one live process, including between requests. Both
/// worker kinds draw from the same slots for a scan.
pub(crate) struct SessionSlots {
    idle: Mutex<Vec<SessionSlot>>,
    changed: Condvar,
}

impl SessionSlots {
    pub(crate) fn new(concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            idle: Mutex::new(
                (0..concurrency.clamp(1, 2))
                    .map(|_| SessionSlot::default())
                    .collect(),
            ),
            changed: Condvar::new(),
        })
    }

    fn checkout(
        &self,
        expected: &WorkerHello,
        started: Instant,
        timeout: Duration,
    ) -> Option<SlotPermit<'_>> {
        let mut idle = lock_unpoison(&self.idle);
        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .filter(|t| !t.is_zero())?;
            if !idle.is_empty() {
                // Reuse a matching process first, then an empty slot. Only a
                // full pool needs to retire a process belonging to the other lane.
                let index = idle
                    .iter()
                    .rposition(|s| s.session.as_ref().is_some_and(|s| s.identity() == expected))
                    .or_else(|| idle.iter().rposition(|s| s.session.is_none()))
                    .unwrap_or(0);
                return Some(SlotPermit {
                    slots: self,
                    slot: idle.swap_remove(index),
                });
            }
            (idle, _) = self
                .changed
                .wait_timeout(idle, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

pub struct WorkerPool {
    command: WorkerCommand,
    expected: WorkerHello,
    worker: RegisteredWorker,
    params: SessionParams,
    slots: Arc<SessionSlots>,
    session_restart_count: AtomicU64,
    pool_rss: WorkerRssTracker,
    rss_tracker: WorkerRssTracker,
}

impl WorkerPool {
    pub fn new(
        command: WorkerCommand,
        expected: WorkerHello,
        worker: RegisteredWorker,
        params: SessionParams,
        rss_tracker: WorkerRssTracker,
    ) -> Arc<Self> {
        Self::with_slots(
            command,
            expected,
            worker,
            params,
            rss_tracker,
            SessionSlots::new(params.concurrency),
        )
    }

    pub(crate) fn with_slots(
        command: WorkerCommand,
        expected: WorkerHello,
        worker: RegisteredWorker,
        params: SessionParams,
        rss_tracker: WorkerRssTracker,
        slots: Arc<SessionSlots>,
    ) -> Arc<Self> {
        Arc::new(Self {
            command,
            expected,
            worker,
            params,
            slots,
            session_restart_count: AtomicU64::new(0),
            pool_rss: WorkerRssTracker::default(),
            rss_tracker,
        })
    }

    pub fn stats(&self) -> SessionPoolStats {
        for slot in lock_unpoison(&self.slots.idle).iter() {
            if let Some(session) = slot
                .session
                .as_ref()
                .filter(|s| s.identity() == &self.expected)
            {
                self.observe_rss(session.peak_rss_bytes());
            }
        }
        SessionPoolStats {
            session_restart_count: self.session_restart_count.load(Ordering::Relaxed),
            peak_worker_rss_bytes: self.pool_rss.peak_worker_rss_bytes(),
        }
    }

    pub(crate) fn matches(
        &self,
        command: &WorkerCommand,
        expected: &WorkerHello,
        worker: &RegisteredWorker,
        params: SessionParams,
    ) -> bool {
        self.command == *command
            && self.expected == *expected
            && self.worker == *worker
            && self.params == params
    }

    pub(crate) fn registered_worker(&self, command: &WorkerCommand) -> Option<RegisteredWorker> {
        (self.command == *command).then(|| self.worker.clone())
    }

    fn observe_rss(&self, value: Option<u64>) {
        self.pool_rss.observe_started_child(value);
        self.rss_tracker.observe_started_child(value);
    }

    pub fn classify_pdf(
        &self,
        request: &ClassifyOperation,
        timeout: Duration,
    ) -> Result<SessionOperationOutcome<ClassifyResult>, SessionOperationFailure> {
        let started = Instant::now();
        if let Err(failure) = crate::parsers::classifier::validate_classifier_source_before(request)
        {
            return Err(SessionOperationFailure {
                failure,
                transport: WorkerTransport::NotApplicable,
                attempt_count: 0,
                duration_ms: 0,
            });
        }
        let result = self.execute_operation(
            &request.file_path,
            timeout.saturating_sub(started.elapsed()),
            |session, remaining| session_classify(session, request, remaining),
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
        request: &ParseOperation,
        timeout: Duration,
    ) -> Result<SessionOperationOutcome<ParseResult>, SessionOperationFailure> {
        self.parse_worker(request, timeout)
    }

    pub fn parse_worker(
        &self,
        request: &ParseOperation,
        timeout: Duration,
    ) -> Result<SessionOperationOutcome<ParseResult>, SessionOperationFailure> {
        let started = Instant::now();
        let timeout = if matches!(
            request.parser_limits,
            ai_daily_worker_contract::ParserLimits::PdfImageOcr { .. }
        ) {
            timeout.min(Duration::from_millis(
                request
                    .remaining_timeout_ms
                    .min(ai_daily_worker_contract::OCR_TIMEOUT_MS),
            ))
        } else {
            timeout
        };
        if let Err(failure) = crate::parsers::validate_worker_request(&self.worker, request) {
            return Err(SessionOperationFailure {
                failure,
                transport: WorkerTransport::NotApplicable,
                attempt_count: 0,
                duration_ms: 0,
            });
        }
        let response = self.execute_operation(
            &request.file_path,
            timeout.saturating_sub(started.elapsed()),
            |session, remaining| {
                let mut attempt_request = request.clone();
                attempt_request.remaining_timeout_ms = duration_ms(remaining);
                session_parse(session, &attempt_request, remaining)
            },
        )?;
        let value =
            crate::parsers::validate_session_worker_response(&self.worker, request, response.value)
                .map_err(|failure| SessionOperationFailure {
                    failure,
                    transport: response.transport,
                    attempt_count: response.attempt_count,
                    duration_ms: response.duration_ms,
                })?;
        Ok(SessionOperationOutcome { value, ..response })
    }

    fn execute_operation<T, SessionCall>(
        &self,
        file_path: &str,
        timeout: Duration,
        mut session_call: SessionCall,
    ) -> Result<SessionOperationOutcome<T>, SessionOperationFailure>
    where
        SessionCall: FnMut(&mut WorkerSession, Duration) -> Result<T, SessionError>,
    {
        let deadline_origin = Instant::now();
        let timeout_failure = |attempts, duration| {
            operation_failure(
                SessionError::Timeout.diagnostic(Some(file_path)),
                transport_for_attempts(attempts),
                attempts,
                duration,
            )
        };
        let remaining = |attempts, duration| {
            timeout
                .checked_sub(deadline_origin.elapsed())
                .filter(|left| !left.is_zero())
                .ok_or_else(|| timeout_failure(attempts, duration))
        };
        let mut permit = self
            .slots
            .checkout(&self.expected, deadline_origin, timeout)
            .ok_or_else(|| timeout_failure(0, 0))?;
        let slot = &mut permit.slot;
        let mut transport_failure_index = 0_u32;
        let mut session_attempt_count = 0_u64;
        let mut session_duration_ms = 0_u64;

        loop {
            if slot
                .session
                .as_ref()
                .is_some_and(|s| s.identity() != &self.expected || s.recycle_due())
            {
                drop(slot.session.take());
                slot.replacement_pending = true;
            }

            if slot.session.is_none() {
                match WorkerSession::start_observed(
                    &self.command,
                    &self.expected,
                    self.params,
                    &self.rss_tracker,
                    remaining(session_attempt_count, session_duration_ms)?,
                ) {
                    Ok(session) => {
                        self.observe_rss(session.peak_rss_bytes());
                        if slot.replacement_pending {
                            self.session_restart_count.fetch_add(1, Ordering::Relaxed);
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
                            RetryAction::GiveUp => {
                                return Err(operation_failure(
                                    error.diagnostic(Some(file_path)),
                                    transport_for_attempts(session_attempt_count),
                                    session_attempt_count,
                                    session_duration_ms,
                                ));
                            }
                        }
                    }
                }
            }

            let time_left = remaining(session_attempt_count, session_duration_ms)?;
            let attempt_started = Instant::now();
            let outcome = session_call(
                slot.session.as_mut().expect("session was started above"),
                time_left,
            );
            let outcome = if deadline_origin.elapsed() >= timeout {
                Err(SessionError::Timeout)
            } else {
                outcome
            };
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
                        transport: WorkerTransport::Session,
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
                        RetryAction::GiveUp => {
                            return Err(operation_failure(
                                error.diagnostic(Some(file_path)),
                                WorkerTransport::Session,
                                session_attempt_count,
                                session_duration_ms,
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn operation_failure(
    failure: ParseFailure,
    transport: WorkerTransport,
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

fn transport_for_attempts(session_attempt_count: u64) -> WorkerTransport {
    if session_attempt_count > 0 {
        WorkerTransport::Session
    } else {
        WorkerTransport::NotApplicable
    }
}

fn observed_duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

struct SlotPermit<'a> {
    slots: &'a SessionSlots,
    slot: SessionSlot,
}

impl Drop for SlotPermit<'_> {
    fn drop(&mut self) {
        lock_unpoison(&self.slots.idle).push(std::mem::take(&mut self.slot));
        self.slots.changed.notify_one();
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

/// 构造一次严格的 PDF classify domain request。
pub fn build_classify_request(
    request_id: String,
    file_path: &Path,
    source_version: &str,
    max_pages: u64,
) -> ClassifyOperation {
    ClassifyOperation {
        request_id,
        payload: ClassifyRequest {
            file_path: file_path.to_string_lossy().into_owned(),
            source_version: source_version.to_string(),
            max_pages,
            policy_version: "pdf_text_presence_v1".to_string(),
        },
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::parsers::{document, office, register_worker};
    use ai_daily_scanner_contract::AdapterPaths;

    #[test]
    #[ignore = "measures the full 15-second OCR queue deadline with a real worker registration"]
    fn image_ocr_queue_obeys_the_parent_deadline() {
        let dir = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
        let command = document::worker_command(&AdapterPaths {
            pdf_worker_path: dir
                .join("ai-daily-pdf-parser.exe")
                .to_string_lossy()
                .into_owned(),
            office_worker_path: String::new(),
        });
        let timeout = Duration::from_secs(30);
        let worker = register_worker(&command, timeout).unwrap();
        let pool = WorkerPool::new(
            command,
            worker.identity.clone(),
            worker,
            SessionParams::with_default_concurrency(1),
            WorkerRssTracker::default(),
        );
        let _held_slot = pool
            .slots
            .checkout(&pool.expected, Instant::now(), timeout)
            .unwrap();
        let file = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pdf_parser/tests/fixtures/ocr-image-pages.pdf");
        let file_path = file.to_str().unwrap();
        let request = ParseOperation {
            request_id: crate::scanner::next_request_id(),
            payload: ai_daily_worker_contract::ParseRequest {
                file_path: file_path.to_owned(),
                file_type: ".pdf".into(),
                backend: ParserBackend::RustPdfiumV3,
                remaining_timeout_ms: 30_000,
                max_file_size_bytes: 10_000_000,
                parser_limits: ai_daily_worker_contract::ParserLimits::PdfImageOcr {
                    max_pages: 1,
                    excerpt_max_chars: 512,
                    selected_pages: vec![1],
                },
                expected_source_version: crate::parsers::current_source(file_path).unwrap().0,
            },
        };
        let started = Instant::now();
        let failure = pool.parse_worker(&request, timeout).unwrap_err();
        let elapsed = started.elapsed();
        assert!(failure.failure.is_timeout());
        assert_eq!(failure.attempt_count, 0);
        assert!(
            (Duration::from_secs(14)..Duration::from_secs(19)).contains(&elapsed),
            "{elapsed:?}"
        );
        println!("OCR queue timed out after {elapsed:?}");
    }

    #[test]
    fn occupied_slots_respect_deadline_and_timeout_replaces_only_one_process() {
        let directory = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
        let paths = AdapterPaths {
            office_worker_path: directory
                .join("ai-daily-office-parser.exe")
                .to_string_lossy()
                .into_owned(),
            pdf_worker_path: directory
                .join("ai-daily-pdf-parser.exe")
                .to_string_lossy()
                .into_owned(),
        };
        let timeout = Duration::from_secs(10);
        let slots = SessionSlots::new(64);
        let pool = |command| {
            let worker = register_worker(&command, timeout).unwrap();
            WorkerPool::with_slots(
                command,
                worker.identity.clone(),
                worker,
                SessionParams::default(),
                WorkerRssTracker::default(),
                slots.clone(),
            )
        };
        let office = pool(office::worker_command(&paths));
        let pdf = pool(document::worker_command(&paths));
        let pid = |session: &mut WorkerSession, _: Duration| {
            Ok(session.child.as_ref().unwrap().child.id())
        };
        office.execute_operation("fixture", timeout, pid).unwrap();
        let pdf_pid = pdf
            .execute_operation("fixture", timeout, pid)
            .unwrap()
            .value;
        let pdf_busy = slots
            .checkout(&pdf.expected, Instant::now(), timeout)
            .unwrap();
        let office_busy = slots
            .checkout(&office.expected, Instant::now(), timeout)
            .unwrap();
        let short = Duration::from_millis(30);
        let started = Instant::now();
        let waiting = office.execute_operation("fixture", short, pid).unwrap_err();
        assert!(waiting.failure.is_timeout());
        assert_eq!(waiting.attempt_count, 0);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(office_busy);

        // No request was sent. This live Office child waits for input until its
        // response deadline expires. The checked-out PDF child stays usable.
        let expired = office
            .execute_operation("fixture", short, |session, remaining| {
                session.child.as_ref().unwrap().read_line(remaining)
            })
            .unwrap_err();
        assert!(expired.failure.is_timeout());
        assert_eq!(expired.attempt_count, 1);
        office.execute_operation("fixture", timeout, pid).unwrap();
        assert_eq!(office.stats().session_restart_count, 1);
        drop(pdf_busy);
        assert_eq!(
            pdf.execute_operation("fixture", timeout, pid)
                .unwrap()
                .value,
            pdf_pid
        );
        assert_eq!(pdf.stats().session_restart_count, 0);
        assert_eq!(lock_unpoison(&slots.idle).len(), 2);
    }
}
