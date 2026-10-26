use ai_daily_report::{
    config::AppConfig,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    run::{repair_run, run_report, App},
    types::{Period, ReportRequest, ReportSource, RunContext},
    ReportError,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-12T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Network {
    responses: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<Value>>,
}
struct NetworkHandle(Arc<Network>);
impl ModelTransport for NetworkHandle {
    fn send(&self, body: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        self.0
            .requests
            .lock()
            .unwrap()
            .push(serde_json::from_slice(body).unwrap());
        Ok(self
            .0
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected call"))
    }
}
fn response(value: Value) -> HttpResponse {
    HttpResponse {
        status: 200,
        retry_after: None,
        body: serde_json::to_vec(
            &json!({"choices":[{"finish_reason":"stop","message":{"content":value.to_string()}}]}),
        )
        .unwrap(),
    }
}
fn weekly() -> Value {
    serde_json::from_str(include_str!("fixtures/weekly-v1.json")).unwrap()
}
fn monthly() -> Value {
    serde_json::from_str(include_str!("fixtures/monthly-v1.json")).unwrap()
}
fn weekly_w42() -> Value {
    let mut value = weekly();
    value["week_label"] = json!("2026-W42");
    value["date_range"] = json!("2026-10-12 ~ 2026-10-18");
    value
}
struct Fixture {
    _dir: tempfile::TempDir,
    app: App,
    network: Arc<Network>,
}
impl Fixture {
    fn new(replies: Vec<HttpResponse>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::write(dir.path().join("config/settings.yaml"),"paths:\n  work_dir: nonexistent\nllm:\n  max_request_utf8_bytes: 64000\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\n  max_tokens: 4096\n").unwrap();
        let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        let mut app = App::open(config, now()).unwrap();
        let network = Arc::new(Network {
            responses: Mutex::new(replies.into()),
            requests: Mutex::new(vec![]),
        });
        app.transport = Box::new(NetworkHandle(network.clone()));
        let daily = json!({"date":"2026-10-11","completed_work":"方案草稿待评审。","work_summary":"预算外增加复核。","next_plan":"现有材料未提供。"});
        app.store.connection().unwrap().execute("INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json) VALUES ('2026-10-11','','','',?)",[daily.to_string()]).unwrap();
        Self {
            _dir: dir,
            app,
            network,
        }
    }
    fn with_published_v4_daily(self) -> Self {
        let report: Value =
            serde_json::from_str(include_str!("fixtures/readability/v4-input.json")).unwrap();
        self.app
            .store
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,schema_version,report_kind) VALUES ('2026-10-17','','','',?,4,'file_evidence')",
                [report.to_string()],
            )
            .unwrap();
        self.app
            .store
            .activate(NaiveDate::from_ymd_opt(2026, 10, 12).unwrap())
            .unwrap();
        self
    }
    fn request(&self, digit: char, month: bool, save: bool) -> ReportRequest {
        self.request_for(
            digit,
            if month { "2026-10" } else { "2026-W41" },
            month,
            save,
        )
    }
    fn request_for(&self, digit: char, label: &str, month: bool, save: bool) -> ReportRequest {
        let period = if month {
            Period::monthly(now().date_naive(), Some(label), ReportSource::Db)
        } else {
            Period::weekly(now().date_naive(), Some(label), ReportSource::Db)
        }
        .unwrap();
        ReportRequest {
            context: RunContext::new(
                digit.to_string().repeat(32),
                now(),
                period.start,
                String::new(),
            )
            .unwrap(),
            period,
            input: String::new(),
            save,
            dry_run: false,
            scheduled: false,
            regeneration_reason: None,
        }
    }
    fn calls(&self) -> usize {
        self.network.requests.lock().unwrap().len()
    }
}

#[test]
fn database_week_and_month_publish_without_scanner_and_repeat_reuses() {
    let f = Fixture::new(vec![response(weekly()), response(monthly())]);
    let week = run_report(f.request('a', false, true), &f.app).unwrap();
    let month = run_report(f.request('b', true, true), &f.app).unwrap();
    assert_eq!(week.action, "published");
    assert_eq!(month.action, "published");
    assert_eq!(f.calls(), 2);
    assert!(week
        .output_path
        .as_ref()
        .unwrap()
        .ends_with(Path::new("weekly/2026-W41.md")));
    assert!(month
        .output_path
        .as_ref()
        .unwrap()
        .ends_with(Path::new("monthly/2026-10.md")));
    assert!(!f.app.config.index_db_path.exists());
    assert!(week.markdown.contains("采购审批方案草稿"));
    assert_eq!(
        run_report(f.request('c', false, true), &f.app)
            .unwrap()
            .action,
        "reused"
    );
    assert_eq!(f.calls(), 2);
    let text = f.network.requests.lock().unwrap()[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("草稿待评审"));
    assert!(text.contains("2026-10-06"));
    fs::remove_file(week.output_path.unwrap()).unwrap();
    assert_eq!(
        repair_run(&f.app, &week.run_id, now()).unwrap().markdown,
        week.markdown
    );
    assert_eq!(f.calls(), 2);
}

