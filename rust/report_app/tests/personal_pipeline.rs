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
        Arc, Mutex,
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
        "scanner":{"office_worker_path": std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/ai-daily-office-parser.exe"),
            "pdf_worker_path": std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug/ai-daily-pdf-parser.exe")},
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
            source: ReportSource::Scan,
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

struct OfflineModel(Arc<AtomicUsize>);
impl ModelTransport for OfflineModel {
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(TransportFailure::Timeout)
    }
}

#[test]
fn recovery_alert_survives_material_expiry_without_extending_retention() {
    use ai_daily_report::personal_recovery;
    let (_dir, config) = setup();
    let now = Utc::now();
    let day = now
        .with_timezone(&ai_daily_report::types::shanghai())
        .date_naive()
        .pred_opt()
        .unwrap();
    let mut app = App::open(config.clone(), now).unwrap();
    app.transport = Box::new(OfflineModel(Arc::new(AtomicUsize::new(0))));
    run_daily(request(day, true), &app).unwrap();
    let later = now + chrono::Duration::days(9);
    let issues =
        personal_recovery::issues(&app.store.connection().unwrap(), "personal_work", later)
            .unwrap();
    assert_eq!(issues.len(), 1);
    assert!(!issues[0].material_window_open);
}

struct ResponseVersionModel {
    sent: Arc<AtomicUsize>,
    version: Option<Value>,
    invalid_source: bool,
    extra_field: bool,
}

