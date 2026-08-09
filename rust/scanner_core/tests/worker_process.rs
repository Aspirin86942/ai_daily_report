use ai_daily_scanner_core::process::{run_process, ProcessError, ProcessSpec, WorkerRssTracker};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use std::time::Instant;

#[test]
fn missing_worker_is_an_explicit_start_failure() {
    let missing = std::env::temp_dir().join(format!(
        "missing-worker-{}-{}.exe",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let spec = ProcessSpec::new(missing, Duration::from_secs(1));

    let error = run_process(&spec).expect_err("missing worker must not start");

    assert_eq!(error, ProcessError::StartFailed);
}

#[test]
fn embedded_nul_in_process_arguments_is_rejected_before_start() {
    let program = std::env::current_exe().expect("test executable should exist");
    let mut spec = ProcessSpec::new(program, Duration::from_secs(1));
    spec.args = vec![OsString::from("invalid\0argument")];

    let error = run_process(&spec).expect_err("NUL must never reach CreateProcess or exec");

    assert_eq!(error, ProcessError::StartFailed);
}

#[test]
fn worker_stdout_stderr_and_exit_code_are_captured() {
    let Some(python) = python_executable() else {
        return;
    };
    let mut spec = ProcessSpec::new(python, Duration::from_secs(5));
    spec.args = vec![
        OsString::from("-c"),
        OsString::from(
            "import sys; data=sys.stdin.buffer.read(); sys.stdout.buffer.write(data); sys.stderr.write('audit'); raise SystemExit(7)",
        ),
    ];
    spec.stdin = "中文 payload".as_bytes().to_vec();

    let output = run_process(&spec).expect("worker should complete");

    assert_eq!(output.exit_code, 7);
    assert_eq!(output.stdout, "中文 payload".as_bytes());
    assert_eq!(output.stderr, b"audit");
}

#[cfg(windows)]
#[test]
fn windows_job_peak_is_recorded_in_the_shared_run_tracker() {
    let Some(python) = python_executable() else {
        return;
    };
    let tracker = WorkerRssTracker::default();
    let mut spec = ProcessSpec::new(python, Duration::from_secs(5));
    spec.args = vec![
        OsString::from("-c"),
        OsString::from("payload=bytearray(4*1024*1024); print(len(payload))"),
    ];
    spec.rss_tracker = Some(tracker.clone());

    let output = run_process(&spec).expect("tracked worker should complete");

    assert_eq!(output.exit_code, 0);
    assert!(
        tracker.peak_worker_rss_bytes().is_some_and(|peak| peak > 0),
        "a started contained child must publish a non-zero Job peak"
    );
}

#[test]
fn empty_worker_input_is_an_immediate_eof() {
    let Some(python) = python_executable() else {
        return;
    };
    let mut spec = ProcessSpec::new(python, Duration::from_secs(5));
    spec.args = vec![
        OsString::from("-c"),
        OsString::from("import sys; print(len(sys.stdin.buffer.read()))"),
    ];

    let output = run_process(&spec).expect("empty input should complete");

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, b"0\r\n");
}

#[test]
fn windows_command_line_quoting_preserves_exact_arguments() {
    let Some(python) = python_executable() else {
        return;
    };
    let expected = ["value with spaces", "quoted\"value", "trailing\\"];
    let mut spec = ProcessSpec::new(python, Duration::from_secs(5));
    spec.args = vec![
        OsString::from("-c"),
        OsString::from("import json,sys; print(json.dumps(sys.argv[1:]))"),
        OsString::from(expected[0]),
        OsString::from(expected[1]),
        OsString::from(expected[2]),
    ];

    let output = run_process(&spec).expect("quoted arguments should reach the worker");
    let actual: Vec<String> =
        serde_json::from_slice(&output.stdout).expect("worker output should be JSON");

    assert_eq!(actual, expected);
}

#[test]
fn worker_sleep_past_deadline_is_terminated() {
    let Some(python) = python_executable() else {
        return;
    };
    let mut spec = ProcessSpec::new(python, Duration::from_millis(100));
    spec.args = vec![
        OsString::from("-c"),
        OsString::from("import time; time.sleep(30)"),
    ];

    let error = run_process(&spec).expect_err("sleeping worker must time out");

    assert_eq!(error, ProcessError::TimedOut);
}

#[test]
fn output_past_capture_limit_is_rejected_without_pipe_deadlock() {
    let Some(python) = python_executable() else {
        return;
    };
    let mut spec = ProcessSpec::new(python, Duration::from_secs(5));
    spec.args = vec![
        OsString::from("-c"),
        OsString::from("import sys; sys.stdout.buffer.write(b'x' * 1048576)"),
    ];
    spec.capture_limit = 1024;

    let error = run_process(&spec).expect_err("oversized output must be rejected");

    assert_eq!(error, ProcessError::OutputTooLarge);
}

#[cfg(windows)]
#[test]
fn concurrent_workers_do_not_inherit_each_others_pipe_handles() {
    let Some(python) = python_executable() else {
        return;
    };
    const FAST_WORKERS: usize = 12;
    let barrier = Arc::new(Barrier::new(FAST_WORKERS + 2));
    let started = Instant::now();

    let slow_barrier = Arc::clone(&barrier);
    let slow_python = python.clone();
    let slow = thread::spawn(move || {
        slow_barrier.wait();
        let mut spec = ProcessSpec::new(slow_python, Duration::from_secs(5));
        spec.args = vec![
            OsString::from("-c"),
            OsString::from("import time; time.sleep(1.5); print('slow')"),
        ];
        run_process(&spec).expect("slow worker should complete")
    });

    let fast = (0..FAST_WORKERS)
        .map(|_| {
            let worker_barrier = Arc::clone(&barrier);
            let worker_python = python.clone();
            thread::spawn(move || {
                worker_barrier.wait();
                let mut spec = ProcessSpec::new(worker_python, Duration::from_secs(5));
                spec.args = vec![OsString::from("-c"), OsString::from("print('fast')")];
                let output = run_process(&spec).expect("fast worker should complete");
                (started.elapsed(), output)
            })
        })
        .collect::<Vec<_>>();

    barrier.wait();
    for worker in fast {
        let (completed_at, output) = worker.join().expect("fast worker thread should join");
        assert_eq!(output.stdout, b"fast\r\n");
        assert!(
            completed_at < Duration::from_millis(1_200),
            "fast worker waited for an unrelated slow worker: {completed_at:?}"
        );
    }
    assert_eq!(
        slow.join().expect("slow worker thread should join").stdout,
        b"slow\r\n"
    );
}

#[cfg(windows)]
#[test]
fn timeout_terminates_worker_grandchild() {
    let Some(python) = python_executable() else {
        return;
    };
    let directory = tempfile::tempdir().expect("temporary process root should exist");
    let pid_path = directory.path().join("grandchild.pid");
    let script = directory.path().join("spawn_grandchild.py");
    fs::write(
        &script,
        "import pathlib, subprocess, sys, time\nchild = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\npathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='ascii')\ntime.sleep(30)\n",
    )
    .expect("worker script should be writable");
    let mut spec = ProcessSpec::new(python, Duration::from_secs(1));
    spec.args = vec![
        OsString::from(script.as_os_str()),
        OsString::from(pid_path.as_os_str()),
    ];

    let error = run_process(&spec).expect_err("worker tree must hit its deadline");

    assert_eq!(error, ProcessError::TimedOut);
    let pid: u32 = fs::read_to_string(&pid_path)
        .expect("worker should record its grandchild")
        .parse()
        .expect("grandchild PID should be numeric");
    assert_process_exited(pid);
}

#[cfg(windows)]
#[test]
fn nested_job_runner_can_contain_its_own_worker() {
    let mut spec = ProcessSpec::new(
        std::env::current_exe().expect("test executable should exist"),
        Duration::from_secs(5),
    );
    spec.args = vec![
        OsString::from("--exact"),
        OsString::from("nested_job_helper"),
        OsString::from("--nocapture"),
    ];

    let output = run_process(&spec).expect("nested runner should remain containable");

    assert_eq!(
        output.exit_code,
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(windows)]
#[test]
fn nested_job_helper() {
    let invoked_as_helper = std::env::args()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair == ["--exact", "nested_job_helper"]);
    if !invoked_as_helper {
        return;
    }
    let Some(python) = python_executable() else {
        return;
    };
    let mut spec = ProcessSpec::new(python, Duration::from_secs(2));
    spec.args = vec![OsString::from("-c"), OsString::from("print('nested')")];
    let output = run_process(&spec).expect("inner worker must join a nested Job");
    assert_eq!(output.exit_code, 0);
}

#[cfg(windows)]
#[test]
fn python_outer_watchdog_closes_job_and_kills_grandchild() {
    let Some(python) = python_executable() else {
        return;
    };
    let directory = tempfile::tempdir().expect("temporary watchdog root should exist");
    let pid_path = directory.path().join("watchdog-grandchild.pid");
    let worker_script = directory.path().join("watchdog_worker.py");
    let watchdog_script = directory.path().join("outer_watchdog.py");
    fs::write(
        &worker_script,
        "import pathlib, subprocess, sys, time\nchild = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\npathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='ascii')\ntime.sleep(30)\n",
    )
    .expect("watchdog worker should be writable");
    fs::write(
        &watchdog_script,
        "import os, pathlib, subprocess, sys, time\nhelper, python, worker, pid_path = sys.argv[1:]\nenv = os.environ.copy()\nenv['AI_DAILY_WATCHDOG_HELPER'] = '1'\nenv['AI_DAILY_WATCHDOG_PYTHON'] = python\nenv['AI_DAILY_WATCHDOG_WORKER'] = worker\nenv['AI_DAILY_WATCHDOG_PID'] = pid_path\nproc = subprocess.Popen([helper, '--exact', 'outer_watchdog_helper', '--nocapture'], env=env)\npid_file = pathlib.Path(pid_path)\ndef pid_recorded():\n    try:\n        return pid_file.read_text(encoding='ascii').strip().isdigit()\n    except OSError:\n        return False\ndeadline = time.monotonic() + 10\nwhile not pid_recorded() and time.monotonic() < deadline:\n    if proc.poll() is not None:\n        raise SystemExit('scanner helper exited before spawning worker')\n    time.sleep(0.02)\nif not pid_recorded():\n    proc.kill(); proc.wait(); raise SystemExit('grandchild PID was not recorded')\nproc.kill()\nproc.wait(timeout=5)\n",
    )
    .expect("outer watchdog should be writable");

    let status = Command::new(&python)
        .args([
            watchdog_script.as_os_str(),
            std::env::current_exe()
                .expect("test executable should exist")
                .as_os_str(),
            python.as_os_str(),
            worker_script.as_os_str(),
            pid_path.as_os_str(),
        ])
        .status()
        .expect("Python outer watchdog should run");

    assert!(
        status.success(),
        "Python outer watchdog should kill the helper"
    );
    let pid: u32 = fs::read_to_string(&pid_path)
        .expect("grandchild PID should remain as audit evidence")
        .trim()
        .parse()
        .expect("grandchild PID should be numeric");
    assert_process_exited(pid);
}

#[cfg(windows)]
#[test]
fn outer_watchdog_helper() {
    if std::env::var_os("AI_DAILY_WATCHDOG_HELPER").is_none() {
        return;
    }
    let python = PathBuf::from(
        std::env::var_os("AI_DAILY_WATCHDOG_PYTHON")
            .expect("watchdog helper Python path should be provided"),
    );
    let worker = std::env::var_os("AI_DAILY_WATCHDOG_WORKER")
        .expect("watchdog worker path should be provided");
    let pid_path =
        std::env::var_os("AI_DAILY_WATCHDOG_PID").expect("watchdog PID path should be provided");
    let mut spec = ProcessSpec::new(python, Duration::from_secs(30));
    spec.args = vec![worker, pid_path];
    let _ = run_process(&spec);
}

#[cfg(windows)]
fn assert_process_exited(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

    const SYNCHRONIZE_PROCESS: u32 = 0x0010_0000;
    let handle = unsafe { OpenProcess(SYNCHRONIZE_PROCESS, 0, pid) };
    if handle.is_null() {
        return;
    }
    let wait = unsafe { WaitForSingleObject(handle, 3_000) };
    unsafe {
        CloseHandle(handle);
    }
    assert_eq!(wait, WAIT_OBJECT_0, "process {pid} survived Job cleanup");
}

fn python_executable() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let candidates = if cfg!(windows) {
        vec![root.join(".venv").join("Scripts").join("python.exe")]
    } else {
        vec![root.join(".venv").join("bin").join("python")]
    };
    candidates.into_iter().find(|path| path.is_file())
}
