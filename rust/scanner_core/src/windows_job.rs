//! Windows worker execution with strict process-tree containment.

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io::Write;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::ptr::{null, null_mut};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::process::{
    join_reader, join_writer, read_bounded, ProcessError, ProcessFailure, ProcessOutput,
    ProcessSpec, WorkerRssTracker,
};

const TERMINATION_WAIT: Duration = Duration::from_secs(5);
const TIMEOUT_EXIT_CODE: u32 = 124;

pub(crate) fn run(spec: &ProcessSpec) -> Result<ProcessOutput, ProcessFailure> {
    let started = Instant::now();
    let job = create_kill_on_close_job().map_err(ProcessFailure::before_start)?;
    let mut rss_observation = JobRssObservation::new(spec.rss_tracker.as_ref(), job.raw());
    let (child_stdin, parent_stdin) =
        create_pipe(false).map_err(ProcessFailure::before_start)?;
    let (parent_stdout, child_stdout) =
        create_pipe(true).map_err(ProcessFailure::before_start)?;
    let (parent_stderr, child_stderr) =
        create_pipe(true).map_err(ProcessFailure::before_start)?;

    let application = wide_null(spec.program.as_os_str());
    let mut command_line = build_command_line(spec.program.as_os_str(), &spec.args);
    let current_dir = spec
        .current_dir
        .as_ref()
        .map(|path| wide_null(path.as_os_str()));
    let current_dir_ptr = current_dir.as_ref().map_or(null(), |value| value.as_ptr());

    // CREATE_PROCESS inherits handles process-wide. Restrict the inherited set
    // explicitly so concurrent Rayon workers cannot inherit one another's pipe
    // endpoints and keep an unrelated reader waiting for EOF.
    let inherited_handles = [child_stdin.raw(), child_stdout.raw(), child_stderr.raw()];
    let attributes = ProcThreadAttributeList::with_handle_list(&inherited_handles)
        .map_err(ProcessFailure::before_start)?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = child_stdin.raw();
    startup.StartupInfo.hStdOutput = child_stdout.raw();
    startup.StartupInfo.hStdError = child_stderr.raw();
    startup.lpAttributeList = attributes.raw();
    let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };

    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
            null(),
            current_dir_ptr,
            &startup.StartupInfo,
            &mut process_info,
        )
    };
    if created == 0 {
        return Err(ProcessFailure::before_start(ProcessError::StartFailed));
    }
    rss_observation.mark_started();
    let Some(process) = OwnedHandle::new(process_info.hProcess) else {
        if let Some(thread_handle) = OwnedHandle::new(process_info.hThread) {
            drop(thread_handle);
        }
        return Err(ProcessFailure::after_start(ProcessError::StartFailed));
    };
    let Some(thread_handle) = OwnedHandle::new(process_info.hThread) else {
        if !stop_uncontained_process(process.raw()) {
            return Err(ProcessFailure::after_start(
                ProcessError::ContainmentFailed,
            ));
        }
        return Err(ProcessFailure::after_start(ProcessError::StartFailed));
    };
    drop(child_stdin);
    drop(child_stdout);
    drop(child_stderr);
    if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } == 0 {
        if !stop_uncontained_process(process.raw()) {
            return Err(ProcessFailure::after_start(
                ProcessError::ContainmentFailed,
            ));
        }
        return Err(ProcessFailure::after_start(
            ProcessError::ContainmentFailed,
        ));
    }
    rss_observation.mark_contained();
    if unsafe { ResumeThread(thread_handle.raw()) } == u32::MAX {
        if !terminate_and_wait(&job, &process) {
            return Err(ProcessFailure::after_start(
                ProcessError::ContainmentFailed,
            ));
        }
        return Err(ProcessFailure::after_start(
            ProcessError::ContainmentFailed,
        ));
    }
    drop(thread_handle);

    let mut stdin_file = unsafe { File::from_raw_handle(parent_stdin.into_raw()) };
    let mut stdout_file = unsafe { File::from_raw_handle(parent_stdout.into_raw()) };
    let mut stderr_file = unsafe { File::from_raw_handle(parent_stderr.into_raw()) };
    let input = spec.stdin.clone();
    let capture_limit = spec.capture_limit;
    let mut input_thread = if input.is_empty() {
        drop(stdin_file);
        None
    } else {
        Some(thread::spawn(move || -> Result<(), ProcessError> {
            match stdin_file.write_all(&input) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                Err(_) => Err(ProcessError::IoFailed),
            }
        }))
    };
    let stdout_thread = thread::spawn(move || read_bounded(&mut stdout_file, capture_limit));
    let stderr_thread = thread::spawn(move || read_bounded(&mut stderr_file, capture_limit));

    let wait_result = unsafe {
        WaitForSingleObject(
            process.raw(),
            duration_to_wait_ms(spec.timeout.saturating_sub(started.elapsed())),
        )
    };
    if wait_result == WAIT_TIMEOUT {
        if !terminate_and_wait(&job, &process) {
            return Err(ProcessFailure::after_start(
                ProcessError::ContainmentFailed,
            ));
        }
        let _ = join_optional_writer(&mut input_thread);
        let _ = join_reader(stdout_thread);
        let _ = join_reader(stderr_thread);
        return Err(ProcessFailure::after_start(ProcessError::TimedOut));
    }
    if wait_result != WAIT_OBJECT_0 {
        if !terminate_and_wait(&job, &process) {
            return Err(ProcessFailure::after_start(
                ProcessError::ContainmentFailed,
            ));
        }
        let _ = join_optional_writer(&mut input_thread);
        let _ = join_reader(stdout_thread);
        let _ = join_reader(stderr_thread);
        return Err(ProcessFailure::after_start(ProcessError::IoFailed));
    }

    let mut exit_code = 0_u32;
    if unsafe { GetExitCodeProcess(process.raw(), &mut exit_code) } == 0 {
        if !terminate_and_wait(&job, &process) {
            return Err(ProcessFailure::after_start(
                ProcessError::ContainmentFailed,
            ));
        }
        let _ = join_optional_writer(&mut input_thread);
        let _ = join_reader(stdout_thread);
        let _ = join_reader(stderr_thread);
        return Err(ProcessFailure::after_start(ProcessError::IoFailed));
    }

    // The worker protocol never allows detached descendants. Clear any child
    // still alive after the primary process exits before releasing pipe readers.
    let active = active_processes(job.raw()).ok_or_else(|| {
        ProcessFailure::after_start(ProcessError::ContainmentFailed)
    })?;
    if active > 0
        && (unsafe { TerminateJobObject(job.raw(), exit_code) } == 0
            || !wait_for_job_empty(job.raw(), TERMINATION_WAIT))
    {
        return Err(ProcessFailure::after_start(
            ProcessError::ContainmentFailed,
        ));
    }
    join_optional_writer(&mut input_thread).map_err(ProcessFailure::after_start)?;
    let stdout = join_reader(stdout_thread).map_err(ProcessFailure::after_start)?;
    let stderr = join_reader(stderr_thread).map_err(ProcessFailure::after_start)?;

    Ok(ProcessOutput {
        exit_code,
        stdout,
        stderr,
        duration: started.elapsed(),
    })
}