#[test]
fn unpublished_failure_alert_survives_diagnostic_compaction() {
    use ai_daily_report::personal_recovery;
    let (_dir, config) = setup();
    let now = Utc::now();
    let past = now - chrono::Duration::days(40);
    let app = App::open(config, now).unwrap();
    let context = RunContext::new(
        new_run_id(),
        past,
        past.with_timezone(&ai_daily_report::types::shanghai())
            .date_naive(),
        String::new(),
    )
    .unwrap();
    app.store
        .create(&context, "report", &context.run_id, None, past)
        .unwrap();
    app.store.register_personal_run(&context.run_id).unwrap();
    app.store
        .fail(&context.run_id, "MODEL_TIMEOUT", false, past)
        .unwrap();
    app.store
        .compact_personal(
            now,
            &app.processing
                .as_ref()
                .unwrap()
                .quota
                .root
                .join("snapshots"),
        )
        .unwrap();
    assert!(app.store.get(&context.run_id).is_err());
    let issues =
        personal_recovery::issues(&app.store.connection().unwrap(), "personal_work", now).unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].reason_codes, vec!["MODEL_TIMEOUT"]);
    assert!(!issues[0].material_window_open);
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
fn personal_response_version_omission_preserves_content_validation_and_call_audit() {
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
fn report_waits_for_processing_lock_beyond_five_seconds_without_resetting_its_budget() {
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
fn processing_lock_wait_respects_the_remaining_deadline() {
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
    let evidence: ai_daily_report::reports::personal::EvidenceAttachment =
        serde_json::from_value(value["evidence_attachment"].clone()).unwrap();
    let evidence_path = path.parent().unwrap().join(evidence.link(&day.to_string()));
    assert_eq!(
        fs::read_to_string(&evidence_path).unwrap(),
        evidence.markdown
    );
    assert!(output.markdown.contains(&evidence.link(&day.to_string())));
    assert!(!output.markdown.contains("## 用户补充原文"));
    assert!(output.markdown.len() + evidence.markdown.len() <= 100_000);
    let original = fs::read(&path).unwrap();
    drop(app);
    let future = Utc::now() + chrono::Duration::days(8);
    let mut app = App::open(config, future).unwrap();
    app.processing.as_ref().unwrap().prune_snapshots().unwrap();
    fs::remove_file(&evidence_path).unwrap(); // isolated publication-repair fixture
    let health = ai_daily_report::health::check(&app.config, day, future).unwrap();
    assert!(health
        .checks
        .iter()
        .any(|check| check.code == "REPORT_EVIDENCE_MISMATCH" && check.level == "error"));
    // Repair must restore a missing attachment even while the summary is intact.
    ai_daily_report::run::repair_output(&app, &day.to_string(), future).unwrap();
    assert_eq!(
        fs::read_to_string(&evidence_path).unwrap(),
        evidence.markdown
    );
    fs::remove_file(&path).unwrap();
    fs::remove_file(&evidence_path).unwrap();
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: true,
    });
    let repaired = run_daily(request(day, true), &app).unwrap();
    assert_eq!(repaired.action, "existing");
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(
        fs::read_to_string(&evidence_path).unwrap(),
        evidence.markdown
    );
    let health = ai_daily_report::health::check(&app.config, day, future).unwrap();
    assert!(!health
        .checks
        .iter()
        .any(|check| check.code == "REPORT_EVIDENCE_MISMATCH"));
    fs::write(&evidence_path, "人工修改的依据").unwrap();
    let error = ai_daily_report::run::repair_output(&app, &day.to_string(), future).unwrap_err();
    assert_eq!(error.code, "REPORT_EVIDENCE_CONFLICT");
    assert_eq!(
        fs::read_to_string(&evidence_path).unwrap(),
        "人工修改的依据"
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    assert_eq!(
        app.store
            .personal_calls("personal_work", &day.to_string())
            .unwrap(),
        1
    );
}

#[test]
fn attachment_failure_prevents_publication_and_repair_uses_stored_bytes() {
    let (_dir, config) = setup();
    let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    let sent = Arc::new(AtomicUsize::new(0));
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(Model {
        sent: sent.clone(),
        invalid: false,
    });
    let month = app.config.paths.reports_dir.join("2026-10");
    fs::create_dir_all(&month).unwrap();
    let blocker = month.join("attachments");
    fs::write(&blocker, "occupied").unwrap();
    assert!(run_daily(request(day, true), &app).is_err());
    assert!(app.store.current(&day.to_string()).unwrap().is_none());
    assert!(!month.join("2026-10-19.md").exists());
    fs::remove_file(blocker).unwrap(); // isolated fixture, not a production attachment
    let repaired = ai_daily_report::run::repair_output(&app, &day.to_string(), Utc::now()).unwrap();
    assert!(repaired.output_path.unwrap().is_file());
    assert!(fs::read_dir(month.join("attachments"))
        .unwrap()
        .next()
        .is_some());
    assert_eq!(sent.load(Ordering::SeqCst), 1);
}

struct OversizedSummaryModel {
    sent: Arc<AtomicUsize>,
    item_count: usize,
    padding: usize,
    invalid_last_source: bool,
}

impl ModelTransport for OversizedSummaryModel {
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        let mut response = Model {
            sent: self.sent.clone(),
            invalid: false,
        }
        .send(request, timeout)?;
        if self.sent.load(Ordering::SeqCst) == 1 {
            let mut envelope: Value = serde_json::from_slice(&response.body).unwrap();
            let content = &mut envelope["choices"][0]["message"]["content"];
            let mut report: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
            let mut item = report["items"][0].clone();
            item["text"] = json!(format!(
                "{}{}",
                "字".repeat(self.padding),
                item["text"].as_str().unwrap()
            ));
            report["items"] = json!(vec![item; self.item_count]);
            if self.invalid_last_source {
                report["items"][self.item_count - 1]["source_ids"] = json!(["missing"]);
            }
            *content = json!(report.to_string());
            response.body = serde_json::to_vec(&envelope).unwrap();
        } else {
            let payload: Value = serde_json::from_slice(request).unwrap();
            let input: Value =
                serde_json::from_str(payload["messages"][1]["content"].as_str().unwrap()).unwrap();
            if let Some(draft) = input["draft_items"].as_array() {
                let mut envelope: Value = serde_json::from_slice(&response.body).unwrap();
                let mut items = draft.clone();
                for item in &mut items {
                    item["text"] =
                        json!("金额由 100 调整到 120，条件满足后生效；责任人与截止日期待确认。");
                }
                envelope["choices"][0]["message"]["content"] =
                    json!(json!({"schema_version":5,"items":items}).to_string());
                response.body = serde_json::to_vec(&envelope).unwrap();
            }
        }
        Ok(response)
    }
}

