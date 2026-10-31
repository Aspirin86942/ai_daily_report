#![cfg(windows)]
#[path = "support/model_mock.rs"]
mod model_mock;
use serde_json::{json, Value};
use std::os::windows::process::CommandExt;
use std::{
    collections::BTreeSet,
    fs::{self, File, FileTimes},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};
#[test]
#[ignore = "requires AI_DAILY_TEST_BUNDLE; standalone personal scan acceptance"]
fn standalone_personal_scan_publishes_once_without_collection() {
    let bundle = fs::canonicalize(std::env::var_os("AI_DAILY_TEST_BUNDLE").unwrap()).unwrap();
    let output = PathBuf::from(std::env::var_os("AI_DAILY_ACCEPTANCE_OUTPUT").unwrap());
    let root = output.with_file_name(format!(
        "{}-scan",
        output.file_name().unwrap().to_string_lossy()
    ));
    assert!(!root.exists());
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir(root.join("work")).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let config = root.join("config");
    let server = Server::new();
    fs::write(config.join("settings.yaml"),json!({
        "paths":{"work_dir":root.join("work"),"data_dir":root.join("data"),"db_dir":root.join("db"),"reports_dir":root.join("reports"),"log_dir":root.join("logs")},
        "report":{"profile":"personal"},
        "archive":{"allow_work_dir_for_model":true},
        "schedule":{"execution_limit_minutes":6},
        "llm":{"base_url":format!("http://127.0.0.1:{}",server.port),"model_id":"synthetic-local",
            "max_request_utf8_bytes":160000,"context_window_tokens":128000,"verified_max_output_tokens":8192,
            "max_tokens":4096,"request_timeout_seconds":3}
    }).to_string()).unwrap();
    let target = ai_daily_report::types::target_date(
        chrono::Utc::now(),
        None,
        true,
        chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
    )
    .unwrap();
    let stamp = ai_daily_report::types::utc_window(target)
        .unwrap()
        .0
        .timestamp() as u64
        + 3600;
    let file = root.join("work/quotation.md");
    fs::write(
        &file,
        "金额由 100 元调整为 120 元；预算内且复核通过后生效，仍待审批。责任人与截止时间待确认。\n",
    )
    .unwrap();
    File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(stamp)))
        .unwrap();
    let mut loaded = BTreeSet::new();
    let mut checks = vec![];
    macro_rules! ok {
        ($args:expr) => {{let (success,value,error)=run(&bundle,&config,$args,&mut loaded);assert!(success,"{error}");checks.push(json!({"args":$args,"passed":true}));value}};
    }
    ok!(&["doctor", "--strict"]);
    assert_eq!(ok!(&["daily", "--dry-run"])["model_calls"], 0);
    assert_eq!(server.calls(), 0);
    let started = Instant::now();
    let result = ok!(&["daily", "--scheduled"]);
    let elapsed = started.elapsed().as_millis();
    assert_eq!(result["target_date"], target.to_string());
    assert_eq!(result["model_calls"], 1);
    assert_eq!(server.calls(), 1);
    assert!(!root.join("data/daily_cache/archive").exists());
    let reports = rusqlite::Connection::open(root.join("db/reports.sqlite3")).unwrap();
    let raw: String = reports
        .query_row(
            "SELECT raw_json FROM daily_reports WHERE date=?",
            [target.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    let report: ai_daily_report::reports::personal::PersonalReport =
        serde_json::from_str(&raw).unwrap();
    report.validate().unwrap();
    assert!(!report.items.is_empty());
    assert!(report.evidence_attachment.is_some());
    assert_eq!(
        report.sources[0].coverage_limits["personal"]["input_mode"],
        "scan"
    );
    fs::rename(root.join("work"), root.join("work-offline")).unwrap();
    assert_eq!(ok!(&["daily", "--scheduled"])["action"], "existing");
    assert_eq!(server.calls(), 1);
    let (_, health, error) = run(
        &bundle,
        &config,
        &["health", "--format", "json"],
        &mut loaded,
    );
    assert_eq!(
        health["task_names"].as_object().unwrap().len(),
        2,
        "{error}"
    );
    assert!(!health["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["code"] == "COLLECTION_GAP" || c["code"] == "PARSE_QUEUE"));
    let plan = ok!(&["task", "plan", "--kind", "daily", "--sid", "S-1-5-21-123"]);
    assert_eq!(
        plan["xml"]
            .as_str()
            .unwrap()
            .matches("<CalendarTrigger")
            .count(),
        1
    );
    fs::write(root.join("acceptance.json"),serde_json::to_vec_pretty(&json!({"status":"passed","bundle":bundle,
        "remote_model_calls":0,"local_http_requests":server.calls(),"checks":checks,"elapsed_ms":elapsed,
        "production_tasks_changed":0,"report_items":report.items.len(),"health":health})).unwrap()).unwrap();
}

fn modules(pid: u32) -> Vec<String> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::{
            ProcessStatus::{EnumProcessModulesEx, GetModuleFileNameExW, LIST_MODULES_ALL},
            Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
        },
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if process.is_null() {
            return vec![];
        }
        let mut handles = [std::ptr::null_mut(); 1024];
        let mut bytes = 0;
        let mut found = Vec::new();
        if EnumProcessModulesEx(
            process,
            handles.as_mut_ptr(),
            std::mem::size_of_val(&handles) as u32,
            &mut bytes,
            LIST_MODULES_ALL,
        ) != 0
        {
            for handle in handles
                .iter()
                .take((bytes as usize / std::mem::size_of_val(&handles[0])).min(handles.len()))
            {
                let mut name = [0u16; 32768];
                let length =
                    GetModuleFileNameExW(process, *handle, name.as_mut_ptr(), name.len() as u32);
                if length > 0 {
                    found.push(String::from_utf16_lossy(&name[..length as usize]));
                }
            }
        }
        CloseHandle(process);
        found
    }
}
fn request(stream: &mut TcpStream) -> Value {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = stream.read(&mut buffer).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&buffer[..n]);
        assert!(bytes.len() < 1024 * 1024);
        if let Some(offset) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if bytes.len() >= offset + 4 + length {
                return serde_json::from_slice(&bytes[offset + 4..offset + 4 + length]).unwrap();
            }
        }
    }
}
struct Server {
    port: u16,
    count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, s) = (count.clone(), stop.clone());
        let handle = thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let input = request(&mut stream);
                c.fetch_add(1, Ordering::SeqCst);
                let (status,body)=(200,json!({"choices":[{"finish_reason":"stop","message":{"content":model_mock::reply(&input).to_string()}}],"usage":{"prompt_tokens":100,"completion_tokens":80,"total_tokens":180}}).to_string().into_bytes());
                thread::sleep(Duration::from_millis(60));
                write!(stream,"HTTP/1.1 {status} test\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            port,
            count,
            stop,
            thread: Some(handle),
        }
    }
    fn calls(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}
