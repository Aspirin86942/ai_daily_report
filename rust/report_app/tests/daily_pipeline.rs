use ai_daily_report::{
    config::AppConfig,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    run::{repair_output, run_daily, App},
    snapshots::digest,
    types::{Period, ReportMode, ReportRequest, ReportSource, RunContext, RunPhase},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, FileTimes},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-12T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Network {
    responses: Mutex<VecDeque<HttpResponse>>,
    sent: Mutex<usize>,
}
struct NetworkHandle(Arc<Network>);
impl ModelTransport for NetworkHandle {
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        *self.0.sent.lock().unwrap() += 1;
        Ok(self
            .0
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request"))
    }
}
fn response(value: Value) -> HttpResponse {
    HttpResponse{status:200,retry_after:None,body:serde_json::to_vec(&json!({"choices":[{"finish_reason":"stop","message":{"content":value.to_string()}}],"usage":{"prompt_tokens":100,"completion_tokens":200}})).unwrap()}
}
fn replies() -> Vec<HttpResponse> {
    let id = format!("src_{}", digest("采购方案.md".as_bytes()));
    let fact = json!({"topic":"采购审批调整","background":null,"detail":"含税金额不超过50000元且预算内由部门负责人审批；草稿待评审，生效日期尚未确定。","conditions":["超过50000元或预算外增加财务复核","关联交易不适用紧急通道"],"status":"草稿待评审","source_ids":[id],"uncertainty":null});
    let cited = json!({"text":"采购审批草稿待评审，生效日期尚未确定。含税金额不超过50000元且预算内由部门负责人审批；超过50000元或预算外增加财务复核，关联交易不适用紧急通道。","source_ids":[id],"fact_ids":["batch_000:000"]});
    vec![
        response(json!({"schema_version":3,"facts":[fact],"pending":[]})),
        response(
            json!({"schema_version":3,"overview":[cited],"matters":[{"title":"采购审批调整","background":null,"details":[cited],"status":null}],"pending":[]}),
        ),
    ]
}
struct Fixture {
    _dir: tempfile::TempDir,
    app: App,
    network: Arc<Network>,
}
impl Fixture {
    fn new(with_file: bool, replies: Vec<HttpResponse>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        fs::write(dir.path().join("config/settings.yaml"),"paths:\n  work_dir: work\nllm:\n  max_request_utf8_bytes: 64000\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\n  max_tokens: 4096\nrecovery:\n  enforce_model_budgets: false\n").unwrap();
        let mut config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        let workers = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
        config.office_worker_path =
            fs::canonicalize(workers.join("ai-daily-office-parser.exe")).unwrap();
        config.pdf_worker_path = fs::canonicalize(workers.join("ai-daily-pdf-parser.exe")).unwrap();
        if with_file {
            let path = config.paths.work_dir.join("采购方案.md");
            fs::write(&path,"含税金额不超过50000元且预算内由部门负责人审批；超过50000元或预算外增加财务复核。关联交易不适用紧急通道。草稿待评审，生效日期尚未确定。\n").unwrap();
            let stamp = DateTime::parse_from_rfc3339("2026-10-11T02:00:00Z")
                .unwrap()
                .timestamp() as u64;
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(stamp)))
                .unwrap();
        }
        let network = Arc::new(Network {
            responses: Mutex::new(replies.into()),
            sent: Mutex::new(0),
        });
        let mut app = App::open(config, now()).unwrap();
        app.transport = Box::new(NetworkHandle(network.clone()));
        Self {
            _dir: dir,
            app,
            network,
        }
    }
    fn request(&self, digit: char, save: bool) -> ReportRequest {
        let day = NaiveDate::from_ymd_opt(2026, 10, 11).unwrap();
        ReportRequest {
            period: Period {
                mode: ReportMode::Daily,
                source: ReportSource::Scan,
                start: day,
                end: day,
                label: day.to_string(),
            },
            context: RunContext::new(digit.to_string().repeat(32), now(), day, String::new())
                .unwrap(),
            input: String::new(),
            save,
            dry_run: false,
            scheduled: false,
            regeneration_reason: None,
        }
    }
    fn calls(&self) -> usize {
        *self.network.sent.lock().unwrap()
    }
}