#[test]
fn oversized_summaries_compact_complete_items_and_validate_all_sources() {
    for (item_count, padding, invalid_last_source, calls, kept) in [
        (1, 1600, false, 2, 1), // Compact the only item instead of discarding it.
        (6, 0, false, 2, 1),    // Regenerate an invalid item count.
        (5, 300, false, 2, 5),  // Keep every item through compaction.
        (6, 0, true, 2, 1),     // Omission cannot hide an invalid source.
    ] {
        let (_dir, config) = setup();
        let sent = Arc::new(AtomicUsize::new(0));
        let mut app = App::open(config, Utc::now()).unwrap();
        app.transport = Box::new(OversizedSummaryModel {
            sent: sent.clone(),
            item_count,
            padding,
            invalid_last_source,
        });
        let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let output = run_daily(request(day, true), &app).unwrap();
        assert_eq!(sent.load(Ordering::SeqCst), calls);
        assert!(output.markdown.contains("条件满足后生效"));
        assert!(output.markdown.contains("待确认"));
        let run = app.store.get(&output.run_id).unwrap();
        let report: ai_daily_report::reports::personal::PersonalReport =
            serde_json::from_str(run.report_json.as_deref().unwrap()).unwrap();
        assert!(
            ai_daily_report::reports::personal::body_chars(&report.items)
                <= ai_daily_report::reports::personal::MAX_BODY_CHARS
        );
        assert!(report.items.len() <= 5);
        assert_eq!(report.items.len(), kept);
        assert!(report.items.iter().all(|item| item
            .text
            .ends_with("金额由 100 调整到 120，条件满足后生效；责任人与截止日期待确认。")));
        assert!(!report
            .gaps
            .iter()
            .any(|gap| gap.code == "PERSONAL_OUTPUT_REDUCED"));
        let call = &app.store.calls(&output.run_id).unwrap()[0];
        if item_count > 5 {
            assert_eq!(call["error_code"], "MODEL_RESPONSE_INVALID");
        } else {
            assert_eq!(call["status"], "succeeded");
        }
    }
}

fn summary_draft(chars: usize) -> Value {
    let mut items = json!([
        {"title":"核查结果","text":"已核查记录，金额由 100 调整到 120，条件满足后生效。","kind":"progress","statement_kind":"user_statement","source_ids":["user_input"]},
        {"title":"审批状态","text":"仍待审批，不能按已批准执行；责任人与截止日期待确认。","kind":"attention","statement_kind":"user_statement","source_ids":["user_input"]},
        {"title":"明细核对","text":"部分明细尚待核实，现有结果仅限已核查范围。","kind":"progress","statement_kind":"user_statement","source_ids":["user_input"]},
        {"title":"下一步：核实差异","text":"先核对未匹配记录，条件满足后再确认金额；负责人和期限待确认。","kind":"next_step","statement_kind":"suggestion","source_ids":["user_input"]}
    ]);
    let original: Vec<ai_daily_report::reports::personal::PersonalItem> =
        serde_json::from_value(items.clone()).unwrap();
    let padding = chars - ai_daily_report::reports::personal::body_chars(&original);
    items[0]["text"] = json!(format!(
        "{}{}",
        "字".repeat(padding),
        items[0]["text"].as_str().unwrap()
    ));
    json!({"schema_version":5,"items":items})
}

struct CompactionModel {
    requests: Arc<Mutex<Vec<Value>>>,
    draft: Value,
    behavior: &'static str,
}