fn run(
    bundle: &Path,
    config: &Path,
    args: &[&str],
    loaded: &mut BTreeSet<String>,
) -> (bool, Value, String) {
    let system = std::env::var_os("SystemRoot").unwrap();
    let temp = std::env::temp_dir();
    let mut command = Command::new(bundle.join("ai-daily-report.exe"));
    command
        .env_clear()
        .env("SystemRoot", &system)
        .env("WINDIR", &system)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env("PATH", Path::new(&system).join("System32"))
        .env("DEEPSEEK_API_KEY", "synthetic-acceptance-key");
    command
        .arg("--config-dir")
        .arg(config)
        .args(args)
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let output_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(4 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    });
    let error_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.take(1024 * 1024).read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(45);
    while child.try_wait().unwrap().is_none() {
        loaded.extend(modules(child.id()));
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("bundle command exceeded deadline: {args:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().unwrap();
    let text = String::from_utf8(output_thread.join().unwrap()).unwrap();
    (
        status.success(),
        serde_json::from_str(&text).unwrap_or(Value::Null),
        String::from_utf8_lossy(&error_thread.join().unwrap()).into(),
    )
}
#[test]
#[ignore = "requires AI_DAILY_TEST_BUNDLE; standalone package acceptance"]
fn standalone_bundle_runs_reports_recovery_and_scheduling_without_python() {
    let bundle =
        fs::canonicalize(std::env::var_os("AI_DAILY_TEST_BUNDLE").expect("bundle required"))
            .unwrap();
    let output = PathBuf::from(
        std::env::var_os("AI_DAILY_ACCEPTANCE_OUTPUT").expect("fresh output directory required"),
    );
    assert!(!output.exists());
    fs::create_dir_all(output.join("config")).unwrap();
    let root = fs::canonicalize(output).unwrap();
    fs::create_dir(root.join("work")).unwrap();
    let source = root.join("work/approval.md");
    fs::write(
        &source,
        "金额由100元调整为120元；预算内且复核通过后生效，仍待审批。责任人与截止时间待确认。\n",
    )
    .unwrap();
    let target = ai_daily_report::types::target_date(
        chrono::Utc::now(),
        None,
        true,
        chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
    )
    .unwrap();
    let stamp = ai_daily_report::types::utc_window(target)
        .unwrap()
        .0
        .timestamp() as u64
        + 3600;
    File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(stamp)))
        .unwrap();
    let server = Server::new();
    let config = root.join("config");
    fs::write(config.join("settings.yaml"),json!({"paths":{"work_dir":root.join("work"),"data_dir":root.join("data"),"db_dir":root.join("db"),"reports_dir":root.join("reports"),"log_dir":root.join("logs")},"llm":{"base_url":format!("http://127.0.0.1:{}",server.port),"model_id":"synthetic-local","max_request_utf8_bytes":160000,"context_window_tokens":128000,"verified_max_output_tokens":8192,"max_tokens":4096,"request_timeout_seconds":3}}).to_string()).unwrap();
    let mut loaded = BTreeSet::new();
    let mut checks = vec![];
    macro_rules! ok {($args:expr)=>{{let args=$args;let(success,value,error)=run(&bundle,&config,args,&mut loaded);assert!(success,"{args:?}: {error}");checks.push(json!({"args":args,"passed":true}));value}}}
    ok!(&["doctor", "--strict"]);
    assert_eq!(server.calls(), 0);
    assert_eq!(ok!(&["daily", "--dry-run"])["model_calls"], 0);
    let first = ok!(&["daily", "--scheduled"]);
    assert_eq!(first["model_calls"], 1);
    assert_eq!(ok!(&["daily", "--scheduled"])["action"], "existing");
    let path = PathBuf::from(first["output_path"].as_str().unwrap());
    let original = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    ok!(&[
        "repair-output",
        "--run-id",
        first["run_id"].as_str().unwrap()
    ]);
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(server.calls(), 1);
    ok!(&["daily", "--target-date", &target.to_string(), "--no-save"]);
    ok!(&[
        "daily",
        "--target-date",
        &target.to_string(),
        "--regenerate",
        "--reason",
        "isolated acceptance"
    ]);
    assert_eq!(server.calls(), 3);
    ok!(&["doctor", "--check-model"]);
    assert_eq!(server.calls(), 4);
    for command in ["weekly", "monthly", "backfill", "coverage", "ocr"] {
        let (success, _, error) = run(&bundle, &config, &[command], &mut loaded);
        assert!(
            !success && error.contains("FEATURE_RETIRED"),
            "{command}: {error}"
        );
        checks.push(json!({"retired":command,"passed":true}));
    }
    ok!(&["list"]);
    ok!(&["runs", "--limit", "50"]);
    ok!(&["search", "--query", "审批", "--format", "json"]);
    let backup = root.join("reports-backup.sqlite3");
    ok!(&["backup", "--output", backup.to_str().unwrap()]);
    let conn = rusqlite::Connection::open(backup).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert!(ok!(&["task", "plan", "--sid", "S-1-5-21-100"])
        .to_string()
        .contains("ai-daily-report.exe"));
    ok!(&["cleanup"]);
    assert_eq!(server.calls(), 4);
    assert!(!loaded.iter().any(|p| {
        let name = Path::new(p)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();
        name.contains("python") || name.ends_with(".pyd") || name == "vcruntime140.dll"
    }));
    fs::write(root.join("acceptance.json"),serde_json::to_vec_pretty(&json!({"status":"passed","bundle":bundle,"path_scope":"Windows System32 only","python_environment_inherited":false,"remote_model_calls":0,"local_http_requests":server.calls(),"checks":checks,"loaded_modules":loaded,"production_tasks_changed":0,"real_daily_windows":0})).unwrap()).unwrap();
}
