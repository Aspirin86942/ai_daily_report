use ai_daily_report::{config::AppConfig, operations};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs};

fn fixture() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: work\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    (dir, config)
}

#[test]
fn scheduled_results_keep_execution_material_and_publication_separate() {
    let (_dir, config) = fixture();
    let cases = [
        (
            "collect",
            json!({"status":"partial","execution_status":"succeeded","material_status":"partial","queued":15,"errors":0,"reason_codes":["MEMORY_RESERVE"]}),
            "partial",
            "succeeded",
            "partial",
            "not_applicable",
        ),
        (
            "health",
            json!({"status":"warning","checks":[]}),
            "warning",
            "succeeded",
            "unknown",
            "not_applicable",
        ),
        (
            "daily",
            json!({"action":"published","quality":"partial","target_date":"2026-10-18"}),
            "succeeded",
            "succeeded",
            "partial",
            "published",
        ),
        (
            "daily",
            json!({"action":"deferred","material_status":"partial","queued":3,"reason_codes":["ARCHIVE_INPUT_PENDING"]}),
            "deferred",
            "deferred",
            "partial",
            "not_published",
        ),
    ];
    for (kind, value, _, _, _, _) in &cases {
        operations::append_scheduled_log(
            &config,
            kind,
            1,
            None,
            Some("2026-10-18"),
            &Ok(value.clone()),
        )
        .unwrap();
    }
    let mut events = vec![];
    for entry in fs::read_dir(&config.paths.log_dir).unwrap() {
        for line in fs::read_to_string(entry.unwrap().path()).unwrap().lines() {
            events.push(serde_json::from_str::<Value>(line).unwrap());
        }
    }
    for (event, (_, _, status, execution, materials, publication)) in events.iter().zip(cases) {
        assert_eq!(event["status"], status);
        assert_eq!(event["execution_status"], execution);
        assert_eq!(event["material_status"], materials);
        assert_eq!(event["publication_status"], publication);
    }
    assert_eq!(events[0]["queued"], 15);
    assert_eq!(events[0]["reason_codes"], json!(["MEMORY_RESERVE"]));
}

#[test]
fn archive_daily_task_limit_includes_bounded_material_preparation() {
    let (dir, mut config) = fixture();
    fs::write(dir.path().join("ai-daily-report.exe"), b"task fixture").unwrap();
    config.archive.daily_source = "archive".into();
    config.recovery.run_budget_seconds = 1800;
    config.schedule.execution_limit_minutes = 31;
    assert_eq!(
        operations::task_plan(&config, "S-1-5-21-123", Utc::now())
            .unwrap_err()
            .code,
        "TASK_RUNTIME_LIMIT_TOO_SHORT"
    );
    config.schedule.execution_limit_minutes = 33;
    operations::task_plan(&config, "S-1-5-21-123", Utc::now()).unwrap();
    config.archive.daily_source = "scan".into();
    config.schedule.execution_limit_minutes = 31;
    operations::task_plan(&config, "S-1-5-21-123", Utc::now()).unwrap();
}

#[test]
fn parse_task_retries_only_inside_the_workday_window() {
    let (dir, mut config) = fixture();
    fs::write(dir.path().join("ai-daily-report.exe"), b"task fixture").unwrap();
    config.archive.collection_workday_window =
        Some(ai_daily_report::archive::config::WorkdayWindow {
            start: "09:00".into(),
            end: "18:00".into(),
        });
    let now = DateTime::parse_from_rfc3339("2026-10-19T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let plan = operations::task_plan_kind(&config, "S-1-5-21-123", now, "parse").unwrap();
    let xml = plan["xml"].as_str().unwrap();
    assert_eq!(plan["kind"], "parse");
    assert!(plan["arguments"]
        .as_str()
        .unwrap()
        .contains("archive drain --scheduled"));
    assert!(xml.contains("T09:05:00+08:00"));
    assert!(xml.contains("T17:35:00+08:00"));
    assert!(xml.contains("T18:00:00+08:00"));
    assert!(!xml.contains("<Saturday/>"));
    assert!(!xml.contains("<Sunday/>"));
    assert!(!xml.contains("T18:05:00+08:00"));
}