fn join_optional_writer(
    writer: &mut Option<thread::JoinHandle<Result<(), ProcessError>>>,
) -> Result<(), ProcessError> {
    match writer.take() {
        Some(handle) => join_writer(handle),
        None => Ok(()),
    }
}

fn create_kill_on_close_job() -> Result<OwnedHandle, ProcessError> {
    let handle = unsafe { CreateJobObjectW(null(), null()) };
    let job = OwnedHandle::new(handle).ok_or(ProcessError::ContainmentFailed)?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        Err(ProcessError::ContainmentFailed)
    } else {
        Ok(job)
    }
}

/// Long-lived worker containment handle (spec Part 7.3). Each session/one-shot
/// child gets its own kill-on-close Job Object, so terminating one request's
/// process tree can never kill a sibling session in the same pool.
pub(crate) struct SessionJob(OwnedHandle);

// Windows kernel Job handles may be transferred between threads. Ownership is
// unique and every SessionJob operation is serialized by its pool slot mutex;
// no shared raw-handle access is introduced here.
unsafe impl Send for SessionJob {}

impl SessionJob {
    /// Assigns an already-spawned child process to a fresh kill-on-close job.
    pub(crate) fn assign(process: std::os::windows::io::RawHandle) -> Result<Self, ProcessError> {
        let job = create_kill_on_close_job()?;
        if unsafe { AssignProcessToJobObject(job.raw(), process) } == 0 {
            return Err(ProcessError::ContainmentFailed);
        }
        Ok(Self(job))
    }