impl ModelTransport for CompactionModel {
    fn send(&self, request: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        let payload: Value = serde_json::from_slice(request).unwrap();
        let input: Value =
            serde_json::from_str(payload["messages"][1]["content"].as_str().unwrap()).unwrap();
        self.requests.lock().unwrap().push(payload);
        let mut report = self.draft.clone();
        if input["draft_items"].is_array() {
            assert_eq!(input["max_chars"], 1500);
            let mut draft = input["draft_items"].clone();
            for (index, item) in draft.as_array_mut().unwrap().iter_mut().enumerate() {
                assert_eq!(
                    item.as_object_mut().unwrap().remove("item_id"),
                    Some(json!(index + 1))
                );
            }
            assert_eq!(draft, self.draft["items"]);
            report["items"] = input["draft_items"].clone();
            if self.behavior == "timeout" {
                return Err(TransportFailure::Timeout);
            }
            if self.behavior != "too_long" {
                report["items"][0]["text"] = json!("金额由 100 调整到 120，条件满足后生效。");
            }
            match self.behavior {
                "drop_plan" => {
                    report["items"].as_array_mut().unwrap().pop();
                }
                "change_kind" => report["items"][3]["kind"] = json!("progress"),
                "change_certainty" => {
                    report["items"][3]["statement_kind"] = json!("user_statement")
                }
                "drop_source" => report["items"][3]["source_ids"] = json!([]),
                "reorder" => report["items"].as_array_mut().unwrap().swap(0, 2),
                _ => {}
            }
        }
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: serde_json::to_vec(&json!({"choices":[{"finish_reason":"stop","message":{"content":report.to_string()}}],"usage":{"prompt_tokens":100,"completion_tokens":100}})).unwrap(),
        })
    }
}

#[test]
fn thousand_five_hundred_boundary_compacts_all_items_and_reuses_both_audited_calls() {
    for chars in [1500, 1501, 1504] {
        let (_dir, config) = setup();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::open(config, Utc::now()).unwrap();
        app.transport = Box::new(CompactionModel {
            requests: requests.clone(),
            draft: summary_draft(chars),
            behavior: "valid",
        });
        let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let outcome = run_daily(request(day, true), &app).unwrap();
        let run = app.store.get(&outcome.run_id).unwrap();
        let report: ai_daily_report::reports::personal::PersonalReport =
            serde_json::from_str(run.report_json.as_deref().unwrap()).unwrap();
        assert_eq!(report.items.len(), 4);
        assert_eq!(
            report.items[3].kind,
            ai_daily_report::reports::personal::ItemKind::NextStep
        );
        assert!(report.items[3].text.contains("条件满足后"));
        assert!(report.items[3].text.contains("待确认"));
        assert!(
            ai_daily_report::reports::personal::body_chars(&report.items)
                <= ai_daily_report::reports::personal::MAX_BODY_CHARS
        );
        assert!(report.gaps.is_empty());
        assert_eq!(report.quality, "complete");
        let calls = app.store.calls(&outcome.run_id).unwrap();
        assert_eq!(calls.len(), if chars == 1500 { 1 } else { 2 });
        assert!(calls.iter().all(|call| call["status"] == "succeeded"));
        if chars > 1500 {
            assert_eq!(calls[1]["logical_step"], "personal_summary.compact");
            assert_ne!(
                calls[0]["request_fingerprint"],
                calls[1]["request_fingerprint"]
            );
        }
        let snapshots = ai_daily_report::snapshots::SnapshotStore::new(
            app.config.paths.data_dir.join("daily_cache/snapshots"),
            1024 * 1024 * 1024,
        )
        .unwrap();
        let saved = ai_daily_report::snapshots::SnapshotRef {
            path: run.snapshot_path.unwrap(),
            sha256: run.snapshot_sha256.unwrap(),
        };
        let input: ai_daily_report::run::personal::PersonalInput =
            serde_json::from_value(snapshots.read(&saved).unwrap()).unwrap();
        let started = Instant::now();
        let runner = ai_daily_report::model::ModelRunner {
            store: &app.store,
            snapshots: &snapshots,
            transport: app.transport.as_ref(),
            llm: &app.config.llm,
            recovery_inputs: None,
            allow_new_calls: false,
            run_id: &outcome.run_id,
            started_at: Utc::now(),
            started,
            deadline: started,
        };
        let reused =
            ai_daily_report::model::personal::generate(&runner, input.plan.as_ref().unwrap())
                .unwrap();
        assert_eq!(reused.items.len(), 4);
        assert_eq!(reused.omitted, 0);
        assert_eq!(requests.lock().unwrap().len(), calls.len());
        assert_eq!(app.store.calls(&outcome.run_id).unwrap().len(), calls.len());
    }
}

