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
    WorkerOperation, WorkerRequest, WorkerResponse, CONTRACT, CONTRACT_VERSION, PROTOCOL_VERSION,
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

mod error;
mod pool;
pub use error::*;
pub use pool::*;

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

    fn read_frame<T: serde::de::DeserializeOwned>(
        &self,
        timeout: Duration,
        limit: usize,
        invalid: &'static str,
    ) -> Result<T, SessionError> {
        let line = self.read_line(timeout)?;
        if line.len() > limit {
            return Err(SessionError::FrameTooLarge);
        }
        serde_json::from_slice(&line).map_err(|_| SessionError::ProtocolCorruption(invalid.into()))
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

/// One process serves one request at a time. The pool provides concurrency.
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
        let mut child = SessionChild::spawn(command, rss_tracker)?;
        let hello: WorkerHello = child.read_frame(
            timeout.saturating_sub(started.elapsed()),
            SESSION_HELLO_FRAME_LIMIT,
            "session hello is not one strict JSON frame",
        )?;
        hello.validate().map_err(protocol_violation(
            "session hello violates the strict contract",
        ))?;
        if &hello != expected {
            return Err(SessionError::BuildMismatch);
        }
        // Idle time starts after the handshake completes.
        child.touch_activity();
        Ok(Self {
            child: Some(child),
            params,
            identity: hello,
            requests_served: 0,
        })
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

    fn exchange(
        &mut self,
        request_id: &str,
        operation: WorkerOperation,
        payload: &impl serde::Serialize,
        capture_limit: usize,
        started: Instant,
        timeout: Duration,
    ) -> Result<serde_json::Value, SessionError> {
        let envelope = WorkerRequest {
            contract: CONTRACT.into(),
            protocol_version: SESSION_PROTOCOL_VERSION,
            request_id: request_id.into(),
            operation,
            payload: serde_json::to_value(payload).map_err(|_| SessionError::IoFailed)?,
        };
        let frame = serde_json::to_vec(&envelope).map_err(|_| SessionError::IoFailed)?;
        if frame.len() > SESSION_REQUEST_FRAME_LIMIT {
            return Err(SessionError::FrameTooLarge);
        }
        let child = self.child.as_mut().ok_or(SessionError::Eof)?;
        child.write_line(frame)?;
        let response: WorkerResponse = child.read_frame(
            timeout.saturating_sub(started.elapsed()),
            capture_limit,
            "session stdout is not one strict JSON response",
        )?;
        response.validate().map_err(protocol_violation(
            "session response violates the strict contract",
        ))?;
        if response.operation != operation || response.request_id != request_id {
            return Err(SessionError::ProtocolCorruption(
                "session response operation/contract/request_id mismatch".into(),
            ));
        }
        // A valid response finishes the request even when the parser rejected it.
        self.requests_served += 1;
        child.touch_activity();
        match response.error {
            Some(diagnostic) => Err(SessionError::Rejected(Box::new(diagnostic))),
            None => Ok(response
                .result
                .expect("validated success response has a result")),
        }
    }
}

fn protocol_violation(context: &'static str) -> impl FnOnce(String) -> SessionError {
    move |message| SessionError::ProtocolCorruption(format!("{context}: {message}"))
}

pub fn session_classify(
    session: &mut WorkerSession,
    request: &ClassifyOperation,
    timeout: Duration,
) -> Result<ClassifyResult, SessionError> {
    let started = Instant::now();
    request.validate().map_err(protocol_violation(
        "classify request violates the strict contract",
    ))?;
    let value = session.exchange(
        &request.request_id,
        WorkerOperation::PdfClassify,
        &request.payload,
        SESSION_CLASSIFY_FRAME_LIMIT,
        started,
        timeout,
    )?;
    let result: ClassifyResult = serde_json::from_value(value).map_err(|_| {
        SessionError::ProtocolCorruption(
            "classify response result is not a classifier result".into(),
        )
    })?;
    result
        .validate_for_max_pages(request.max_pages)
        .map_err(protocol_violation(
            "session classifier result violates the request page window",
        ))?;
    Ok(result)
}

pub fn session_parse(
    session: &mut WorkerSession,
    request: &ParseOperation,
    timeout: Duration,
) -> Result<ParseResult, SessionError> {
    let started = Instant::now();
    request.validate().map_err(protocol_violation(
        "parse request violates the strict contract",
    ))?;
    let operation = match request.backend {
        ParserBackend::RustPdfiumV3 => WorkerOperation::PdfParse,
        _ => WorkerOperation::OfficeParse,
    };
    let capture_limit = crate::parsers::worker_response_capture_limit(request)
        .map_err(|_| SessionError::IoFailed)?;
    let value = session.exchange(
        &request.request_id,
        operation,
        &request.payload,
        capture_limit,
        started,
        timeout,
    )?;
    let result: ParseResult = serde_json::from_value(value).map_err(|_| {
        SessionError::ProtocolCorruption("parse response result is not a parse result".into())
    })?;
    result.validate().map_err(protocol_violation(
        "session parse result violates the domain contract",
    ))?;
    Ok(result)
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
