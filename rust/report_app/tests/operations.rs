use ai_daily_report::{
    config::AppConfig,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    operations,
    run::App,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
fn fixture() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::Builder::new()
        .prefix("rust report & 中文 ")
        .tempdir()
        .unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::create_dir(dir.path().join("work")).unwrap();
    fs::write(dir.path().join("config/settings.yaml"),"paths:\n  work_dir: work\nllm:\n  max_request_utf8_bytes: 64000\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\n").unwrap();
    fs::write(dir.path().join("ai-daily-report.exe"), b"task-plan-fixture").unwrap();
    let mut config = AppConfig::load_with_env(
        dir.path(),
        None,
        &BTreeMap::from([("DEEPSEEK_API_KEY".into(), "synthetic-key".into())]),
    )
    .unwrap();
    let workers = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
    config.office_worker_path =
        fs::canonicalize(workers.join("ai-daily-office-parser.exe")).unwrap();
    config.pdf_worker_path = fs::canonicalize(workers.join("ai-daily-pdf-parser.exe")).unwrap();
    fs::create_dir(dir.path().join("templates")).unwrap();
    for (name, body) in operations::TEMPLATES {
        fs::write(dir.path().join("templates").join(name), body).unwrap();
    }
    (dir, config)
}
struct Network(bool);
impl ModelTransport for Network {
    fn send(&self, request: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        if !self.0 {
            return Ok(HttpResponse {
                status: 400,
                retry_after: None,
                body: b"{}".to_vec(),
            });
        }
        let value: Value = serde_json::from_slice(request).unwrap();
        assert!(value["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("虚构"));
        let content = json!({"schema_version":3,"facts":[{"topic":"接口检查","background":null,"detail":"采购审批方案待评审。","conditions":[],"status":"草稿","source_ids":["S001"],"uncertainty":null}],"pending":[]});
        Ok(HttpResponse{status:200,retry_after:None,body:json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}],"usage":{"total_tokens":123}}).to_string().into_bytes()})
    }
}
#[test]
fn task_plan_is_read_only_escapes_xml_and_uses_direct_rust_entry() {
    let (_dir, config) = fixture();
    let plan = operations::task_plan(&config, "S-1-5-21-123", now()).unwrap();
    assert!(!config.paths.db_dir.exists());
    assert_eq!(plan["model_check_verified"], false);
    assert!(plan["xml"].as_str().unwrap().contains("&amp;"));
    assert!(plan["xml"].as_str().unwrap().contains("09:00:00+08:00"));
    assert!(plan["arguments"]
        .as_str()
        .unwrap()
        .ends_with("daily --scheduled"));
    assert!(plan["program"]
        .as_str()
        .unwrap()
        .ends_with("ai-daily-report.exe"));
    assert!(!plan.to_string().contains("synthetic-key"));
    assert!(operations::task_plan(&config, "S-1-<script>", now()).is_err());
}
#[test]
fn strict_doctor_uses_real_same_build_workers_and_checks_packaged_templates() {
    let (dir, config) = fixture();
    let app = App::open(config, now()).unwrap();
    let value = operations::doctor(&app, false, now()).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(app.store.list_runs(None, 100).unwrap().len(), 0);
    fs::write(dir.path().join("templates/weekly_template.md"), "changed").unwrap();
    assert_eq!(
        operations::doctor(&app, false, now()).unwrap_err().code,
        "TEMPLATE_BUILD_MISMATCH"
    );
}
#[test]
fn explicit_model_probe_is_audited_and_task_plan_requires_current_config_success() {
    let (_dir, config) = fixture();
    let mut app = App::open(config, now()).unwrap();
    app.transport = Box::new(Network(true));
    let result = operations::model_check(&app, now()).unwrap();
    assert_eq!(result["calls"], 1);
    let calls = app.store.calls(result["run_id"].as_str().unwrap()).unwrap();
    assert_eq!(calls[0]["status"], "succeeded");
    assert_eq!(
        operations::task_plan(&app.config, "S-1-5-21-123", now()).unwrap()["model_check_verified"],
        true
    );
    app.config.llm.temperature = 0.1;
    assert_eq!(
        operations::task_plan(&app.config, "S-1-5-21-123", now()).unwrap()["model_check_verified"],
        false
    );
    app.transport = Box::new(Network(false));
    assert_eq!(
        operations::model_check(&app, now()).unwrap_err().code,
        "MODEL_HTTP_ERROR"
    );
    assert_eq!(app.store.list_runs(None, 100).unwrap().len(), 2);
}
#[test]
fn scanner_archive_includes_wal_and_leaves_original_bytes_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("scanner.sqlite3");
    let conn = rusqlite::Connection::open(&source).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE sample(value TEXT);INSERT INTO sample VALUES('retained');PRAGMA user_version=3;").unwrap();
    let before = fs::read(&source).unwrap();
    let result = operations::archive_scanner(&source, dir.path(), now()).unwrap();
    assert_eq!(fs::read(&source).unwrap(), before);
    let target = dir.path().join(result["archive_name"].as_str().unwrap());
    let backup =
        rusqlite::Connection::open_with_flags(target, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        backup
            .query_row("SELECT value FROM sample", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "retained"
    );
}
#[test]
fn dotenv_uses_fixed_config_parent_and_process_override_without_exposing_values() {
    let (dir, _) = fixture();
    fs::write(
        dir.path().join(".env"),
        "DEEPSEEK_API_KEY='file-key'\nDAILY_REPORT_LLM__MAX_TOKENS=3072\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(
        dir.path(),
        None,
        &BTreeMap::from([("DEEPSEEK_API_KEY".into(), "process-key".into())]),
    )
    .unwrap();
    assert_eq!(config.api_key(), "process-key");
    assert_eq!(config.llm.max_tokens, 3072);
    assert!(!format!("{config:?}").contains("process-key"));
}
