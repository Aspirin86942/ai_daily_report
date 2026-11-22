#[path = "support/worker_path.rs"]
mod worker_path;
use ai_daily_report::{
    config::AppConfig,
    health,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    operations,
    run::App,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    config.pdf_worker_path = worker_path::pdf_worker();
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
        assert!(value["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with(ai_daily_report::model::personal::SCAN_PROMPT_V9));
        let input: Value =
            serde_json::from_str(value["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(input["max_chars"], 1500);
        assert_eq!(input["sources"][0]["source_id"], "user_input");
        assert!(input["sources"][0]["text"]
            .as_str()
            .unwrap()
            .contains("虚构"));
        let content = json!({"schema_version":5,"items":[{"title":"接口检查","text":"采购审批方案待评审。","kind":"attention","statement_kind":"user_statement","source_ids":["user_input"]}]});
        Ok(HttpResponse{status:200,retry_after:None,body:json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}],"usage":{"total_tokens":123}}).to_string().into_bytes()})
    }
}
struct OpenAiNetwork(Option<&'static str>);
impl ModelTransport for OpenAiNetwork {
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        let value: Value = serde_json::from_slice(request).unwrap();
        if let Some(effort) = self.0 {
            assert_eq!(value["reasoning_effort"], effort);
            assert!(value["max_completion_tokens"].as_u64().unwrap() > 0);
            assert!(value.get("max_tokens").is_none());
            assert_eq!(value.get("temperature").is_some(), effort == "none");
        } else {
            assert!(value["max_tokens"].as_u64().unwrap() > 0);
            assert!(value.get("max_completion_tokens").is_none());
            assert!(value.get("reasoning_effort").is_none());
            assert!(value.get("temperature").is_some());
        }
        assert!(value.get("thinking").is_none());
        assert!(value.get("extra_body").is_none());
        Network(true).send(request, timeout)
    }
}

#[test]
fn openai_model_probe_audits_modern_and_legacy_request_formats() {
    for effort in [None, Some("none"), Some("medium")] {
        let (_dir, mut config) = fixture();
        config.llm.provider = "openai".into();
        config.llm.model_id = "gpt-6-luna".into();
        config.llm.reasoning_effort = effort.map(str::to_owned);
        config.validate().unwrap();
        let mut app = App::open(config, now()).unwrap();
        app.transport = Box::new(OpenAiNetwork(effort));
        let result = operations::model_check(&app, now()).unwrap();
        assert_eq!(result["calls"], 1);
        let calls = app.store.calls(result["run_id"].as_str().unwrap()).unwrap();
        assert_eq!(calls[0]["status"], "succeeded");
        assert_eq!(
            operations::task_plan_kind(&app.config, "S-1-5-21-123", now(), "daily").unwrap()
                ["model_check_verified"],
            true
        );
    }
}