    pub(crate) fn terminate(&self, exit_code: u32) -> bool {
        (unsafe { TerminateJobObject(self.0.raw(), exit_code) }) != 0
    }

    /// Returns the peak committed memory observed for this session's complete
    /// process tree. The worker is contained in a dedicated Job Object, so the
    /// value cannot be polluted by sibling sessions.
    pub(crate) fn peak_memory_bytes(&self) -> Option<u64> {
        peak_job_memory_bytes(self.0.raw())
    }
}

struct JobRssObservation<'a> {
    tracker: Option<&'a WorkerRssTracker>,
    job: HANDLE,
    started: bool,
    contained: bool,
}

impl<'a> JobRssObservation<'a> {
    fn new(tracker: Option<&'a WorkerRssTracker>, job: HANDLE) -> Self {
        Self {
            tracker,
            job,
            started: false,
            contained: false,
        }
    }

    fn mark_started(&mut self) {
        self.started = true;
    }

    fn mark_contained(&mut self) {
        self.contained = true;
    }
}

impl Drop for JobRssObservation<'_> {
    fn drop(&mut self) {
        let Some(tracker) = self.tracker.filter(|_| self.started) else {
            return;
        };
        let peak = self
            .contained
            .then(|| peak_job_memory_bytes(self.job))
            .flatten();
        tracker.observe_started_child(peak);
    }
}

fn peak_job_memory_bytes(job: HANDLE) -> Option<u64> {
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        )
    };
    (ok != 0).then(|| u64::try_from(limits.PeakJobMemoryUsed).unwrap_or(u64::MAX))
}

/// Creates one anonymous pipe. `parent_reads` selects which endpoint stays in
/// the parent; that endpoint is explicitly made non-inheritable.
fn create_pipe(parent_reads: bool) -> Result<(OwnedHandle, OwnedHandle), ProcessError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let mut read = null_mut();
    let mut write = null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(ProcessError::IoFailed);
    }
    let read = OwnedHandle::new(read).ok_or(ProcessError::IoFailed)?;
    let write = OwnedHandle::new(write).ok_or(ProcessError::IoFailed)?;
    let parent = if parent_reads {
        read.raw()
    } else {
        write.raw()
    };
    if unsafe { SetHandleInformation(parent, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(ProcessError::IoFailed);
    }
    Ok((read, write))
}

fn terminate_and_wait(job: &OwnedHandle, process: &OwnedHandle) -> bool {
    (unsafe { TerminateJobObject(job.raw(), TIMEOUT_EXIT_CODE) }) != 0
        && wait_for_process(process.raw(), TERMINATION_WAIT)
        && wait_for_job_empty(job.raw(), TERMINATION_WAIT)
}

fn stop_uncontained_process(process: HANDLE) -> bool {
    // The process is still suspended here. A failed terminate can also mean it
    // already exited, so the wait result is the authoritative cleanup proof.
    let _ = unsafe { TerminateProcess(process, TIMEOUT_EXIT_CODE) };
    wait_for_process(process, TERMINATION_WAIT)
}

fn wait_for_process(process: HANDLE, timeout: Duration) -> bool {
    (unsafe { WaitForSingleObject(process, duration_to_wait_ms(timeout)) }) == WAIT_OBJECT_0
}

