use ai_daily_report::{
    config::AppConfig,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    run::{run_daily, App},
    types::{new_run_id, Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use chrono::{NaiveDate, Utc};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct Model {
    sent: Arc<AtomicUsize>,
    invalid: bool,
}
impl ModelTransport for Model {
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        let report = json!({"schema_version":5,"items":[{
            "title":"报价仍待审批","text":"金额由 100 调整到 120，条件满足后生效；责任人与截止日期待确认。",
            "kind":"attention","statement_kind":"user_statement",
            "source_ids":[if self.invalid {"missing"} else {"user_input"}]
        }]});
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: serde_json::to_vec(&json!({
                "choices":[{"finish_reason":"stop","message":{"content":report.to_string()}}],
                "usage":{"prompt_tokens":100,"completion_tokens":100}
            }))
            .unwrap(),
        })
    }
}

fn setup() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::create_dir(dir.path().join("work")).unwrap();
    fs::write(dir.path().join("config/settings.yaml"), json!({
        "paths":{"work_dir":"work"}, "report":{"profile":"personal"},
        "archive":{"daily_source":"archive"},
        "llm":{"max_request_utf8_bytes":128000,"context_window_tokens":128000,"verified_max_output_tokens":8192,"max_tokens":4096},
        "recovery":{"enforce_model_budgets":false}
    }).to_string()).unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    (dir, config)
}

fn request(day: NaiveDate, save: bool) -> ReportRequest {
    ReportRequest {
        context: RunContext::new(new_run_id(), Utc::now(), day, String::new()).unwrap(),
        period: Period {
            mode: ReportMode::Daily,
            source: ReportSource::Archive,
            start: day,
            end: day,
            label: day.to_string(),
        },
        input: "金额由 100 调整到 120，条件满足后生效，仍待审批；责任人与截止日期待确认。".into(),
        save,
        dry_run: false,
        scheduled: false,
        regeneration_reason: None,
    }
}

#[test]
fn publishes_one_request_and_repairs_after_processing_content_expires() {
    let (_dir, config) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    let mut app = App::open(config.clone(), Utc::now()).unwrap();
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: false,
    });
    let output = run_daily(request(day, true), &app).unwrap();
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    assert_eq!(output.action, "published");
    assert!(output.markdown.len() <= 100_000);
    assert!(output.markdown.contains("仍待审批"));
    let value: Value = serde_json::from_str(
        app.store
            .get(&output.run_id)
            .unwrap()
            .report_json
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["schema_version"], 5);
    assert_eq!(app.store.reports_in_range(day, day).unwrap().0.len(), 1);
    let path = output.output_path.unwrap();
    let original = fs::read(&path).unwrap();
    drop(app);
    let future = Utc::now() + chrono::Duration::days(8);
    let mut app = App::open(config, future).unwrap();
    app.processing
        .as_ref()
        .unwrap()
        .open_archive(&app.config)
        .unwrap();
    fs::remove_file(&path).unwrap(); // isolated publication-repair fixture
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: true,
    });
    let repaired = run_daily(request(day, true), &app).unwrap();
    assert_eq!(repaired.action, "existing");
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    assert_eq!(
        app.store
            .personal_calls("personal_work", &day.to_string())
            .unwrap(),
        1
    );
}

#[test]
fn date_budget_survives_reopen_and_ignores_legacy_budget_switch() {
    let (_dir, config) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    for attempt in 0..4 {
        let mut app = App::open(config.clone(), Utc::now()).unwrap();
        app.transport = Box::new(Model {
            sent: sent.clone(),
            invalid: false,
        });
        let output = run_daily(request(day, false), &app).unwrap();
        assert_eq!(output.model_calls, if attempt < 3 { 1 } else { 0 });
        if attempt == 3 {
            assert!(output.markdown.contains("模型"));
        }
    }
    assert_eq!(sent.load(Ordering::SeqCst), 3);
    {
        let app = App::open(config.clone(), Utc::now() + chrono::Duration::days(31)).unwrap();
        assert_eq!(
            app.store
                .personal_calls("personal_work", &day.to_string())
                .unwrap(),
            3
        );
        let count: i64 = app
            .store
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM report_model_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "detail compaction must preserve independent date counters"
        );
    }
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: false,
    });
    run_daily(request(day.succ_opt().unwrap(), false), &app).unwrap();
    assert_eq!(sent.load(Ordering::SeqCst), 4);
}

#[test]
fn invalid_sources_never_publish_as_facts_and_requests_remain_bounded() {
    let (_dir, config) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    for _ in 0..4 {
        let mut app = App::open(config.clone(), Utc::now()).unwrap();
        app.transport = Box::new(Model {
            sent: sent.clone(),
            invalid: true,
        });
        let output = run_daily(
            request(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap(), false),
            &app,
        )
        .unwrap();
        assert_eq!(output.quality, "partial");
        let run = app.store.get(&output.run_id).unwrap();
        let report: Value = serde_json::from_str(run.report_json.as_deref().unwrap()).unwrap();
        assert!(report["items"].as_array().unwrap().is_empty());
        assert!(report["user_notes"].as_str().unwrap().contains("100"));
    }
    assert_eq!(sent.load(Ordering::SeqCst), 3);
}