#[test]
fn failed_compaction_preserves_next_steps_and_audits_bounded_fallback() {
    for behavior in [
        "too_long",
        "drop_plan",
        "change_kind",
        "change_certainty",
        "drop_source",
        "reorder",
        "timeout",
    ] {
        let (_dir, config) = setup();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::open(config, Utc::now()).unwrap();
        app.transport = Box::new(CompactionModel {
            requests: requests.clone(),
            draft: summary_draft(1504),
            behavior,
        });
        let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let outcome = run_daily(request(day, true), &app).unwrap();
        let run = app.store.get(&outcome.run_id).unwrap();
        let report: ai_daily_report::reports::personal::PersonalReport =
            serde_json::from_str(run.report_json.as_deref().unwrap()).unwrap();
        assert!(
            ai_daily_report::reports::personal::body_chars(&report.items)
                <= ai_daily_report::reports::personal::MAX_BODY_CHARS,
            "{behavior}"
        );
        assert_eq!(
            report.items.last().unwrap().kind,
            ai_daily_report::reports::personal::ItemKind::NextStep,
            "{behavior}"
        );
        assert_eq!(
            report.items.last().unwrap().text,
            summary_draft(1504)["items"][3]["text"].as_str().unwrap()
        );
        assert_eq!(report.quality, "partial");
        assert!(report
            .gaps
            .iter()
            .any(|g| g.code == "PERSONAL_OUTPUT_REDUCED"));
        let calls = app.store.calls(&outcome.run_id).unwrap();
        assert_eq!(
            calls.len(),
            if behavior == "timeout" { 2 } else { 3 },
            "{behavior}"
        );
        assert_eq!(calls[0]["status"], "succeeded");
        let error = if behavior == "timeout" {
            "MODEL_TIMEOUT"
        } else {
            "MODEL_RESPONSE_INVALID"
        };
        assert!(
            report.gaps.iter().any(|g| g.code == error),
            "{behavior}: {:?}",
            report.gaps
        );
        assert_eq!(requests.lock().unwrap().len(), calls.len());
    }
}

struct StaticDraftModel {
    sent: Arc<AtomicUsize>,
    draft: Value,
}

impl ModelTransport for StaticDraftModel {
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: serde_json::to_vec(&json!({
                "choices":[{"finish_reason":"stop","message":{"content":self.draft.to_string()}}],
                "usage":{"prompt_tokens":100,"completion_tokens":100}
            }))
            .unwrap(),
        })
    }
}

fn extended_draft(chars: usize) -> Value {
    let mut items = json!([
        {"title":"劳务用工差异","text":"劳务用工规模与参保人数严重不匹配，需核实。","kind":"attention","statement_kind":"user_statement","source_ids":["user_input"]},
        {"title":"供应商线索","text":"供应商董监高与单一来源采购线索需关注。","kind":"attention","statement_kind":"uncertain","source_ids":["user_input"]},
        {"title":"镜片报废筛查","text":"报废原因字眼筛查无命中，原表已保留。","kind":"progress","statement_kind":"user_statement","source_ids":["user_input"]},
        {"title":"信用报告取数","text":"信用报告批量取数完成，共 13 份。","kind":"progress","statement_kind":"user_statement","source_ids":["user_input"]},
        {"title":"下一步：索要明细","text":"索取决算明细与花名册，条件满足后再核对。","kind":"next_step","statement_kind":"suggestion","source_ids":["user_input"]}
    ]);
    let original: Vec<ai_daily_report::reports::personal::PersonalItem> =
        serde_json::from_value(items.clone()).unwrap();
    let padding = chars - ai_daily_report::reports::personal::body_chars(&original);
    items[0]["text"] = json!(format!(
        "{}{}",
        "字".repeat(padding),
        items[0]["text"].as_str().unwrap()
    ));
    json!({"schema_version":5,"items":items})
}