fn wait_for_job_empty(job: HANDLE, timeout: Duration) -> bool {
    // Job objects become signaled when their active-process count reaches
    // zero. Waiting on the kernel object avoids a fixed polling sleep on every
    // short-lived worker while the accounting record catches up.
    (unsafe { WaitForSingleObject(job, duration_to_wait_ms(timeout)) }) == WAIT_OBJECT_0
        && active_processes(job) == Some(0)
}

fn active_processes(job: HANDLE) -> Option<u32> {
    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast::<c_void>(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    };
    (ok != 0).then_some(accounting.ActiveProcesses)
}

fn duration_to_wait_ms(duration: Duration) -> u32 {
    let millis = duration.as_millis().max(1).min((u32::MAX - 1) as u128);
    millis as u32
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn build_command_line(program: &OsStr, args: &[std::ffi::OsString]) -> Vec<u16> {
    let mut command = Vec::new();
    append_quoted_argument(&mut command, program);
    for argument in args {
        command.push(b' ' as u16);
        append_quoted_argument(&mut command, argument.as_os_str());
    }
    command.push(0);
    command
}

// Mirrors the CommandLineToArgvW quoting rules used by Rust's Command.
fn append_quoted_argument(target: &mut Vec<u16>, argument: &OsStr) {
    let value: Vec<u16> = argument.encode_wide().collect();
    let needs_quotes = value.is_empty()
        || value
            .iter()
            .any(|character| *character == b' ' as u16 || *character == b'\t' as u16);
    if !needs_quotes && !value.contains(&(b'"' as u16)) {
        target.extend_from_slice(&value);
        return;
    }
    target.push(b'"' as u16);
    let mut backslashes = 0_usize;
    for character in value {
        if character == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if character == b'"' as u16 {
            target.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
        } else {
            target.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
        }
        backslashes = 0;
        target.push(character);
    }
    target.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    target.push(b'"' as u16);
}

struct OwnedHandle(HANDLE);

struct ProcThreadAttributeList {
    // usize keeps the native allocation aligned for the opaque Windows list.
    _storage: Vec<usize>,
    raw: *mut c_void,
}

impl ProcThreadAttributeList {
    fn with_handle_list(handles: &[HANDLE]) -> Result<Self, ProcessError> {
        let mut byte_count = 0_usize;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut byte_count);
        }
        if byte_count == 0 {
            return Err(ProcessError::StartFailed);
        }
        let word_count = byte_count.div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; word_count];
        let raw = storage.as_mut_ptr().cast::<c_void>();
        if unsafe { InitializeProcThreadAttributeList(raw, 1, 0, &mut byte_count) } == 0 {
            return Err(ProcessError::StartFailed);
        }
        if unsafe {
            UpdateProcThreadAttribute(
                raw,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast::<c_void>(),
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        } == 0
        {
            unsafe {
                DeleteProcThreadAttributeList(raw);
            }
            return Err(ProcessError::StartFailed);
        }
        Ok(Self {
            _storage: storage,
            raw,
        })
    }

    fn raw(&self) -> *mut c_void {
        self.raw
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.raw);
        }
    }
}

impl OwnedHandle {
    fn new(handle: HANDLE) -> Option<Self> {
        (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_raw(self) -> *mut c_void {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::process::{Command, Stdio};

    #[test]
    fn assignment_failure_cleanup_proves_uncontained_process_exit() {
        let python = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join(".venv")
            .join("Scripts")
            .join("python.exe");
        if !python.is_file() {
            return;
        }
        let mut child = Command::new(python)
            .args(["-c", "import time; time.sleep(30)"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("uncontained fixture process should start");
        let cleaned = stop_uncontained_process(child.as_raw_handle().cast());
        if !cleaned {
            let _ = child.kill();
        }
        let _ = child.wait();

        assert!(
            cleaned,
            "cleanup must prove the process reached signaled state"
        );
    }
}
