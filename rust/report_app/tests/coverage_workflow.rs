use ai_daily_report::{
    config::AppConfig,
    maintenance::coverage::{self, PlanRequest},
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    publish::Publisher,
    run::App,
    snapshots::{digest, fingerprint},
    store::StagedReport,
    types::RunContext,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, FileTimes},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Network {
    replies: Mutex<VecDeque<HttpResponse>>,
    sent: Mutex<usize>,
}
struct Handle(Arc<Network>);
impl ModelTransport for Handle {
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        *self.0.sent.lock().unwrap() += 1;
        Ok(self
            .0
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected call"))
    }
}
fn response(content: Value) -> HttpResponse {
    HttpResponse{status:200,retry_after:None,body:json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}],"usage":{"total_tokens":100}}).to_string().into_bytes()}
}
fn replies() -> Vec<HttpResponse> {
    let source = format!("src_{}", digest(b"scope/approval.md"));
    let fact = json!({"topic":"采购审批","background":null,"detail":"超过 5 万元增加财务复核。","conditions":["预算外也增加复核"],"status":"待评审","source_ids":[source],"uncertainty":null});
    let cited = json!({"text":"采购审批方案待评审；超过 5 万元或预算外增加财务复核。","fact_ids":["batch_000:000"],"source_ids":[source]});
    vec![
        response(json!({"schema_version":3,"facts":[fact],"pending":[]})),
        response(
            json!({"schema_version":3,"overview":[cited],"matters":[{"title":"采购审批","background":null,"details":[cited],"status":null}],"pending":[]}),
        ),
    ]
}
struct Fixture {
    _dir: tempfile::TempDir,
    app: App,
    network: Arc<Network>,
    date: chrono::NaiveDate,
    old_id: String,
    body_path: PathBuf,
}
impl Fixture {
    fn new(replies: Vec<HttpResponse>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::create_dir_all(dir.path().join("work/scope")).unwrap();
        fs::write(dir.path().join("config/settings.yaml"),"paths:\n  work_dir: work\nllm:\n  max_request_utf8_bytes: 64000\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\n  max_tokens: 4096\n").unwrap();
        let mut config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        let workers = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
        config.office_worker_path =
            fs::canonicalize(workers.join("ai-daily-office-parser.exe")).unwrap();
        config.pdf_worker_path = fs::canonicalize(workers.join("ai-daily-pdf-parser.exe")).unwrap();
        let file = config.paths.work_dir.join("scope/approval.md");
        fs::write(&file, "超过 5 万元或预算外增加财务复核。方案待评审。\n").unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(now().timestamp() as u64)),
            )
            .unwrap();
        let mut app = App::open(config, now()).unwrap();
        let network = Arc::new(Network {
            replies: Mutex::new(replies.into()),
            sent: Mutex::new(0),
        });
        app.transport = Box::new(Handle(network.clone()));
        let date = ai_daily_report::types::parse_date("2026-10-11").unwrap();
        let old_id = "a".repeat(32);
        let context =
            RunContext::new(old_id.clone(), now(), date, app.fingerprint(false).unwrap()).unwrap();
        app.store
            .create(&context, "report", &old_id, None, now())
            .unwrap();
        let snapshot = app
            .snapshots
            .put(&old_id, "input", &json!({"fixture":true}))
            .unwrap();
        app.store
            .prepare(&old_id, &snapshot, &snapshot.sha256, &[], now())
            .unwrap();
        let report = json!({"schema_version":3,"date":date.to_string(),"window_start_utc":context.window_start_utc.to_rfc3339(),"quality":"partial","sources":[{"source_id":format!("src_{}",digest(b"scope/approval.md")),"relative_path":"scope/approval.md","size_bytes":file.metadata().unwrap().len(),"content_sha256":digest(&fs::read(file).unwrap())}],"gaps":[{"code":"MODEL_INPUT_BOUNDED"}]});
        let publisher = Publisher::new(&app.config.paths.reports_dir, &app.store).unwrap();
        let body_path = publisher.output_path(&date.to_string()).unwrap();
        app.store
            .stage_report(
                &old_id,
                &StagedReport {
                    report: &report,
                    markdown: "原日报正文\n",
                    output_path: &body_path.to_string_lossy(),
                    publication_fingerprint: &"d".repeat(64),
                },
                now(),
            )
            .unwrap();
        publisher.publish(&old_id, now()).unwrap();
        Self {
            _dir: dir,
            app,
            network,
            date,
            old_id,
            body_path,
        }
    }
    fn plan(&self, refresh: bool) -> PathBuf {
        coverage::make_plan(
            &self.app,
            PlanRequest {
                dates: &[self.date],
                gap_codes: &["MODEL_INPUT_BOUNDED".into()],
                scope_root: &self.app.config.paths.work_dir.join("scope"),
                refresh,
            },
            now(),
        )
        .unwrap()
    }
    fn sent(&self) -> usize {
        *self.network.sent.lock().unwrap()
    }
}
#[test]
fn candidate_generation_and_publication_are_separate_and_idempotent() {
    let f = Fixture::new(replies());
    let plan = f.plan(true);
    assert_eq!(f.sent(), 0);
    let manifest = coverage::generate(&f.app, &plan, now()).unwrap();
    assert_eq!(f.sent(), 2);
    assert_eq!(fs::read_to_string(&f.body_path).unwrap(), "原日报正文\n");
    assert_eq!(coverage::generate(&f.app, &plan, now()).unwrap(), manifest);
    assert_eq!(f.sent(), 2);
    let first = coverage::publish(&f.app, &manifest, now()).unwrap();
    let second = coverage::publish(&f.app, &manifest, now()).unwrap();
    assert_eq!(first, second);
    assert_eq!(f.sent(), 2);
    assert!(fs::read_to_string(&f.body_path).unwrap().contains("待评审"));
    assert_ne!(
        f.app
            .store
            .current(&f.date.to_string())
            .unwrap()
            .unwrap()
            .run_id,
        f.old_id
    );
}
#[test]
fn source_changes_and_modified_candidates_are_rejected_without_extra_calls() {
    let f = Fixture::new(replies());
    let plan = f.plan(true);
    fs::write(
        f.app.config.paths.work_dir.join("scope/approval.md"),
        "changed",
    )
    .unwrap();
    assert_eq!(
        coverage::generate(&f.app, &plan, now()).unwrap_err().code,
        "COVERAGE_SOURCE_CHANGED"
    );
    assert_eq!(f.sent(), 0);
    let f = Fixture::new(replies());
    let plan = f.plan(true);
    let manifest = coverage::generate(&f.app, &plan, now()).unwrap();
    fs::write(
        manifest.parent().unwrap().join("candidates/2026-10-11.md"),
        "人工修改候选",
    )
    .unwrap();
    assert_eq!(
        coverage::publish(&f.app, &manifest, now())
            .unwrap_err()
            .code,
        "COVERAGE_CANDIDATE_CHANGED"
    );
    assert_eq!(f.sent(), 2);
    assert_eq!(fs::read_to_string(&f.body_path).unwrap(), "原日报正文\n");
}
#[test]
fn partial_recovery_reuses_success_and_retains_original_job_consumption() {
    let mut outputs = replies();
    let compose = outputs.pop().unwrap();
    outputs.push(HttpResponse {
        status: 400,
        retry_after: None,
        body: b"{}".to_vec(),
    });
    outputs.push(compose);
    let f = Fixture::new(outputs);
    let plan = f.plan(true);
    assert_eq!(
        coverage::generate(&f.app, &plan, now()).unwrap_err().code,
        "MODEL_HTTP_ERROR"
    );
    assert_eq!(f.sent(), 2);
    let recovery = coverage::recover(
        &f.app,
        &plan,
        "恢复成稿步骤",
        "使用已核对材料恢复",
        false,
        now(),
    )
    .unwrap();
    assert_eq!(f.sent(), 2);
    assert_eq!(
        coverage::recover(
            &f.app,
            &plan,
            "恢复成稿步骤",
            "使用已核对材料恢复",
            false,
            now()
        )
        .unwrap(),
        recovery
    );
    let manifest = coverage::generate(&f.app, &plan, now()).unwrap();
    assert_eq!(f.sent(), 3);
    coverage::publish(&f.app, &manifest, now()).unwrap();
    let value: Value = serde_json::from_slice(&fs::read(plan).unwrap()).unwrap();
    let calls = f
        .app
        .store
        .job_calls(value["plan"]["job_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1]["status"], "failed");
}
#[test]
fn explicit_failed_only_amendment_keeps_old_call_records() {
    let mut outputs = vec![HttpResponse {
        status: 400,
        retry_after: None,
        body: b"{}".to_vec(),
    }];
    outputs.extend(replies());
    let mut f = Fixture::new(outputs);
    let plan = f.plan(true);
    assert!(coverage::generate(&f.app, &plan, now()).is_err());
    f.app.config.llm.thinking_mode = Some("disabled".into());
    f.app.config.llm.thinking_parameter_location = Some("extra_body".into());
    coverage::amend(&f.app, &plan, "明确修订模型参数位置", now()).unwrap();
    assert_eq!(f.sent(), 1);
    coverage::generate(&f.app, &plan, now()).unwrap();
    assert_eq!(f.sent(), 3);
    let value: Value = serde_json::from_slice(&fs::read(plan).unwrap()).unwrap();
    assert_eq!(
        f.app
            .store
            .job_calls(value["plan"]["job_id"].as_str().unwrap())
            .unwrap()
            .len(),
        3
    );
}
#[test]
fn plan_without_refresh_never_sends_and_external_edits_preserve_formal_body() {
    let f = Fixture::new(replies());
    let plan = f.plan(false);
    assert_eq!(
        coverage::generate(&f.app, &plan, now()).unwrap_err().code,
        "COVERAGE_PREFLIGHT_REQUIRED"
    );
    assert_eq!(f.sent(), 0);
    let f = Fixture::new(replies());
    let plan = f.plan(true);
    let manifest = coverage::generate(&f.app, &plan, now()).unwrap();
    fs::write(&f.body_path, "人工修改正式日报\n").unwrap();
    assert_eq!(
        coverage::publish(&f.app, &manifest, now())
            .unwrap_err()
            .code,
        "COVERAGE_CURRENT_CHANGED"
    );
    assert_eq!(
        fs::read_to_string(&f.body_path).unwrap(),
        "人工修改正式日报\n"
    );
}
#[test]
fn changing_hashed_plan_is_detected_before_generation() {
    let f = Fixture::new(replies());
    let plan = f.plan(true);
    let mut value: Value = serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();
    value["plan"]["calls_limit"] = json!(100);
    fs::write(&plan, value.to_string()).unwrap();
    assert_eq!(
        coverage::generate(&f.app, &plan, now()).unwrap_err().code,
        "MAINTENANCE_ARTIFACT_CHANGED"
    );
    assert_eq!(f.sent(), 0);
    assert_ne!(fingerprint(&value["plan"]).unwrap(), value["sha256"]);
}

#[test]
fn repeated_recovery_keeps_successes_and_failure_records_without_quota_changes() {
    let mut outputs = replies();
    let compose = outputs.pop().unwrap();
    outputs.extend([
        HttpResponse {
            status: 400,
            retry_after: None,
            body: b"{}".to_vec(),
        },
        HttpResponse {
            status: 400,
            retry_after: None,
            body: b"{}".to_vec(),
        },
        compose,
    ]);
    let f = Fixture::new(outputs);
    let plan = f.plan(true);
    assert!(coverage::generate(&f.app, &plan, now()).is_err());
    let first = coverage::recover(&f.app, &plan, "恢复", "保留已成功请求", false, now()).unwrap();
    let saved = fs::read(&first).unwrap();
    assert!(coverage::generate(&f.app, &plan, now()).is_err());
    assert_eq!(f.sent(), 3);
    let second =
        coverage::recover(&f.app, &plan, "再次恢复", "继续保留已有请求", false, now()).unwrap();
    assert_ne!(first, second);
    assert_eq!(fs::read(first).unwrap(), saved);
    coverage::generate(&f.app, &plan, now()).unwrap();
    assert_eq!(f.sent(), 4);
    let value: Value = serde_json::from_slice(&fs::read(plan).unwrap()).unwrap();
    let job = value["plan"]["job_id"].as_str().unwrap();
    assert_eq!(f.app.store.job_calls(job).unwrap().len(), 4);
    assert!(calls_have_no_quota(&f.app.store.job_calls(job).unwrap()));
}

fn calls_have_no_quota(calls: &[Value]) -> bool {
    calls.iter().all(|call| {
        serde_json::from_str::<Value>(call["usage_json"].as_str().unwrap())
            .unwrap()
            .get("reserved_budget")
            .is_none()
    })
}
