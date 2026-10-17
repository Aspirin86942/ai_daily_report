use ai_daily_report::{
    config::AppConfig, operations::append_scheduled_log, store::Store, ReportError,
};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{collections::BTreeMap, fs, time::Instant};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-17T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn scheduled_log_records_kind_run_date_phase_and_duration() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::create_dir_all(dir.path().join("data/db")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: work\n  data_dir: data\n  reports_dir: data/reports\n  db_dir: data/db\n  log_dir: logs\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    let store = Store::open(&dir.path().join("data/db/reports.sqlite3"), now()).unwrap();
    let context: ai_daily_report::types::RunContext =
        serde_json::from_str(include_str!("fixtures/context-v2.json")).unwrap();
    let generation = "a".repeat(32);
    store
        .create(&context, "report", &generation, None, now())
        .unwrap();
    let started = Instant::now();
    let ok = Ok(json!({"run_id":context.run_id,"target_date":"2026-10-16","action":"generated","quality":"partial"}));
    append_scheduled_log(
        &config,
        "daily",
        started.elapsed().as_millis() as u64,
        Some(&context.run_id),
        Some("2026-10-16"),
        &ok,
    )
    .unwrap();
    let failed = Err(ReportError::new("MODEL_RESPONSE_INVALID", "无效响应"));
    append_scheduled_log(
        &config,
        "daily",
        5,
        Some(&context.run_id),
        Some("2026-10-16"),
        &failed,
    )
    .unwrap();
    let text = fs::read_to_string(dir.path().join("logs/scheduled-2026-10-17.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["kind"], "daily");
    assert_eq!(lines[0]["run_id"], context.run_id);
    assert_eq!(lines[0]["target_date"], "2026-10-16");
    assert_eq!(lines[0]["status"], "succeeded");
    assert!(lines[0]["duration_ms"].as_u64().is_some());
    assert_eq!(lines[1]["status"], "failed");
    assert_eq!(lines[1]["error_code"], "MODEL_RESPONSE_INVALID");
    assert_eq!(lines[1]["phase"], "created", "失败记录应带运行阶段");
}