#[test]
fn reasoning_configuration_preserves_legacy_fingerprints_and_rejects_mixed_protocols() {
    let (_dir, mut config) = fixture();
    let fingerprint = config.fingerprint(false).unwrap();
    let mut serialized = serde_json::to_value(&config.llm).unwrap();
    assert!(serialized.get("reasoning_effort").is_none());
    serialized["reasoning_effort"] = Value::Null;
    config.llm = serde_json::from_value(serialized).unwrap();
    assert_eq!(config.fingerprint(false).unwrap(), fingerprint);
    config.llm.reasoning_effort = Some("none".into());
    assert_eq!(config.validate().unwrap_err().code, "CONFIG_INVALID");
    config.llm.provider = "openai".into();
    config.validate().unwrap();
    config.llm.thinking_mode = Some("disabled".into());
    assert_eq!(config.validate().unwrap_err().code, "CONFIG_INVALID");
    config.llm.thinking_mode = None;
    config.llm.reasoning_effort = Some("unsupported".into());
    assert_eq!(config.validate().unwrap_err().code, "CONFIG_INVALID");
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
    let templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
    let names = fs::read_dir(&templates)
        .unwrap()
        .map(|file| file.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("daily_") && name.ends_with(".md"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names,
        operations::TEMPLATES
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect()
    );
    for (name, body) in operations::TEMPLATES {
        assert_eq!(
            fs::read(templates.join(name)).unwrap(),
            body.as_bytes(),
            "{name}"
        );
    }
    let value = operations::doctor(&app, false, now()).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["templates"], names.len());
    assert_eq!(app.store.list_runs(None, 100).unwrap().len(), 0);
    for name in [
        "daily_prompt.md",
        "daily_compact_prompt.md",
        "daily_render_template.md",
    ] {
        let path = dir.path().join("templates").join(name);
        fs::write(&path, "changed").unwrap();
        assert_eq!(
            operations::doctor(&app, false, now()).unwrap_err().code,
            "TEMPLATE_BUILD_MISMATCH"
        );
        fs::copy(templates.join(name), &path).unwrap();
    }
}
#[test]
fn explicit_model_probe_is_audited_and_task_plan_requires_current_config_success() {
    let (_dir, config) = fixture();
    let mut app = App::open(config, now()).unwrap();
    app.transport = Box::new(Network(true));
    let result = operations::model_check(&app, now()).unwrap();
    assert_eq!(result["calls"], 1);
    let record = app.store.get(result["run_id"].as_str().unwrap()).unwrap();
    let snapshot: Value = serde_json::from_slice(
        &fs::read(
            app.config
                .paths
                .data_dir
                .join("daily_cache/snapshots")
                .join(record.snapshot_path.unwrap()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot["plan"]["version"], 8);
    assert_eq!(snapshot["plan"]["target"]["summary_prompt_version"], 9);
    assert_eq!(snapshot["plan"]["target"]["publication"]["max_chars"], 1500);
    let calls = app.store.calls(result["run_id"].as_str().unwrap()).unwrap();
    assert_eq!(calls[0]["status"], "succeeded");
    assert_eq!(
        calls[0]["request_sha256"],
        ai_daily_report::snapshots::digest(
            &ai_daily_report::snapshots::canonical_json(&snapshot["plan"]["request"]).unwrap()
        )
    );
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
#[ignore = "requires the retained phase-three Windows bundle"]
fn retained_scanner_archive_includes_wal_and_preserves_original() {
    let old = PathBuf::from(std::env::var_os("AI_DAILY_RETAINED_BUNDLE").unwrap());
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("scanner.sqlite3");
    let conn = rusqlite::Connection::open(&source).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE sample(value TEXT);INSERT INTO sample VALUES('retained');PRAGMA user_version=3;").unwrap();
    let before = fs::read(&source).unwrap();
    let output = std::process::Command::new(old.join("ai-daily-report.exe"))
        .args(["archive-scanner", "--source"])
        .arg(&source)
        .arg("--archive-dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let db = rusqlite::Connection::open_with_flags(
        dir.path().join(result["archive_name"].as_str().unwrap()),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(
        db.query_one("SELECT value FROM sample", [], |row| row
            .get::<_, String>(0))
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

#[test]
fn health_keeps_attention_for_empty_personal_publications() {
    let (_dir, config) = fixture();
    let app = App::open(config, now()).unwrap();
    let db = app.store.connection().unwrap();
    db.execute_batch(
        "INSERT INTO report_runs(run_id,scope_id,target_date,timezone,context_json,config_fingerprint,generation_id,phase,status,created_at,updated_at)
         VALUES ('earlier','personal_work','2026-10-14','Asia/Shanghai','{}','fp','old','generating','failed','2026-10-15T01:00:00+00:00','2026-10-15T01:00:00+00:00'),
                ('current','personal_work','2026-10-14','Asia/Shanghai','{}','fp','new','published','succeeded','2026-10-15T02:00:00+00:00','2026-10-15T02:00:00+00:00');
         INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,current_run_id)
         VALUES ('2026-10-14','','','','{}','current');"
    ).unwrap();
    for (version, items, quality, unresolved) in [
        (4, json!([]), "partial", 0),
        (5, json!([]), "partial", 1),
        (6, json!([]), "partial", 1),
        (6, json!([]), "no_changes", 0),
        (6, json!([{"title":"已核对"}]), "partial", 0),
    ] {
        db.execute(
            "UPDATE report_runs SET report_json=?1,quality=?2 WHERE run_id='current'",
            rusqlite::params![
                json!({"schema_version":version,"items":items}).to_string(),
                quality
            ],
        )
        .unwrap();
        let report = health::check(&app.config, "2026-10-14".parse().unwrap(), now()).unwrap();
        let attention = report
            .checks
            .iter()
            .find(|check| check.code == "RUN_ATTENTION")
            .unwrap();
        assert_eq!(attention.evidence["pending"], unresolved);
        assert_eq!(attention.evidence["recent_failures"], unresolved);
    }
}

#[test]
fn scheduled_health_preserves_a_newer_or_already_enriched_observation() {
    let (_dir, config) = fixture();
    let date = now().date_naive();
    let earlier = health::check(&config, date, now()).unwrap();
    let later = health::check(&config, date, now() + chrono::Duration::seconds(1)).unwrap();
    let path = health::persist(&later).unwrap();
    let mut combined = serde_json::to_value(&later).unwrap();
    combined["tasks"] = json!([{"name":"verified"}]);
    let bytes = serde_json::to_vec(&combined).unwrap();
    fs::write(&path, &bytes).unwrap();
    assert_eq!(health::persist(&earlier).unwrap(), path);
    assert_eq!(health::persist(&later).unwrap(), path);
    assert_eq!(fs::read(path).unwrap(), bytes);
}
