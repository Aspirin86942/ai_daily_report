use ai_daily_scanner_core::process::{run_process, ProcessError, ProcessSpec, WorkerRssTracker};
use std::{
    ffi::OsString,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    },
    thread,
    time::{Duration, Instant},
};
fn helper() -> PathBuf {
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join(if cfg!(windows) {
            "process_fixture.exe"
        } else {
            "process_fixture"
        });
    assert!(
        path.is_file(),
        "Build the Rust process_fixture example before focused process tests"
    );
    path
}
fn spec(mode: &str) -> ProcessSpec {
    let mut value = ProcessSpec::new(helper(), Duration::from_secs(5));
    value.args = vec![mode.into()];
    value
}
#[test]
fn missing_worker_is_an_explicit_start_failure() {
    let value = ProcessSpec::new(
        helper().with_file_name("nonexistent-fixture.exe"),
        Duration::from_secs(1),
    );
    assert_eq!(run_process(&value).unwrap_err(), ProcessError::StartFailed);
}
#[test]
fn embedded_nul_in_process_arguments_is_rejected_before_start() {
    let mut value = spec("fast");
    value.args.push(OsString::from("invalid\0argument"));
    assert_eq!(run_process(&value).unwrap_err(), ProcessError::StartFailed);
}
#[test]
fn worker_stdout_stderr_and_exit_code_are_captured() {
    let mut value = spec("echo");
    value.stdin = "中文 payload".as_bytes().to_vec();
    let output = run_process(&value).unwrap();
    assert_eq!(output.exit_code, 7);
    assert_eq!(output.stdout, value.stdin);
    assert_eq!(output.stderr, b"audit");
}
#[test]
fn empty_worker_input_is_an_immediate_eof() {
    assert_eq!(run_process(&spec("eof")).unwrap().stdout, b"0\n");
}
#[test]
fn windows_command_line_quoting_preserves_exact_arguments() {
    let expected = ["value with spaces", "quoted\"value", "trailing\\"];
    let mut value = spec("args");
    value.args.extend(expected.iter().map(OsString::from));
    let output = run_process(&value).unwrap();
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&output.stdout).unwrap(),
        expected
    );
}
#[test]
fn worker_sleep_past_deadline_is_terminated() {
    let mut value = spec("sleep");
    value.timeout = Duration::from_millis(100);
    assert_eq!(run_process(&value).unwrap_err(), ProcessError::TimedOut);
}
#[test]
fn output_past_capture_limit_is_rejected_without_pipe_deadlock() {
    let mut value = spec("flood");
    value.capture_limit = 1024;
    assert_eq!(
        run_process(&value).unwrap_err(),
        ProcessError::OutputTooLarge
    );
}
#[test]
fn cancellation_terminates_an_inflight_worker() {
    let mut value = spec("sleep");
    let flag = Arc::new(AtomicBool::new(false));
    value.cancel = Some(flag.clone());
    let thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        flag.store(true, Ordering::SeqCst);
    });
    assert_eq!(run_process(&value).unwrap_err(), ProcessError::Cancelled);
    thread.join().unwrap();
}
#[cfg(windows)]
#[test]
fn windows_job_peak_is_recorded_and_memory_limit_is_enforced() {
    let tracker = WorkerRssTracker::default();
    let mut value = spec("memory");
    value.args.push((4 * 1024 * 1024).to_string().into());
    value.rss_tracker = Some(tracker.clone());
    assert_eq!(run_process(&value).unwrap().exit_code, 0);
    assert!(tracker.peak_worker_rss_bytes().unwrap() > 0);
    let mut value = spec("memory");
    value.args.push((128 * 1024 * 1024).to_string().into());
    value.memory_limit_bytes = Some(32 * 1024 * 1024);
    let output = run_process(&value).unwrap();
    assert_ne!(output.exit_code, 0);
}
#[cfg(windows)]
#[test]
fn concurrent_workers_do_not_inherit_each_others_pipe_handles() {
    let barrier = Arc::new(Barrier::new(14));
    let slow_barrier = barrier.clone();
    let started = Instant::now();
    let slow = thread::spawn(move || {
        slow_barrier.wait();
        run_process(&spec("slow")).unwrap()
    });
    let fast: Vec<_> = (0..12)
        .map(|_| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                let output = run_process(&spec("fast")).unwrap();
                (started.elapsed(), output)
            })
        })
        .collect();
    barrier.wait();
    for handle in fast {
        let (time, output) = handle.join().unwrap();
        assert_eq!(output.stdout, b"fast\n");
        assert!(
            time < Duration::from_millis(1200),
            "unrelated pipe delayed fast child: {time:?}"
        );
    }
    assert_eq!(slow.join().unwrap().stdout, b"slow\n");
}
#[cfg(windows)]
fn assert_exited(pid: u32) {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !handle.is_null() {
            let result = WaitForSingleObject(handle, 3000);
            CloseHandle(handle);
            assert_eq!(result, WAIT_OBJECT_0, "child process survived containment");
        }
    }
}
#[cfg(windows)]
#[test]
fn timeout_terminates_worker_grandchild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.pid");
    let mut value = spec("spawn");
    value.args.push(path.clone().into_os_string());
    value.timeout = Duration::from_secs(1);
    assert_eq!(run_process(&value).unwrap_err(), ProcessError::TimedOut);
    assert_exited(fs::read_to_string(path).unwrap().parse().unwrap());
}
#[cfg(windows)]
#[test]
fn nested_job_runner_can_contain_its_own_worker() {
    assert_eq!(run_process(&spec("nested")).unwrap().exit_code, 0);
}
#[cfg(windows)]
#[test]
fn killed_owner_closes_job_and_terminates_grandchild() {
    use std::os::windows::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.pid");
    let mut child = std::process::Command::new(helper())
        .args(["contain", path.to_str().unwrap()])
        .creation_flags(0x08000000)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_exited(fs::read_to_string(path).unwrap().parse().unwrap());
}
