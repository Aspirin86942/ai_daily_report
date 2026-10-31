use ai_daily_report::{config::AppConfig, operations};
use chrono::Utc;
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
            "health",
            json!({"status":"warning","checks":[]}),
            "warning",
            "succeeded",
            "unknown",
            "not_applicable",
        ),
        (
            "daily",
            json!({"action":"published","quality":"partial","target_date":"2026-10-18","reason_codes":["MODEL_INPUT_BOUNDED"]}),
            "succeeded",
            "succeeded",
            "partial",
            "published",
        ),
        (
            "daily",
            json!({"action":"skipped","reason":"already_running"}),
            "skipped",
            "skipped",
            "unknown",
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
    assert!(events
        .iter()
        .all(|event| event.get("queued").is_none() && event.get("pending_changes").is_none()));
    assert_eq!(events[1]["reason_codes"], json!(["MODEL_INPUT_BOUNDED"]));
}

#[test]
fn daily_task_limit_includes_time_to_save_status() {
    let (dir, mut config) = fixture();
    fs::write(dir.path().join("ai-daily-report.exe"), b"task fixture").unwrap();
    config.recovery.run_budget_seconds = 300;
    config.schedule.execution_limit_minutes = 5;
    assert_eq!(
        operations::task_plan(&config, "S-1-5-21-123", Utc::now())
            .unwrap_err()
            .code,
        "TASK_RUNTIME_LIMIT_TOO_SHORT"
    );
    config.schedule.execution_limit_minutes = 6;
    operations::task_plan(&config, "S-1-5-21-123", Utc::now()).unwrap();
}

#[test]
fn collection_commands_and_task_kinds_are_retired() {
    use ai_daily_report::cli::Cli;
    use clap::Parser;
    for args in [
        vec!["app", "collect"],
        vec!["app", "archive", "drain"],
        vec!["app", "archive", "retry", "--version", "old"],
        vec!["app", "archive", "activate-schedule"],
        vec!["app", "daily", "--source", "archive"],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
    let (_dir, config) = fixture();
    for kind in ["collect", "parse"] {
        assert!(Cli::try_parse_from([
            "app",
            "task",
            "plan",
            "--kind",
            kind,
            "--sid",
            "S-1-5-21-123"
        ])
        .is_err());
        assert_eq!(
            operations::task_plan_kind(&config, "S-1-5-21-123", Utc::now(), kind)
                .unwrap_err()
                .code,
            "TASK_KIND_INVALID"
        );
    }
    assert_eq!(
        serde_json::from_str::<ai_daily_report::types::ReportSource>("\"archive\"").unwrap(),
        ai_daily_report::types::ReportSource::Archive
    );
}
