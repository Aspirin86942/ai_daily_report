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
    fail_compose: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let fail_compose = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, f, s) = (count.clone(), fail_compose.clone(), stop.clone());
        let handle = thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let input = request(&mut stream);
                c.fetch_add(1, Ordering::SeqCst);
                let user: Value =
                    serde_json::from_str(input["messages"][1]["content"].as_str().unwrap())
                        .unwrap_or(Value::Null);
                let fail = user["facts"].is_array() && f.swap(false, Ordering::SeqCst);
                let (status, body) = if fail {
                    (400, b"{}".to_vec())
                } else {
                    (200,json!({"choices":[{"finish_reason":"stop","message":{"content":model_mock::reply(&input).to_string()}}],"usage":{"prompt_tokens":100,"completion_tokens":80,"total_tokens":180}}).to_string().into_bytes())
                };
                thread::sleep(Duration::from_millis(60));
                write!(stream,"HTTP/1.1 {status} test\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            port,
            count,
            fail_compose,
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
    let root = fs::canonicalize(&output).unwrap();
    let work = root.join("work");
    fs::create_dir(&work).unwrap();
    let source = work.join("approval.md");
    fs::write(&source, "采购审批方案为待评审草稿。\n").unwrap();
    File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1791680400)))
        .unwrap();
    let server = Server::new();
    let config = root.join("config");
    let db = root.join("db");
    let reports = root.join("reports");
    fs::write(config.join("settings.yaml"),json!({"paths":{"work_dir":work,"data_dir":root.join("data"),"db_dir":db,"reports_dir":reports,"log_dir":root.join("logs")},"llm":{"base_url":format!("http://127.0.0.1:{}",server.port),"model_id":"synthetic-local","max_request_utf8_bytes":64000,"context_window_tokens":100000,"verified_max_output_tokens":8192,"max_tokens":4096,"request_timeout_seconds":3},"recovery":{"enforce_model_budgets":false,"run_budget_seconds":20}}).to_string()).unwrap();
    let mut loaded = BTreeSet::new();
    let mut checks = Vec::new();
    macro_rules! ok{($args:expr)=>{{let args=$args;let(success,value,error)=run(&bundle,&config,args,&mut loaded);assert!(success,"{args:?}: {error}");checks.push(json!({"args":args,"result":value.get("action").or(value.get("status"))}));value}}}
    ok!(&["doctor", "--strict"]);
    assert_eq!(server.calls(), 0);
    ok!(&["daily", "--target-date", "2026-10-11", "--dry-run"]);
    assert_eq!(server.calls(), 0);
    let first = ok!(&["daily", "--target-date", "2026-10-11"]);
    assert_eq!(server.calls(), 2);
    assert_eq!(first["model_calls"], 2);
    let again = ok!(&["daily", "--target-date", "2026-10-11"]);
    assert_eq!(again["model_calls"], 0);
    assert_eq!(server.calls(), 2);
    let body = PathBuf::from(first["output_path"].as_str().unwrap());
    let original = fs::read(&body).unwrap();
    fs::remove_file(&body).unwrap();
    ok!(&["repair-output", "--target-date", "2026-10-11"]);
    assert_eq!(fs::read(&body).unwrap(), original);
    assert_eq!(server.calls(), 2);
    server.fail_compose.store(true, Ordering::SeqCst);
    let (failed, _, error) = run(
        &bundle,
        &config,
        &[
            "daily",
            "--target-date",
            "2026-10-11",
            "--regenerate",
            "--reason",
            "isolated recovery test",
        ],
        &mut loaded,
    );
    assert!(!failed && error.contains("MODEL_HTTP_ERROR"), "{error}");
    assert_eq!(server.calls(), 4);
    fs::write(&source, "Changed after input was frozen.\n").unwrap();
    ok!(&["daily", "--target-date", "2026-10-11"]);
    assert_eq!(server.calls(), 5);
    ok!(&["weekly", "2026-W41", "--source", "db"]);
    ok!(&["monthly", "2026-10", "--source", "db"]);
    assert_eq!(server.calls(), 7);
    File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1791680400)))
        .unwrap();
    ok!(&["weekly", "2026-W41", "--source", "scan", "--no-save"]);
    ok!(&["monthly", "2026-10", "--source", "scan", "--no-save"]);
    assert_eq!(server.calls(), 9);
    ok!(&["doctor", "--check-model"]);
    assert_eq!(server.calls(), 10);
    ok!(&["daily", "--target-date", "2026-10-10"]);
    assert_eq!(server.calls(), 10);
    ok!(&["daily", "--scheduled"]);
    assert_eq!(server.calls(), 10);
    assert!(fs::read_dir(root.join("logs")).unwrap().next().is_some());
    ok!(&["list"]);
    ok!(&["runs", "--limit", "50"]);
    let preflight = ok!(&[
        "backfill",
        "--start-date",
        "2026-10-09",
        "--end-date",
        "2026-10-09",
        "--dry-run",
    ]);
    let backfill_args = [
        "backfill",
        "--start-date",
        "2026-10-09",
        "--end-date",
        "2026-10-09",
        "--preflight",
        preflight["preflight"].as_str().unwrap(),
        "--call-budget",
        "0",
    ];
    assert_eq!(ok!(&backfill_args)["complete"], true);
    assert_eq!(ok!(&backfill_args)["complete"], true);
    let backup_path = root.join("reports-backup.sqlite3");
    ok!(&["backup", "--output", backup_path.to_str().unwrap()]);
    let backup = rusqlite::Connection::open(backup_path).unwrap();
    assert_eq!(
        backup
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let task = ok!(&["task", "plan", "--sid", "S-1-5-21-100"]);
    assert!(task.to_string().contains("ai-daily-report.exe"));
    ok!(&["cleanup"]);
    assert_eq!(server.calls(), 10);
    let connection = rusqlite::Connection::open(db.join("reports.sqlite3")).unwrap();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM report_model_calls", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 10);
    assert!(loaded
        .iter()
        .any(|p| p.to_ascii_lowercase().ends_with("ai-daily-report.exe")));
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

#[test]
#[ignore = "requires AI_DAILY_TEST_BUNDLE; standalone personal package acceptance"]
fn standalone_personal_bundle_keeps_date_budget_and_independent_evidence() {
    let bundle =
        fs::canonicalize(std::env::var_os("AI_DAILY_TEST_BUNDLE").expect("bundle required"))
            .unwrap();
    let output = PathBuf::from(std::env::var_os("AI_DAILY_ACCEPTANCE_OUTPUT").unwrap());
    let root = output.with_file_name(format!(
        "{}-personal",
        output.file_name().unwrap().to_string_lossy()
    ));
    assert!(!root.exists());
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir(root.join("work")).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let server = Server::new();
    let config = root.join("config");
    let work = root.join("work");
    let data = root.join("data");
    fs::write(config.join("settings.yaml"),json!({
        "paths":{"work_dir":work,"data_dir":data,"db_dir":root.join("db"),"reports_dir":root.join("reports"),"log_dir":root.join("logs")},
        "report":{"profile":"personal"},
        "archive":{"daily_source":"archive","run_seconds":120,"read_workers":1,"parse_workers":1},
        "schedule":{"local_time":"00:00","execution_limit_minutes":6},
        "llm":{"base_url":format!("http://127.0.0.1:{}",server.port),"model_id":"synthetic-local",
            "max_request_utf8_bytes":160000,"context_window_tokens":128000,"verified_max_output_tokens":8192,
            "max_tokens":4096,"request_timeout_seconds":3}
    }).to_string()).unwrap();
    let table = |amount| {
        let mut text = String::from("编号,金额（元）,条件,状态\n");
        for i in 1..=200 {
            text.push_str(&format!("{i},{amount},预算内且复核通过后生效,仍待审批\n"));
        }
        text
    };
    fs::write(work.join("amounts.csv"), table(100)).unwrap();
    fs::write(
        work.join("approval.txt"),
        "报价金额 100 元，预算内且复核通过后生效，仍待审批。\n",
    )
    .unwrap();
    let mut loaded = BTreeSet::new();
    let mut checks = Vec::new();
    macro_rules! ok {($args:expr)=>{{let args=$args;let(success,value,error)=run(&bundle,&config,args,&mut loaded);
        assert!(success,"{args:?}: {error} {value}");checks.push(json!({"args":args,"action":value["action"]}));value}}}
    ok!(&["doctor", "--strict"]);
    ok!(&["collect", "--baseline"]);
    fs::write(work.join("amounts.csv"), table(120)).unwrap();
    fs::write(
        work.join("approval.txt"),
        "报价金额 120 元，预算内且复核通过后生效，仍待审批。\n",
    )
    .unwrap();
    fs::write(work.join("new.txt"), "首次观察，截止日期待确认。\n").unwrap();
    let too_big = File::create(work.join("oversized.txt")).unwrap();
    too_big.set_len(51 * 1024 * 1024).unwrap();
    drop(too_big);
    let changed = ok!(&["collect"]);
    assert!(changed["errors"].as_u64().unwrap() > 0);
    let target = chrono::Utc::now()
        .with_timezone(&ai_daily_report::types::shanghai())
        .date_naive()
        .pred_opt()
        .unwrap();
    let (start, _) = ai_daily_report::types::utc_window(target).unwrap();
    // Controlled observation clock in the isolated fixture; no production input is read.
    let conn = rusqlite::Connection::open(data.join("daily_cache/archive/index.sqlite3")).unwrap();
    conn.execute(
        "UPDATE versions SET observed_at=CASE WHEN baseline=1 THEN ?1 ELSE ?2 END",
        rusqlite::params![
            (start - chrono::Duration::days(1)).to_rfc3339(),
            (start + chrono::Duration::hours(9)).to_rfc3339()
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE metadata SET value=? WHERE key='baseline'",
        [(start - chrono::Duration::days(1)).to_rfc3339()],
    )
    .unwrap();
    drop(conn);
    let date = target.to_string();
    let dry = ok!(&["daily", "--target-date", &date, "--dry-run"]);
    assert_eq!(dry["model_calls"], 0);
    let started = Instant::now();
    let report = ok!(&["daily", "--scheduled"]);
    let elapsed = started.elapsed().as_millis();
    assert_eq!(report["target_date"], date);
    assert_eq!(report["model_calls"], 1);
    assert_eq!(server.calls(), 1);
    let path = PathBuf::from(report["output_path"].as_str().unwrap());
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.len() <= 100_000 && body.contains("100") && body.contains("120"));
    assert!(body.contains("仍待审批") && body.contains("建议"));
    let reports = rusqlite::Connection::open(root.join("db/reports.sqlite3")).unwrap();
    let raw: String = reports
        .query_row(
            "SELECT raw_json FROM daily_reports WHERE date=?",
            [&date],
            |r| r.get(0),
        )
        .unwrap();
    let saved: ai_daily_report::reports::personal::PersonalReport =
        serde_json::from_str(&raw).unwrap();
    saved.validate().unwrap();
    assert!(!saved.evidence.is_empty());
    assert!(saved.selection.selected_chars <= 20_000);
    assert_eq!(saved.schema_version, 5);
    assert_eq!(ok!(&["daily", "--scheduled"])["model_calls"], 0);
    assert_eq!(server.calls(), 1);
    let search = ok!(&["search", "--query", "报价", "--format", "json"]);
    assert!(search["total"].as_u64().unwrap() > 0);
    let (_, health, error) = run(
        &bundle,
        &config,
        &["health", "--target-date", &date, "--format", "json"],
        &mut loaded,
    );
    let health_checks = health["checks"].as_array().expect(&error);
    assert!(health_checks
        .iter()
        .any(|c| c["code"] == "REPORT_NOT_PUBLISHED" && c["level"] == "ok"));
    assert!(health_checks
        .iter()
        .any(|c| c["code"] == "REPORT_CONTENT_PARTIAL" && c["level"] == "warning"));
    assert!(health_checks
        .iter()
        .any(|c| c["code"] == "PROCESSING_CACHE" && c["evidence"]["complete"] == true));
    fs::write(
        root.join("health.json"),
        serde_json::to_vec_pretty(&health).unwrap(),
    )
    .unwrap();
    // Expiry is exercised by advancing the cache ledger clock in this fixture.
    let ledger = data.join("daily_cache/.cache-ledger.json");
    let mut entries: Value = serde_json::from_slice(&fs::read(&ledger).unwrap()).unwrap();
    for entry in entries.as_object_mut().unwrap().values_mut() {
        entry["created"] = json!((chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339());
    }
    fs::write(ledger, serde_json::to_vec(&entries).unwrap()).unwrap();
    fs::remove_file(&path).unwrap();
    ok!(&["archive", "drain"]);
    ok!(&["repair-output", "--target-date", &date]);
    assert_eq!(fs::read_to_string(path).unwrap(), body);
    assert_eq!(server.calls(), 1);
    for kind in ["daily", "collect", "parse", "health"] {
        let plan = ok!(&[
            "task",
            "plan",
            "--sid",
            "S-1-5-21-1-2-3-1001",
            "--kind",
            kind
        ]);
        assert_eq!(plan["report_profile"], "personal");
        assert!(plan["processing_dir"]
            .as_str()
            .unwrap()
            .ends_with("daily_cache"));
        if kind == "daily" {
            assert!(plan["xml"].as_str().unwrap().contains("PT6M"));
        }
        if matches!(kind, "collect" | "parse") {
            assert!(plan["xml"].as_str().unwrap().contains("PT3M"));
        }
    }
    let cache_bytes: u64 = walkdir::WalkDir::new(data.join("daily_cache"))
        .into_iter()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert!(cache_bytes <= 1024 * 1024 * 1024);
    assert!(elapsed < 180_000);
    assert!(!data.join("archive").exists());
    fs::write(
        root.join("acceptance.json"),
        serde_json::to_vec_pretty(&json!({
            "status":"passed","bundle":bundle,"daily_duration_ms":elapsed,"output_bytes":body.len(),
            "cache_logical_bytes":cache_bytes,"selected_chars":saved.selection.selected_chars,
            "local_http_requests":server.calls(),"remote_model_calls":0,"real_tasks_changed":0,
            "observation_clock":"isolated fixture","checks":checks,"loaded_modules":loaded
        }))
        .unwrap(),
    )
    .unwrap();
}
