use ai_daily_report::{
    config::AppConfig,
    health,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    operations,
    reports::personal::PersonalReport,
    run::{run_daily, App},
    types::{new_run_id, utc_window, Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, FileTimes},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

struct Model(Arc<Mutex<Vec<Value>>>);
impl ModelTransport for Model {
    fn send(&self, bytes: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        let request: Value = serde_json::from_slice(bytes).unwrap();
        assert!(request["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("每日一次扫描"));
        let input: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        let ids: Vec<_> = input["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["source_id"].clone())
            .collect();
        self.0.lock().unwrap().push(input);
        let response = json!({"schema_version":5,"items":[{
            "title":"方案仍待审批", "text":"金额 120 元，满足条件后生效；责任人与截止日期待确认。",
            "kind":"attention", "statement_kind":"observed", "source_ids":ids
        }]});
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: serde_json::to_vec(&json!({
                "choices":[{"finish_reason":"stop","message":{"content":response.to_string()}}],
                "usage":{"prompt_tokens":100,"completion_tokens":100}
            }))
            .unwrap(),
        })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    app: App,
    day: NaiveDate,
    sent: Arc<Mutex<Vec<Value>>>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        let workers = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
        fs::write(dir.path().join("config/settings.yaml"),json!({
            "paths":{"work_dir":"work"}, "report":{"profile":"personal"},
            "archive":{"allow_work_dir_for_model":true},
            "scanner":{"office_worker_path":fs::canonicalize(workers.join("ai-daily-office-parser.exe")).unwrap(),
                "pdf_worker_path":fs::canonicalize(workers.join("ai-daily-pdf-parser.exe")).unwrap()},
            "llm":{"max_request_utf8_bytes":256000,"context_window_tokens":256000,"verified_max_output_tokens":8192,"max_tokens":4096},
            "recovery":{"enforce_model_budgets":false}
        }).to_string()).unwrap();
        let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        assert_eq!(config.scanner.max_workers, Some(1));
        assert_eq!(config.scanner.total_max_chars, Some(18_000));
        assert_eq!(config.scanner.total_deadline_ms, Some(120_000));
        let mut app = App::open(config, Utc::now()).unwrap();
        let sent = Arc::new(Mutex::new(vec![]));
        app.transport = Box::new(Model(sent.clone()));
        let day = Utc::now()
            .with_timezone(&ai_daily_report::types::shanghai())
            .date_naive()
            .pred_opt()
            .unwrap();
        Self {
            _dir: dir,
            app,
            day,
            sent,
        }
    }
    fn file(&self, name: &str, text: &str, at: DateTime<Utc>) {
        let path = self.app.config.paths.work_dir.join(name);
        fs::write(&path, text).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(at.timestamp() as u64)),
            )
            .unwrap();
    }
    fn request(&self) -> ReportRequest {
        ReportRequest {
            context: RunContext::new(new_run_id(), Utc::now(), self.day, String::new()).unwrap(),
            period: Period {
                mode: ReportMode::Daily,
                source: ReportSource::Scan,
                start: self.day,
                end: self.day,
                label: self.day.to_string(),
            },
            input: String::new(),
            save: true,
            dry_run: false,
            scheduled: true,
            regeneration_reason: None,
        }
    }
    fn scans(&self) -> i64 {
        rusqlite::Connection::open(&self.app.config.index_db_path)
            .unwrap()
            .query_row("SELECT count(*) FROM scan_runs", [], |r| r.get(0))
            .unwrap()
    }
}

