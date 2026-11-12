use ai_daily_report::{
    config::AppConfig,
    health,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    operations,
    reports::personal::{PersonalReport, MAX_FILE_CHARS, MAX_REPORT_BYTES},
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
        assert_eq!(config.scanner.total_max_chars, Some(MAX_FILE_CHARS as u64));
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
fn expanded_scan_material_reaches_model_and_large_attachment_is_restored_without_loss() {
    let mut f = Fixture::new();
    f.app.config.llm.max_request_utf8_bytes = 1_000_000;
    f.app.config.llm.context_window_tokens = Some(1_000_000);
    let start = utc_window(f.day).unwrap().0;
    let files: BTreeMap<_, _> = (0..30)
        .map(|index| {
            let name = format!("material-{index:02}.md");
            let text: String = (0..100)
                .map(|line| {
                    format!(
                        "材料 {index:02} 第 {line:03} 项：金额 120 元，审批条件和负责人待确认。\n"
                    )
                })
                .collect();
            f.file(&name, &text, start);
            (name, text)
        })
        .collect();
    let material_chars: usize = files.values().map(|text| text.chars().count()).sum();
    assert!(material_chars > 100_000);
    assert!(material_chars < MAX_FILE_CHARS);

    let result = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(result.action, "published");
    let sent = f.sent.lock().unwrap();
    assert_eq!(
        sent.len(),
        1,
        "{}",
        f.app
            .store
            .get(&result.run_id)
            .unwrap()
            .report_json
            .unwrap()
    );
    let sources = sent[0]["sources"].as_array().unwrap();
    assert_eq!(sources.len(), files.len());
    for source in sources {
        let path = source["path"].as_str().unwrap();
        let text = source["text"].as_str().unwrap();
        let expected = files[path].replace('\n', "");
        assert!(
            text.replace('\n', "").ends_with(expected.as_str()),
            "{path}"
        );
    }
    drop(sent);

    let stored = f.app.store.get(&result.run_id).unwrap();
    let report: PersonalReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    report.validate().unwrap();
    assert_eq!(report.selection.selected_files, files.len() as u64);
    assert!(report.selection.selected_chars >= material_chars);
    assert_eq!(report.selection.omissions.get("budget"), None);
    assert_eq!(report.sources.len(), files.len());
    let attachment = report.evidence_attachment.as_ref().unwrap();
    assert!(attachment.markdown.len() > 100_000);
    assert!(report.output_bytes(&result.markdown) <= MAX_REPORT_BYTES);
    for text in files.values() {
        for line in text.lines() {
            assert!(attachment.markdown.contains(line));
        }
    }
    for source in &report.sources {
        let text = &report.evidence[&source.source_id][0].quote;
        let expected = files[source.relative_path.as_deref().unwrap()].replace('\n', "");
        assert!(text.replace('\n', "").ends_with(expected.as_str()));
    }
    let path = result.output_path.as_ref().unwrap();
    let evidence_path = path
        .parent()
        .unwrap()
        .join(attachment.link(&f.day.to_string()));
    let original = fs::read(path).unwrap();
    let original_evidence = attachment.markdown.as_bytes();
    assert_eq!(fs::read(&evidence_path).unwrap(), original_evidence);
    assert!(result
        .markdown
        .contains(&attachment.link(&f.day.to_string())));

    let scans = f.scans();
    fs::rename(
        &f.app.config.paths.work_dir,
        f.app.config.paths.work_dir.with_file_name("offline-work"),
    )
    .unwrap();
    fs::remove_file(&evidence_path).unwrap();
    ai_daily_report::run::repair_output(&f.app, &f.day.to_string(), Utc::now()).unwrap();
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(fs::read(&evidence_path).unwrap(), original_evidence);
    let again = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(again.action, "existing");
    assert_eq!(again.markdown, result.markdown);
    assert_eq!(f.scans(), scans);
    assert_eq!(f.sent.lock().unwrap().len(), 1);
}

#[test]
fn expanded_context_admits_all_files_before_bounding_personal_material() {
    let mut f = Fixture::new();
    let settings_path = f.app.config.config_dir.join("settings.yaml");
    let mut settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    settings["scanner"]["context_max_chars"] = json!(5_000_000);
    settings["scanner"]["text_max_chars"] = json!(60_000);
    settings["scanner"]["text_excerpt_max_chars"] = json!(60_000);
    settings["llm"]["max_request_utf8_bytes"] = json!(1_600_000);
    settings["llm"]["context_window_tokens"] = json!(1_050_000);
    fs::write(&settings_path, settings.to_string()).unwrap();
    f.app.config = AppConfig::load_with_env(f._dir.path(), None, &BTreeMap::new()).unwrap();
    let start = utc_window(f.day).unwrap().0;
    let files: BTreeMap<_, _> = (0..12)
        .map(|index| {
            let path = format!("wide-context-{index:02}.md");
            let marker = format!("材料 {index:02}：");
            let line = format!(
                "{marker}{}。\n",
                "金额 120 元，审批条件和负责人待确认；".repeat(7)
            );
            f.file(&path, &line.repeat(300), start);
            (path, marker)
        })
        .collect();

    let result = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(result.action, "published");
    let sent = f.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let sources = sent[0]["sources"].as_array().unwrap();
    assert_eq!(sources.len(), files.len());
    for source in sources {
        let path = source["path"].as_str().unwrap();
        assert!(source["text"]
            .as_str()
            .unwrap()
            .contains(files[path].as_str()));
    }
    let selected_chars: usize = sources
        .iter()
        .map(|source| source["text"].as_str().unwrap().chars().count())
        .sum();
    assert!(selected_chars > 100_000);
    assert!(selected_chars <= MAX_FILE_CHARS);
    drop(sent);

    let (parsed_files, parsed_chars): (i64, i64) =
        rusqlite::Connection::open(&f.app.config.index_db_path)
            .unwrap()
            .query_row(
                "SELECT count(*), coalesce(sum(length(json_extract(content, '$.text'))), 0) FROM parse_cache",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    assert_eq!(parsed_files, files.len() as i64);
    assert!(parsed_chars > 500_000);
    let stored = f.app.store.get(&result.run_id).unwrap();
    let report: PersonalReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    report.validate().unwrap();
    assert_eq!(report.selection.selected_files, files.len() as u64);
    assert_eq!(report.selection.selected_chars, selected_chars);
    assert_eq!(report.selection.omissions.get("read_failed"), None);
    assert!(!report
        .gaps
        .iter()
        .any(|gap| gap.code == "SOURCE_UNAVAILABLE"));
    assert!(report.sources.iter().all(|source| source.usable()));
}

#[test]
fn scan_dry_run_keeps_budgets_and_reports_unread_ranges_without_model_calls() {
    let f = Fixture::new();
    let start = utc_window(f.day).unwrap().0;
    for i in 0..40 {
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
    let selected_chars = selection["summary"]["selected_chars"].as_u64().unwrap();
    assert!(selected_chars > 20_000);
    assert!(selected_chars <= MAX_FILE_CHARS as u64);
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
fn legacy_scan_cap_resumes_saved_response_with_same_yaml_and_rejects_other_changes() {
    use ai_daily_report::{
        model::ModelRunner,
        run::personal::{finish, PersonalInput},
        snapshots::{SnapshotRef, SnapshotStore},
    };
    use std::time::Instant;

    for configured in [None, Some(50_000)] {
        let mut f = Fixture::new();
        let settings_path = f.app.config.config_dir.join("settings.yaml");
        let mut settings: Value =
            serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        if let Some(chars) = configured {
            settings["scanner"]["total_max_chars"] = json!(chars);
        }
        fs::write(&settings_path, settings.to_string()).unwrap();
        let yaml = fs::read(&settings_path).unwrap();
        f.app.config = AppConfig::load_with_env(f._dir.path(), None, &BTreeMap::new()).unwrap();
        f.app.config.scanner.total_max_chars = Some(18_000);
        let original = "金额 120 元。满足条件后生效；责任人与截止日期待确认。";
        f.file("work.md", original, utc_window(f.day).unwrap().0);
        f.app.transport = Box::new(Interrupted);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_daily(f.request(), &f.app)
        }))
        .is_err());
        let run = f
            .app
            .store
            .pending(Some(&f.day.to_string()))
            .unwrap()
            .remove(0);
        let input_ref = SnapshotRef {
            path: run.snapshot_path.clone().unwrap(),
            sha256: run.snapshot_sha256.clone().unwrap(),
        };
        let session = f.app.processing.as_ref().unwrap();
        let snapshots = SnapshotStore::with_quota(
            session.quota.root.join("snapshots"),
            ai_daily_report::processing_cache::MAX_BYTES,
            Arc::clone(&session.quota),
        )
        .unwrap();
        let frozen_bytes = snapshots.read_bytes(&input_ref).unwrap();
        let input: PersonalInput = serde_json::from_slice(&frozen_bytes).unwrap();
        let model = Model(f.sent.clone());
        let started = Instant::now();
        let saved = ModelRunner {
            store: &f.app.store,
            snapshots: &snapshots,
            transport: &model,
            llm: &f.app.config.llm,
            allow_new_calls: true,
            run_id: &run.run_id,
            started_at: Utc::now(),
            started,
            deadline: started + Duration::from_secs(60),
        }
        .execute(input.plan.as_ref().unwrap())
        .unwrap();
        assert!(!saved.reused);
        let calls = f.app.store.calls(&run.run_id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["status"], "succeeded");

        let config = AppConfig::load_with_env(f._dir.path(), None, &BTreeMap::new()).unwrap();
        assert_eq!(
            config.scanner.total_max_chars,
            Some(configured.unwrap_or(MAX_FILE_CHARS as u64))
        );
        assert_ne!(config.fingerprint(false).unwrap(), run.config_fingerprint);
        assert!(config
            .matches_personal_fingerprint(&run.config_fingerprint, false)
            .unwrap());
        assert!(config
            .matches_personal_fingerprint(&input.recovery_fingerprint, true)
            .unwrap());
        let Fixture {
            _dir,
            app,
            day,
            sent,
        } = f;
        drop(app);
        let mut f = Fixture {
            app: App::open(config, Utc::now()).unwrap(),
            _dir,
            day,
            sent,
        };
        f.app.transport = Box::new(Interrupted);
        let unchanged = f.app.config.clone();
        let mut restricted = unchanged.clone();
        restricted.scanner.excluded_dirs = Some(vec![unchanged
            .paths
            .work_dir
            .to_string_lossy()
            .into_owned()]);
        assert!(!ai_daily_report::evidence::permitted(
            &restricted,
            &unchanged.paths.work_dir.join("work.md")
        ));
        let mut different_model = unchanged.clone();
        different_model.llm.model_id.push_str("-changed");
        let mut different_path = unchanged.clone();
        different_path.paths.work_dir = different_path.paths.work_dir.join("other-work");
        for changed in [restricted, different_model, different_path] {
            f.app.config = changed;
            let started = Instant::now();
            assert_eq!(
                finish(
                    &f.app,
                    &run.run_id,
                    started,
                    started + Duration::from_secs(60)
                )
                .unwrap_err()
                .code,
                "SNAPSHOT_CONTEXT_MISMATCH"
            );
        }
        f.app.config = unchanged;
        let scans = f.scans();
        fs::rename(
            &f.app.config.paths.work_dir,
            f.app.config.paths.work_dir.with_file_name("offline-work"),
        )
        .unwrap();
        let result = run_daily(f.request(), &f.app).unwrap();
        assert_eq!(result.action, "published");
        assert_eq!(result.run_id, run.run_id);
        assert_eq!(f.scans(), scans);
        assert_eq!(f.sent.lock().unwrap().len(), 1);
        assert_eq!(f.app.store.calls(&run.run_id).unwrap(), calls);
        assert_eq!(snapshots.read_bytes(&input_ref).unwrap(), frozen_bytes);
        assert_eq!(fs::read(&settings_path).unwrap(), yaml);
        let stored = f.app.store.get(&result.run_id).unwrap();
        assert_eq!(stored.config_fingerprint, run.config_fingerprint);
        let report: PersonalReport =
            serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
        assert!(!report.items.is_empty());
        assert_eq!(
            report.evidence.values().next().unwrap()[0].quote,
            input.selection.materials[0].text
        );
        let attachment = report.evidence_attachment.unwrap();
        for sentence in original.split('。').filter(|part| !part.is_empty()) {
            assert!(attachment.markdown.contains(sentence));
        }
    }
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