#[test]
fn oversized_notes_and_scan_source_fail_before_model_send() {
    let (_dir, config) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: false,
    });
    let mut input = request(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap(), false);
    input.input = "🦀".repeat(2001);
    assert_eq!(
        run_daily(input, &app).unwrap_err().code,
        "USER_INPUT_TOO_LARGE"
    );
    let mut input = request(NaiveDate::from_ymd_opt(2026, 10, 19).unwrap(), false);
    input.period.source = ReportSource::Scan;
    assert_eq!(
        run_daily(input, &app).unwrap_err().code,
        "PERSONAL_SOURCE_INVALID"
    );
    assert_eq!(sent.load(Ordering::SeqCst), 0);
}

#[test]
fn no_change_requires_complete_observation_and_never_calls_the_model() {
    let (_dir, config) = setup();
    let now = Utc::now();
    let day = (now - chrono::Duration::days(1)).date_naive();
    let (start, end) = ai_daily_report::types::utc_window(day).unwrap();
    ai_daily_report::archive::collect::run(&config, true, start - chrono::Duration::days(1))
        .unwrap();
    let store =
        ai_daily_report::archive::store::ArchiveStore::open_collection(&config, true).unwrap();
    for tick in 0..48 {
        let at = (start + chrono::Duration::minutes(tick * 30)).to_rfc3339();
        store.conn.execute("INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES(?,?,?,'complete',0)",
            rusqlite::params![format!("test-{tick}"),at,at]).unwrap();
    }
    drop(store);
    let sent = Arc::new(AtomicUsize::new(0));
    let mut app = App::open(config, now).unwrap();
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: false,
    });
    let mut input = request(day, false);
    input.input.clear();
    let output = run_daily(input.clone(), &app).unwrap();
    assert_eq!(output.quality, "no_changes");
    let store =
        ai_daily_report::archive::store::ArchiveStore::open_collection(&app.config, true).unwrap();
    store
        .conn
        .execute("DELETE FROM collections WHERE id='test-20'", [])
        .unwrap();
    drop(store);
    input.context = RunContext::new(
        new_run_id(),
        end + chrono::Duration::seconds(1),
        day,
        String::new(),
    )
    .unwrap();
    assert_eq!(run_daily(input, &app).unwrap().quality, "partial");
    assert_eq!(sent.load(Ordering::SeqCst), 0);
}

#[test]
fn expired_work_budget_starts_no_model_request_and_publication_conflicts_are_preserved() {
    let (_dir, config) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let mut app = App::open_started(
        config.clone(),
        Utc::now(),
        std::time::Instant::now() - Duration::from_secs(271),
    )
    .unwrap();
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: false,
    });
    let day = Utc::now().date_naive();
    assert_eq!(
        run_daily(request(day, true), &app).unwrap_err().code,
        "PROCESSING_DEADLINE"
    );
    assert_eq!(sent.load(Ordering::SeqCst), 0);
    drop(app);
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: false,
    });
    let output = run_daily(request(day, true), &app).unwrap();
    let path = output.output_path.unwrap();
    fs::write(&path, "user edit").unwrap();
    assert!(run_daily(request(day, true), &app).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "user edit");
    assert_eq!(sent.load(Ordering::SeqCst), 1);
}

#[test]
fn personal_operation_logs_have_separate_retention_and_preserve_legacy_logs() {
    let (_dir, config) = setup();
    fs::create_dir_all(config.paths.log_dir.join("personal")).unwrap();
    let legacy = config.paths.log_dir.join("scheduled-2000-01-01.jsonl");
    fs::write(&legacy, "legacy").unwrap();
    let expired = config
        .paths
        .log_dir
        .join("personal/scheduled-2000-01-01.jsonl");
    fs::write(&expired, "expired").unwrap();
    let day = Utc::now()
        .with_timezone(&ai_daily_report::types::shanghai())
        .date_naive()
        .to_string();
    let current = config
        .paths
        .log_dir
        .join(format!("personal/scheduled-{day}.jsonl"));
    fs::write(&current, vec![b' '; 2 * 1024 * 1024]).unwrap();
    ai_daily_report::operations::append_scheduled_log(
        &config,
        "daily",
        12,
        None,
        Some(&day),
        &Err(ai_daily_report::ReportError::new(
            "PERSONAL_CALL_LIMIT",
            "date budget exhausted",
        )),
    )
    .unwrap();
    assert!(!expired.exists());
    assert_eq!(fs::read_to_string(legacy).unwrap(), "legacy");
    assert!(fs::metadata(&current).unwrap().len() < 4096);
    let event: Value = serde_json::from_str(fs::read_to_string(current).unwrap().trim()).unwrap();
    assert_eq!(event["retryable"], false);
    assert_eq!(event["termination"], "application_exit");
}