#[test]
fn complete_draft_within_extended_limit_publishes_every_item_without_compaction() {
    let (_dir, config) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(StaticDraftModel {
        sent: sent.clone(),
        draft: extended_draft(913),
    });
    let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    let outcome = run_daily(request(day, true), &app).unwrap();
    let run = app.store.get(&outcome.run_id).unwrap();
    let report: ai_daily_report::reports::personal::PersonalReport =
        serde_json::from_str(run.report_json.as_deref().unwrap()).unwrap();
    assert_eq!(report.items.len(), 5);
    assert_eq!(
        ai_daily_report::reports::personal::body_chars(&report.items),
        913
    );
    assert_eq!(
        report.items.last().unwrap().kind,
        ai_daily_report::reports::personal::ItemKind::NextStep
    );
    assert!(report.gaps.is_empty());
    assert_eq!(report.quality, "complete");
    let calls = app.store.calls(&outcome.run_id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["status"], "succeeded");
    assert_eq!(calls[0]["logical_step"], "personal_summary");
    assert_eq!(
        app.store
            .personal_calls("personal_work", &day.to_string())
            .unwrap(),
        1
    );
    assert_eq!(sent.load(Ordering::SeqCst), 1);
}

#[test]
fn oversized_next_steps_never_replace_an_existing_valid_report_with_an_empty_fallback() {
    let (_dir, config) = setup();
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(Model {
        sent: Arc::new(AtomicUsize::new(0)),
        invalid: false,
    });
    let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    let original = run_daily(request(day, true), &app).unwrap();
    let path = original.output_path.unwrap();
    let original_bytes = fs::read(&path).unwrap();
    let mut draft = summary_draft(1504);
    for item in draft["items"].as_array_mut().unwrap() {
        item["kind"] = json!("next_step");
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    app.transport = Box::new(CompactionModel {
        requests: requests.clone(),
        draft,
        behavior: "timeout",
    });
    let mut regenerate = request(day, true);
    regenerate.regeneration_reason = Some("verify protected plans".into());
    let error = run_daily(regenerate, &app).unwrap_err();
    assert_eq!(error.code, "PERSONAL_RECOVERY_INCOMPLETE");
    assert_eq!(
        app.store.current(&day.to_string()).unwrap().unwrap().run_id,
        original.run_id
    );
    assert_eq!(fs::read(&path).unwrap(), original_bytes);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[test]
fn no_save_preview_carries_evidence_without_publishing_files() {
    let (_dir, config) = setup();
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(Model {
        sent: Arc::new(AtomicUsize::new(0)),
        invalid: false,
    });
    let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    let preview = run_daily(request(day, false), &app).unwrap();
    assert!(preview.output_path.is_none());
    assert!(preview.markdown.contains("[查看依据](#item-1)"));
    assert!(preview.markdown.contains("id=\"item-1\""));
    assert!(preview.markdown.contains("## 用户补充原文"));
    assert!(!app.config.paths.reports_dir.join("2026-10").exists());
}

#[test]
fn repeated_previews_survive_reopen_and_compaction_without_date_limit() {
    let (_dir, config) = setup();
    let sent = Arc::new(AtomicUsize::new(0));
    let day = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
    for _ in 0..4 {
        let mut app = App::open(config.clone(), Utc::now()).unwrap();
        app.transport = Box::new(Model {
            sent: sent.clone(),
            invalid: false,
        });
        let output = run_daily(request(day, false), &app).unwrap();
        assert_eq!(output.model_calls, 1);
    }
    assert_eq!(sent.load(Ordering::SeqCst), 4);
    {
        let app = App::open(config.clone(), Utc::now() + chrono::Duration::days(31)).unwrap();
        assert_eq!(
            app.store
                .personal_calls("personal_work", &day.to_string())
                .unwrap(),
            4
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
    assert_eq!(sent.load(Ordering::SeqCst), 5);
}

#[test]
fn invalid_sources_never_publish_as_facts_and_requests_remain_bounded() {
    let (_dir, mut config) = setup();
    config.llm.invalid_response_retries = 1;
    let sent = Arc::new(AtomicUsize::new(0));
    for attempt in 1..=4 {
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
        assert_eq!(sent.load(Ordering::SeqCst), attempt * 2);
    }
    assert_eq!(sent.load(Ordering::SeqCst), 8);
}

#[test]
fn oversized_notes_and_database_source_fail_before_model_send() {
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
    input.period.source = ReportSource::Db;
    assert_eq!(
        run_daily(input, &app).unwrap_err().code,
        "PERSONAL_SOURCE_INVALID"
    );
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
            "PERSONAL_INPUT_EXPIRED",
            "saved input expired",
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