#[test]
fn personal_items_and_statement_labels_reach_both_period_prompts() {
    use ai_daily_report::reports::{
        data::ReportSource,
        personal::{ItemKind, Material, PersonalItem, PersonalReport, Selection, StatementKind},
    };
    let f = Fixture::new(vec![response(weekly()), response(monthly())]);
    let text = "报价由 100 调整到 120，仍待审批。";
    let mut selection = Selection::default();
    selection.summary.selected_chars = text.chars().count();
    selection.materials.push(Material {
        source: ReportSource::user(text, now()),
        text: text.into(),
        evidence: vec![],
        priority: 0,
    });
    let context = RunContext::new(
        "a".repeat(32),
        now(),
        NaiveDate::from_ymd_opt(2026, 10, 11).unwrap(),
        String::new(),
    )
    .unwrap();
    let mut report = PersonalReport::from_selection(&context, "work", &selection);
    report.items = vec![
        PersonalItem {
            title: "报价调整".into(),
            text: text.into(),
            kind: ItemKind::Attention,
            statement_kind: StatementKind::UserStatement,
            source_ids: vec!["user_input".into()],
        },
        PersonalItem {
            title: "跟进建议".into(),
            text: "建议确认责任人。".into(),
            kind: ItemKind::NextStep,
            statement_kind: StatementKind::Suggestion,
            source_ids: vec!["user_input".into()],
        },
    ];
    report.validate().unwrap();
    f.app.store.connection().unwrap().execute("UPDATE daily_reports SET raw_json=?,schema_version=5,report_kind='personal_daily' WHERE date='2026-10-11'",
        [serde_json::to_string(&report).unwrap()]).unwrap();
    run_report(f.request('b', false, false), &f.app).unwrap();
    run_report(f.request('c', true, false), &f.app).unwrap();
    for request in f.network.requests.lock().unwrap().iter() {
        let text = request["messages"][1]["content"].as_str().unwrap();
        assert!(
            text.contains("仍待审批")
                && text.contains("user_statement")
                && text.contains("suggestion"),
            "{text}"
        );
    }
}

#[test]
fn period_reuse_keeps_publication_identity_and_preserves_external_edits() {
    for month in [false, true] {
        let f = Fixture::new(vec![response(if month { monthly() } else { weekly() })]);
        let published = run_report(f.request('a', month, true), &f.app).unwrap();
        let reused = run_report(f.request('b', month, true), &f.app).unwrap();
        assert_eq!(reused.run_id, "b".repeat(32));
        assert_eq!(reused.action, "reused");
        assert_eq!(reused.markdown, published.markdown);
        assert_eq!(reused.output_path, published.output_path);
        assert_eq!(reused.model_calls, 0);
        let observation = f.app.store.get(&reused.run_id).unwrap();
        assert!(matches!(
            observation.status,
            ai_daily_report::store::RunStatus::Succeeded
        ));
        assert_eq!(
            observation.phase,
            ai_daily_report::types::RunPhase::Prepared
        );
        assert_eq!(
            observation.previous_run_id.as_deref(),
            Some(published.run_id.as_str())
        );

        let path = published.output_path.as_ref().unwrap();
        fs::write(path, "用户修改").unwrap();
        let request = f.request('c', month, true);
        let label = request.period.label.clone();
        assert_eq!(
            run_report(request, &f.app).unwrap_err().code,
            "OUTPUT_CONFLICT"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "用户修改");
        let failed = f.app.store.get(&"c".repeat(32)).unwrap();
        assert!(matches!(
            failed.status,
            ai_daily_report::store::RunStatus::Failed
        ));
        assert_eq!(failed.error_code.as_deref(), Some("OUTPUT_CONFLICT"));
        assert_eq!(
            f.app
                .store
                .current_period(if month { "monthly" } else { "weekly" }, &label)
                .unwrap()
                .unwrap()
                .run_id,
            published.run_id
        );
        assert_eq!(f.calls(), 1);
    }
}