#[test]
fn native_scan_to_new_daily_then_repeat_and_repair_without_new_calls() {
    let f = Fixture::new(true, replies());
    let output = run_daily(f.request('a', true), &f.app).unwrap();
    assert_eq!(output.action, "published");
    assert_eq!(f.calls(), 2);
    assert!(output.markdown.contains("生效日期尚未确定"));
    let run = f.app.store.get(&output.run_id).unwrap();
    let report: Value = serde_json::from_str(run.report_json.as_deref().unwrap()).unwrap();
    assert_eq!(report["schema_version"], 3);
    assert!(report["sources"][0]["model_chars"].as_u64().unwrap() > 0);
    assert!(report["sources"][0]["coverage_limits"]["evidence_audit"]["model_ranges"].is_array());
    let second = run_daily(f.request('b', true), &f.app).unwrap();
    assert_eq!(second.action, "reused");
    assert_eq!(f.calls(), 2);
    assert_eq!(second.markdown, output.markdown);
    fs::remove_file(output.output_path.unwrap()).unwrap();
    let repaired = repair_output(&f.app, "2026-10-11", now()).unwrap();
    assert_eq!(repaired.model_calls, 0);
    assert_eq!(repaired.markdown, output.markdown);
    assert_eq!(f.calls(), 2);
}

#[test]
fn preview_is_audited_without_publishing_and_dry_run_never_requests_model() {
    let f = Fixture::new(true, replies());
    let preview = run_daily(f.request('a', false), &f.app).unwrap();
    assert_eq!(preview.action, "preview");
    assert!(preview.output_path.is_none());
    assert!(f.app.store.current("2026-10-11").unwrap().is_none());
    assert_eq!(f.calls(), 2);
    let mut request = f.request('b', true);
    request.dry_run = true;
    let dry = run_daily(request, &f.app).unwrap();
    assert_eq!(dry.action, "dry_run");
    assert_eq!(f.calls(), 2);
    assert!(f.app.store.calls(&dry.run_id).unwrap().is_empty());
}

#[test]
fn empty_day_needs_neither_credentials_nor_model_capacity_confirmation() {
    let mut f = Fixture::new(false, vec![]);
    f.app.config.llm.context_window_tokens = None;
    f.app.config.llm.verified_max_output_tokens = None;
    let output = run_daily(f.request('a', true), &f.app).unwrap();
    assert_eq!(output.quality, "no_changes");
    assert_eq!(f.calls(), 0);
    assert!(output.markdown.contains("未发现修改的文件"));
}

#[test]
fn failed_composition_recovers_frozen_inputs_without_rescanning_changed_file() {
    let mut responses = replies();
    let compose = responses.pop().unwrap();
    responses.push(HttpResponse {
        status: 400,
        retry_after: None,
        body: b"{}".to_vec(),
    });
    responses.push(compose);
    let f = Fixture::new(true, responses);
    assert_eq!(
        run_daily(f.request('a', true), &f.app).unwrap_err().code,
        "MODEL_HTTP_ERROR"
    );
    fs::write(
        f.app.config.paths.work_dir.join("采购方案.md"),
        "此修改不应进入先前冻结的日报。",
    )
    .unwrap();
    let recovered = run_daily(f.request('b', true), &f.app).unwrap();
    assert_eq!(recovered.run_id, "a".repeat(32));
    assert_eq!(f.calls(), 3);
    assert!(!recovered.markdown.contains("此修改"));
    assert_eq!(
        f.app.store.get(&recovered.run_id).unwrap().phase,
        RunPhase::Published
    );
}
