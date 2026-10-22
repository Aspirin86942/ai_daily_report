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
    time::{Duration, Instant},
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

struct ResponseVersionModel {
    sent: Arc<AtomicUsize>,
    version: Option<Value>,
    invalid_source: bool,
    extra_field: bool,
}

impl ModelTransport for ResponseVersionModel {
    fn send(&self, body: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        let mut response = Model {
            sent: self.sent.clone(),
            invalid: self.invalid_source,
        }
        .send(body, timeout)?;
        let mut envelope: Value = serde_json::from_slice(&response.body).unwrap();
        let content = &mut envelope["choices"][0]["message"]["content"];
        let mut value: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("schema_version");
        if let Some(version) = &self.version {
            value["schema_version"] = version.clone();
        }
        if self.extra_field {
            value["unexpected"] = json!(true);
        }
        *content = json!(value.to_string());
        response.body = serde_json::to_vec(&envelope).unwrap();
        Ok(response)
    }
}

#[test]
fn personal_response_version_omission_preserves_content_validation_and_date_budget() {
    for (version, invalid_source, extra_field, valid) in [
        (None, false, false, true),
        (Some(json!(5)), false, false, true),
        (Some(json!(4)), false, false, false),
        (Some(Value::Null), false, false, false),
        (Some(json!("5")), false, false, false),
        (None, true, false, false),
        (None, false, true, false),
    ] {
        let (_dir, config) = setup();
        let sent = Arc::new(AtomicUsize::new(0));
        let mut app = App::open(config, Utc::now()).unwrap();
        app.transport = Box::new(ResponseVersionModel {
            sent: sent.clone(),
            version: version.clone(),
            invalid_source,
            extra_field,
        });
        let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let outcome = run_daily(request(day, true), &app).unwrap();
        let run = app.store.get(&outcome.run_id).unwrap();
        let report: Value = serde_json::from_str(run.report_json.as_deref().unwrap()).unwrap();
        assert_eq!(report["schema_version"], 5);
        assert_eq!(
            report["items"].as_array().unwrap().len(),
            usize::from(valid)
        );
        assert_eq!(
            report["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["code"] == "MODEL_RESPONSE_INVALID"),
            !valid,
            "version={version:?}, invalid_source={invalid_source}, extra_field={extra_field}"
        );
        let calls = sent.load(Ordering::SeqCst);
        assert_eq!(
            app.store
                .personal_calls("personal_work", &day.to_string())
                .unwrap(),
            calls as u64
        );
        if valid {
            assert_eq!(calls, 1);
            assert!(outcome.markdown.contains("仍待审批"));
        }
        // Published evidence keeps its mandatory version even when the provider omitted it.
        let mut missing_version = report;
        missing_version
            .as_object_mut()
            .unwrap()
            .remove("schema_version");
        assert!(
            serde_json::from_value::<ai_daily_report::reports::personal::PersonalReport>(
                missing_version
            )
            .is_err()
        );
    }
}

#[test]
fn report_waits_for_collection_beyond_five_seconds_without_resetting_its_budget() {
    use ai_daily_report::processing_cache::{ProcessingSession, CONTROL_BYTES, MAX_BYTES};

    let (_dir, config) = setup();
    let session = ProcessingSession::at(
        &config.paths.data_dir.join("daily_cache"),
        Utc::now(),
        Instant::now() + Duration::from_secs(30),
        MAX_BYTES,
        CONTROL_BYTES,
        Duration::ZERO,
    )
    .unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5_500));
        drop(session);
    });
    let started = Instant::now();
    let result = App::open_started(config, Utc::now(), started);
    holder.join().unwrap();
    let app = result.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert_eq!(app.started, started);
    let calls: i64 = app
        .store
        .connection()
        .unwrap()
        .query_row("SELECT count(*) FROM report_model_calls", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(calls, 0);
}

#[test]
fn collection_lock_wait_respects_the_remaining_deadline() {
    use ai_daily_report::processing_cache::ProcessingSession;
    use ai_daily_report::store::locking::ReportLock;

    let (_dir, config) = setup();
    let root = config.paths.data_dir.join("daily_cache");
    let _held = ReportLock::acquire(&root.join("processing"), "personal", Duration::ZERO).unwrap();
    let started = Instant::now();
    let error =
        ProcessingSession::acquire(&config, Utc::now(), started + Duration::from_millis(150))
            .err()
            .unwrap();
    assert_eq!(error.code, "REPORT_ALREADY_RUNNING");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!root.join(".cache-ledger.json").exists());
    assert!(!config.paths.db_dir.join("reports.sqlite3").exists());
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
fn report_reclaims_standby_bytes_for_input_and_model_snapshots() {
    use ai_daily_report::{
        archive::{identity, store::ArchiveStore},
        processing_cache::ProcessingSession,
    };
    use std::time::Instant;
    let (_dir, config) = setup();
    let old = Utc::now() - chrono::Duration::days(2);
    let root = config.paths.data_dir.join("daily_cache");
    let session = ProcessingSession::at(
        &root,
        old,
        Instant::now() + Duration::from_secs(30),
        8 * 1024 * 1024,
        1024 * 1024,
        Duration::ZERO,
    )
    .unwrap();
    let store = session.open_archive(&config).unwrap();
    // Two 3 MiB standby objects leave less space than the existing 2 MiB
    // response reservation. A report must reclaim one before its model call.
    let objects: Vec<_> = (0..2)
        .map(|i| {
            let hash = identity(&[&i.to_string()]);
            let path = store.object(&hash).unwrap();
            let mut reservation = session.quota.reserve(3 * 1024 * 1024).unwrap();
            reservation.track(&path).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, vec![b'a'; 3 * 1024 * 1024]).unwrap();
            reservation.commit(std::slice::from_ref(&path)).unwrap();
            path
        })
        .collect();
    drop(store);
    drop(session);
    let now = Utc::now();
    let mut app = App::open(config, now).unwrap();
    app.processing = None;
    app.processing = Some(
        ProcessingSession::at(
            &root,
            now,
            Instant::now() + Duration::from_secs(30),
            8 * 1024 * 1024,
            1024 * 1024,
            Duration::ZERO,
        )
        .unwrap(),
    );
    let sent = Arc::new(AtomicUsize::new(0));
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: false,
    });
    let day = now
        .with_timezone(&ai_daily_report::types::shanghai())
        .date_naive()
        .pred_opt()
        .unwrap();
    let output = run_daily(request(day, true), &app).unwrap();
    assert_eq!(output.action, "published");
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    assert!(objects.iter().any(|path| !path.exists()));
    assert!(app.processing.as_ref().unwrap().quota.used_bytes().unwrap() <= 8 * 1024 * 1024);
    assert!(ArchiveStore::open_collection(&app.config, false).is_ok());
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