#[test]
fn period_preview_does_not_update_formal_table() {
    let f = Fixture::new(vec![response(monthly())]);
    let report = run_report(f.request('a', true, false), &f.app).unwrap();
    assert_eq!(report.action, "preview");
    assert!(report.output_path.is_none());
    assert!(f
        .app
        .store
        .current_period("monthly", "2026-10")
        .unwrap()
        .is_none());
    assert_eq!(
        f.app
            .store
            .connection()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM monthly_reports", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn period_failed_call_resumes_original_snapshot_and_publication_conflicts_are_recoverable() {
    let f = Fixture::new(vec![
        HttpResponse {
            status: 400,
            retry_after: None,
            body: b"{}".to_vec(),
        },
        response(weekly()),
    ]);
    assert_eq!(
        run_report(f.request('a', false, true), &f.app)
            .unwrap_err()
            .code,
        "MODEL_HTTP_ERROR"
    );
    f.app
        .store
        .connection()
        .unwrap()
        .execute("DELETE FROM daily_reports", [])
        .unwrap();
    let report = run_report(f.request('b', false, true), &f.app).unwrap();
    assert_eq!(report.run_id, "a".repeat(32));
    assert_eq!(f.calls(), 2);
    let requests = f.network.requests.lock().unwrap();
    assert_eq!(requests[0], requests[1]);
    drop(requests);
    fs::write(report.output_path.unwrap(), "用户修改").unwrap();
    assert_eq!(
        repair_run(&f.app, &report.run_id, now()).unwrap_err().code,
        "OUTPUT_CONFLICT"
    );
    assert_eq!(f.calls(), 2);
}

#[test]
fn unknown_period_schema_and_changed_period_labels_fail_validation() {
    let mut changed = weekly();
    changed["week_label"] = json!("2026-W42");
    let f = Fixture::new(vec![response(changed)]);
    let error: ReportError = run_report(f.request('a', false, true), &f.app).unwrap_err();
    assert_eq!(error.code, "MODEL_RESPONSE_INVALID");
    assert_eq!(f.calls(), 1);
}

#[test]
fn published_v4_daily_content_reaches_weekly_reports_summary() {
    let f = Fixture::new(vec![response(weekly_w42())]).with_published_v4_daily();
    let report = run_report(f.request_for('a', "2026-W42", false, true), &f.app).unwrap();
    assert_eq!(report.action, "published");
    assert!(report
        .output_path
        .as_ref()
        .unwrap()
        .ends_with(Path::new("weekly/2026-W42.md")));
    let requests = f.network.requests.lock().unwrap();
    let prompt = requests[0]["messages"][1]["content"].as_str().unwrap();
    for text in [
        "采购审批方案(新)集中说明现状与建议",
        "采购审批方案(新)调整",
        "[集团]例外复核归并",
        "#制度附件与空字段口径",
        "src_22f3bf94abdc4fb1933025dc0b6bca6d62f8558298bf6a316d48c1eff41bd66e",
        "采购审批调整.txt",
        "制度附件.pdf",
        "材料存在覆盖缺口，相关附件内容待补齐",
        "制度附件读取失败，暂不纳入结论",
        "生效日期尚未确认",
        "2026-10-12、2026-10-13、2026-10-14、2026-10-15、2026-10-16、2026-10-18",
    ] {
        assert!(prompt.contains(text), "weekly prompt missing {text}");
    }
}

#[test]
fn v4_and_legacy_daily_content_reach_monthly_preview_summary() {
    let f = Fixture::new(vec![response(monthly())]).with_published_v4_daily();
    let report = run_report(f.request_for('b', "2026-10", true, false), &f.app).unwrap();
    assert_eq!(report.action, "preview");
    assert!(report.output_path.is_none());
    assert_eq!(
        f.app
            .store
            .connection()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM monthly_reports", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let requests = f.network.requests.lock().unwrap();
    let prompt = requests[0]["messages"][1]["content"].as_str().unwrap();
    for text in [
        "方案草稿待评审",
        "预算外增加复核",
        "采购审批方案(新)集中说明现状与建议",
        "#制度附件与空字段口径",
        "制度附件读取失败，暂不纳入结论",
        "2026-10-12、2026-10-13",
    ] {
        assert!(prompt.contains(text), "monthly prompt missing {text}");
    }
}

#[test]
fn scan_week_and_month_use_native_source_text() {
    let mut f = Fixture::new(vec![response(weekly()), response(monthly())]);
    let workers = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
    f.app.config.office_worker_path =
        fs::canonicalize(workers.join("ai-daily-office-parser.exe")).unwrap();
    f.app.config.pdf_worker_path =
        fs::canonicalize(workers.join("ai-daily-pdf-parser.exe")).unwrap();
    fs::create_dir_all(&f.app.config.paths.work_dir).unwrap();
    let file = f.app.config.paths.work_dir.join("approval.md");
    fs::write(&file, "预算外采购增加复核，方案尚待评审。\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH
                    + Duration::from_secs(
                        DateTime::parse_from_rfc3339("2026-10-11T02:00:00Z")
                            .unwrap()
                            .timestamp() as u64,
                    ),
            ),
        )
        .unwrap();
    for (digit, month) in [('a', false), ('b', true)] {
        let mut request = f.request(digit, month, false);
        request.period.source = ReportSource::Scan;
        let outcome = run_report(request, &f.app).unwrap();
        assert_eq!(outcome.action, "preview");
    }
    assert_eq!(f.calls(), 2);
    assert!(f.app.config.index_db_path.exists());
    for request in f.network.requests.lock().unwrap().iter() {
        assert!(request["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("预算外采购增加复核"));
    }
}
