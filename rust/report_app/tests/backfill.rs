use ai_daily_report::{
    cli::{BackfillArgs, Cli, Command},
    config::AppConfig,
    maintenance::backfill,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    run::App,
    snapshots::digest,
};
use chrono::{DateTime, Utc};
use clap::Parser;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, FileTimes},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Network {
    sent: Mutex<usize>,
    replies: Mutex<VecDeque<Value>>,
}
struct Handle(Arc<Network>);
impl ModelTransport for Handle {
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        *self.0.sent.lock().unwrap() += 1;
        let content = self
            .0
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call");
        Ok(HttpResponse{status:200,retry_after:None,body:json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}],"usage":{"total_tokens":90}}).to_string().into_bytes()})
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    app: App,
    network: Arc<Network>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        fs::write(dir.path().join("config/settings.yaml"),"paths:\n  work_dir: work\nllm:\n  max_request_utf8_bytes: 64000\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\n  max_tokens: 4096\nrecovery:\n  enforce_model_budgets: false\n").unwrap();
        let mut config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        let workers = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug");
        config.office_worker_path =
            fs::canonicalize(workers.join("ai-daily-office-parser.exe")).unwrap();
        config.pdf_worker_path = fs::canonicalize(workers.join("ai-daily-pdf-parser.exe")).unwrap();
        let file = config.paths.work_dir.join("approval.md");
        fs::write(&file, "采购审批方案待评审，超过5万元增加财务复核。").unwrap();
        File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1791684000)))
            .unwrap();
        let source = format!("src_{}", digest(b"approval.md"));
        let fact = json!({"topic":"采购审批","background":null,"detail":"超过5万元增加财务复核。","conditions":[],"status":"待评审","source_ids":[source],"uncertainty":null});
        let cited = json!({"text":"采购审批方案待评审，超过5万元增加财务复核。","fact_ids":["batch_000:000"],"source_ids":[source]});
        let network=Arc::new(Network{sent:Mutex::new(0),replies:Mutex::new(vec![json!({"schema_version":3,"facts":[fact],"pending":[]}),json!({"schema_version":3,"overview":[cited],"matters":[{"title":"采购审批","background":null,"details":[cited],"status":null}],"pending":[]})].into())});
        let mut app = App::open(config, now()).unwrap();
        app.transport = Box::new(Handle(network.clone()));
        Self {
            _dir: dir,
            app,
            network,
        }
    }
    fn args(&self, cap: u64) -> BackfillArgs {
        let start = ai_daily_report::types::parse_date("2026-10-10").unwrap();
        let end = ai_daily_report::types::parse_date("2026-10-11").unwrap();
        let path = backfill::preflight(&self.app, start, end, now()).unwrap();
        let cli = Cli::try_parse_from([
            "report",
            "backfill",
            "--start-date",
            "2026-10-10",
            "--end-date",
            "2026-10-11",
            "--preflight",
            path.to_str().unwrap(),
            "--call-budget",
            &cap.to_string(),
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Backfill(args) => args,
            _ => unreachable!(),
        }
    }
    fn sent(&self) -> usize {
        *self.network.sent.lock().unwrap()
    }
}
#[test]
fn preflight_backfill_repeat_and_consistent_backup_preserve_formal_reports() {
    let f = Fixture::new();
    let args = f.args(2);
    assert_eq!(f.sent(), 0);
    let first = backfill::run(&f.app, &args, now()).unwrap();
    assert_eq!(first["complete"], true);
    assert_eq!(f.sent(), 2);
    assert_eq!(first["calls_used"], 2);
    let again = backfill::run(&f.app, &args, now()).unwrap();
    assert_eq!(again["complete"], true);
    assert_eq!(f.sent(), 2);
    let backup = f
        .app
        .config
        .paths
        .data_dir
        .join("backfill/2026-10-10_2026-10-11/reports-before-backfill.sqlite3");
    let db = rusqlite::Connection::open(backup).unwrap();
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let run = f.app.store.current("2026-10-11").unwrap().unwrap();
    fs::write(run.output_path.unwrap(), "人工修订").unwrap();
    let changed = backfill::run(&f.app, &args, now()).unwrap();
    assert_eq!(
        changed["dates"]["2026-10-11"]["error_code"],
        "BACKFILL_EXISTING_OUTPUT_MODIFIED"
    );
    assert_eq!(f.sent(), 2);
}
#[test]
fn no_model_mode_and_explicit_budget_remain_effective_when_optional_limits_are_off() {
    let f = Fixture::new();
    let mut args = f.args(0);
    args.only_no_model = true;
    let state = backfill::run(&f.app, &args, now()).unwrap();
    assert_eq!(f.sent(), 0);
    assert_eq!(state["dates"]["2026-10-10"]["status"], "complete");
    assert!(state["dates"]["2026-10-11"].is_null());
    let f = Fixture::new();
    let mut args = f.args(1);
    let first = backfill::run(&f.app, &args, now()).unwrap();
    assert_eq!(f.sent(), 1);
    assert_eq!(first["stop_reason"], "BACKFILL_CALL_BUDGET_EXHAUSTED");
    let again = backfill::run(&f.app, &args, now()).unwrap();
    assert_eq!(f.sent(), 1);
    assert_eq!(again["calls_used"], 1);
    args.call_budget = Some(2);
    assert_eq!(
        backfill::run(&f.app, &args, now()).unwrap_err().code,
        "BACKFILL_BUDGET_CHANGED"
    );
    assert_eq!(f.sent(), 1);
}
#[test]
fn preflight_input_and_progress_tampering_are_rejected() {
    let f = Fixture::new();
    let args = f.args(2);
    let path = args.preflight.as_ref().unwrap();
    let mut plan: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    plan["preflight"]["dates"][0]["date"] = json!("2026-09-01");
    fs::write(path, plan.to_string()).unwrap();
    assert_eq!(
        backfill::run(&f.app, &args, now()).unwrap_err().code,
        "MAINTENANCE_ARTIFACT_CHANGED"
    );
    assert_eq!(f.sent(), 0);
    let f = Fixture::new();
    let args = f.args(2);
    backfill::run(&f.app, &args, now()).unwrap();
    let path = f
        .app
        .config
        .paths
        .data_dir
        .join("backfill/2026-10-10_2026-10-11/state-v3.json");
    fs::write(path, "{}").unwrap();
    assert_eq!(
        backfill::run(&f.app, &args, now()).unwrap_err().code,
        "BACKFILL_STATE_MODIFIED"
    );
    assert_eq!(f.sent(), 2);
}
