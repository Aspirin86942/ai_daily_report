//! Bounded worker process execution.
//!
//! Windows uses a Job Object so a timeout owns the complete descendant tree.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(windows))]
use std::time::Instant;

use thiserror::Error;

/// Run-scoped peak-memory accumulator shared by every worker process route.
///
/// A failed observation dominates successful samples because the run-level
/// maximum is then unknowable. If no child was started, the frozen contract
/// reports `Some(0)`.
#[derive(Clone, Default)]
pub struct WorkerRssTracker {
    state: Arc<WorkerRssState>,
}

#[derive(Default)]
struct WorkerRssState {
    peak_bytes: AtomicU64,
    observation_failed: AtomicBool,
}

impl std::fmt::Debug for WorkerRssTracker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerRssTracker")
            .field("peak_worker_rss_bytes", &self.peak_worker_rss_bytes())
            .finish()
    }
}

impl PartialEq for WorkerRssTracker {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl Eq for WorkerRssTracker {}

impl WorkerRssTracker {
    pub(crate) fn observe_started_child(&self, peak_bytes: Option<u64>) {
        match peak_bytes {
            Some(value) => {
                self.state.peak_bytes.fetch_max(value, Ordering::Relaxed);
            }
            None => self
                .state
                .observation_failed
                .store(true, Ordering::Relaxed),
        }
    }

    pub fn peak_worker_rss_bytes(&self) -> Option<u64> {
        if self.state.observation_failed.load(Ordering::Relaxed) {
            None
        } else {
            Some(self.state.peak_bytes.load(Ordering::Relaxed))
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    pub capture_limit: usize,
    pub rss_tracker: Option<WorkerRssTracker>,
}

impl ProcessSpec {
    pub fn new(program: PathBuf, timeout: Duration) -> Self {
        Self {
            program,
            args: Vec::new(),
            current_dir: None,
            stdin: Vec::new(),
            timeout,
            capture_limit: 4 * 1024 * 1024,
            rss_tracker: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub exit_code: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProcessError {
    #[error("worker process could not be started")]
    StartFailed,
    #[error("worker process could not be contained")]
    ContainmentFailed,
    #[error("worker process I/O failed")]
    IoFailed,
    #[error("worker process timed out")]
    TimedOut,
    #[error("worker process output exceeded the capture limit")]
    OutputTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessFailure {
    pub error: ProcessError,
    pub child_started: bool,
}

impl ProcessFailure {
    pub(crate) const fn before_start(error: ProcessError) -> Self {
        Self {
            error,
            child_started: false,
        }
    }

    pub(crate) const fn after_start(error: ProcessError) -> Self {
        Self {
            error,
            child_started: true,
        }
    }
}

pub fn run_process(spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
    run_process_observed(spec).map_err(|failure| failure.error)
}

pub(crate) fn run_process_observed(
    spec: &ProcessSpec,
) -> Result<ProcessOutput, ProcessFailure> {
    if !spec.program.is_absolute()
        || os_has_nul(spec.program.as_os_str())
        || spec
            .current_dir
            .as_ref()
            .is_some_and(|path| !path.is_absolute() || os_has_nul(path.as_os_str()))
        || spec.args.iter().any(|argument| os_has_nul(argument))
        || spec.timeout.is_zero()
        || spec.capture_limit == 0
    {
        return Err(ProcessFailure::before_start(ProcessError::StartFailed));
    }

    #[cfg(windows)]
    {
        crate::windows_job::run(spec)
    }

    #[cfg(not(windows))]
    {
        run_portable(spec)
    }
}

fn os_has_nul(value: &OsStr) -> bool {
    value.as_encoded_bytes().contains(&0)
}

#[cfg(not(windows))]
fn run_portable(spec: &ProcessSpec) -> Result<ProcessOutput, ProcessFailure> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::thread;

    let started = Instant::now();
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }
    let mut child = command
        .spawn()
        .map_err(|_| ProcessFailure::before_start(ProcessError::StartFailed))?;
    if let Some(tracker) = &spec.rss_tracker {
        // Portable execution has no Windows Job Object accounting. Mark the
        // started child as unobservable instead of fabricating a zero peak.
        tracker.observe_started_child(None);
    }
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProcessFailure::after_start(ProcessError::IoFailed))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProcessFailure::after_start(ProcessError::IoFailed))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProcessFailure::after_start(ProcessError::IoFailed))?;
    let input = spec.stdin.clone();
    let limit = spec.capture_limit;
    let input_thread = thread::spawn(move || -> Result<(), ProcessError> {
        match stdin.write_all(&input) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(_) => Err(ProcessError::IoFailed),
        }
    });
    let stdout_thread = thread::spawn(move || read_bounded(&mut stdout, limit));
    let stderr_thread = thread::spawn(move || read_bounded(&mut stderr, limit));

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|_| ProcessFailure::after_start(ProcessError::IoFailed))?
        {
            break status;
        }
        if started.elapsed() >= spec.timeout {
            let _ = child.kill();
            let _ = child.wait();
            join_writer(input_thread).map_err(ProcessFailure::after_start)?;
            let _ = join_reader(stdout_thread);
            let _ = join_reader(stderr_thread);
            return Err(ProcessFailure::after_start(ProcessError::TimedOut));
        }
        thread::sleep(Duration::from_millis(5));
    };

    join_writer(input_thread).map_err(ProcessFailure::after_start)?;
    let stdout = join_reader(stdout_thread).map_err(ProcessFailure::after_start)?;
    let stderr = join_reader(stderr_thread).map_err(ProcessFailure::after_start)?;
    let exit_code = status.code().map(|code| code as u32).unwrap_or(u32::MAX);
    Ok(ProcessOutput {
        exit_code,
        stdout,
        stderr,
        duration: started.elapsed(),
    })
}

pub(crate) fn read_bounded(
    reader: &mut impl std::io::Read,
    limit: usize,
) -> Result<Vec<u8>, ProcessError> {
    let mut captured = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    let mut overflowed = false;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|_| ProcessError::IoFailed)?;
        if count == 0 {
            break;
        }
        let available = limit.saturating_sub(captured.len());
        let accepted = available.min(count);
        captured.extend_from_slice(&buffer[..accepted]);
        overflowed |= accepted < count;
    }
    if overflowed {
        Err(ProcessError::OutputTooLarge)
    } else {
        Ok(captured)
    }
}

pub(crate) fn join_reader(
    handle: std::thread::JoinHandle<Result<Vec<u8>, ProcessError>>,
) -> Result<Vec<u8>, ProcessError> {
    handle.join().map_err(|_| ProcessError::IoFailed)?
}

pub(crate) fn join_writer(
    handle: std::thread::JoinHandle<Result<(), ProcessError>>,
) -> Result<(), ProcessError> {
    handle.join().map_err(|_| ProcessError::IoFailed)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_rss_tracker_keeps_the_largest_child_peak() {
        let tracker = WorkerRssTracker::default();
        tracker.observe_started_child(Some(12));
        let clone = tracker.clone();
        clone.observe_started_child(Some(7));
        clone.observe_started_child(Some(42));

        assert_eq!(tracker.peak_worker_rss_bytes(), Some(42));
    }

    #[test]
    fn failed_rss_observation_dominates_other_child_samples() {
        let tracker = WorkerRssTracker::default();
        assert_eq!(tracker.peak_worker_rss_bytes(), Some(0));
        tracker.observe_started_child(Some(42));
        tracker.observe_started_child(None);

        assert_eq!(tracker.peak_worker_rss_bytes(), None);
    }
}