#[test]
fn daily_scan_covers_calendar_boundaries_publishes_and_reuses_without_collection() {
    let f = Fixture::new();
    let (start, end) = utc_window(f.day).unwrap();
    let text = "金额 120 元。满足条件后生效；例外情况仍需审批，责任人与截止日期待确认。";
    f.file("start.md", text, start);
    f.file("end.md", text, end - chrono::Duration::seconds(1));
    f.file(
        "older.md",
        "previous day",
        start - chrono::Duration::seconds(1),
    );
    f.file("today.md", "next day", end);
    let request = f.request();
    assert_eq!(request.context.target_date, f.day);
    let result = run_daily(request, &f.app).unwrap();
    assert_eq!(result.action, "published");
    let sent = f.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let sources = sent[0]["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    for source in sources {
        assert!(matches!(
            source["path"].as_str(),
            Some("start.md" | "end.md")
        ));
        assert!(source["text"]
            .as_str()
            .unwrap()
            .replace('\n', "")
            .ends_with(text));
        assert_eq!(source["parts"][0]["side"], "current");
    }
    drop(sent);
    let stored = f.app.store.get(&result.run_id).unwrap();
    let report: PersonalReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    report.validate().unwrap();
    for refs in report.evidence.values() {
        assert!(refs[0].version_id.starts_with("scan-content:"));
        assert!(!refs[0].ranges.is_empty());
    }
    assert!(result.markdown.contains("次日扫描时的内容"));
    assert!(!f
        .app
        .config
        .paths
        .data_dir
        .join("daily_cache/archive")
        .exists());
    let check = health::check(&f.app.config, f.day, Utc::now()).unwrap();
    assert!(!check
        .checks
        .iter()
        .any(|c| matches!(c.code, "COLLECTION_GAP" | "PARSE_QUEUE")));
    let scans = f.scans();
    fs::rename(
        &f.app.config.paths.work_dir,
        f.app.config.paths.work_dir.with_file_name("offline-work"),
    )
    .unwrap();
    let again = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(again.action, "existing");
    assert_eq!(again.markdown, result.markdown);
    assert_eq!(f.scans(), scans);
    assert_eq!(f.sent.lock().unwrap().len(), 1);
    let attachment = report.evidence_attachment.unwrap();
    let path = Path::new(result.output_path.as_ref().unwrap())
        .parent()
        .unwrap()
        .join(attachment.link(&f.day.to_string()));
    fs::remove_file(&path).unwrap();
    run_daily(f.request(), &f.app).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), attachment.markdown);
}

#[test]
fn scan_dry_run_keeps_budgets_and_reports_unread_ranges_without_model_calls() {
    let f = Fixture::new();
    let start = utc_window(f.day).unwrap().0;
    for i in 0..6 {
        f.file(
            &format!("{i}.md"),
            &"金额待核对，未经审批不得生效。\n".repeat(350),
            start,
        );
    }
    let mut request = f.request();
    request.dry_run = true;
    let result = run_daily(request, &f.app).unwrap();
    assert_eq!(result.action, "dry_run");
    let selection: Value = serde_json::from_str(&result.markdown).unwrap();
    assert!(selection["summary"]["selected_chars"].as_u64().unwrap() <= 18_000);
    assert!(!selection["gaps"].as_array().unwrap().is_empty());
    assert!(f.sent.lock().unwrap().is_empty());
    assert!(f.app.store.current(&f.day.to_string()).unwrap().is_none());
    let attempts = || -> i64 {
        rusqlite::Connection::open(&f.app.config.index_db_path)
            .unwrap()
            .query_row(
                "SELECT coalesce(sum(parse_attempt_count),0) FROM scan_file_execution",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let before = attempts();
    let mut request = f.request();
    request.dry_run = true;
    run_daily(request, &f.app).unwrap();
    assert_eq!(
        attempts(),
        before,
        "unchanged files must reuse their parsed content"
    );
}

struct Interrupted;
impl ModelTransport for Interrupted {
    fn prepare(&self) -> Result<(), ai_daily_report::ReportError> {
        panic!("interrupted after input freeze")
    }
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        unreachable!()
    }
}

#[test]
fn interrupted_scan_resumes_frozen_content_after_source_changes() {
    let mut f = Fixture::new();
    let start = utc_window(f.day).unwrap().0;
    let original = "金额 120 元。满足条件后生效；责任人与截止日期待确认。";
    f.file("work.md", original, start);
    f.app.transport = Box::new(Interrupted);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_daily(
            f.request(),
            &f.app
        )))
        .is_err()
    );
    let scans = f.scans();
    f.file("work.md", "changed later", Utc::now());
    f.app.transport = Box::new(Model(f.sent.clone()));
    let result = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(result.action, "published");
    assert_eq!(f.scans(), scans);
    assert!(f.sent.lock().unwrap()[0]["sources"][0]["text"]
        .as_str()
        .unwrap()
        .replace('\n', "")
        .ends_with(original));
}

#[test]
fn personal_scan_plan_has_one_daily_trigger_and_no_background_recovery() {
    let f = Fixture::new();
    fs::write(
        f.app.config.program_dir.join("ai-daily-report.exe"),
        "fixture",
    )
    .unwrap();
    let plan = operations::task_plan(&f.app.config, "S-1-5-21-123", Utc::now()).unwrap();
    let xml = plan["xml"].as_str().unwrap();
    assert_eq!(xml.matches("<CalendarTrigger").count(), 1);
    assert!(xml.contains("T09:00:00+08:00"));
    assert!(!xml.contains("LogonTrigger"));
    assert!(!xml.contains("RestartOnFailure"));
    assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
    for kind in ["collect", "parse"] {
        assert_eq!(
            operations::task_plan_kind(&f.app.config, "S-1-5-21-123", Utc::now(), kind)
                .unwrap_err()
                .code,
            "TASK_KIND_INVALID"
        );
    }
}
