use ai_daily_report::{config::AppConfig, health, store::Store};
use chrono::{DateTime, Utc};
use std::{collections::BTreeMap, fs};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-18T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn setup() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("config")).unwrap();
    fs::create_dir_all(dir.path().join("work")).unwrap();
    let work = dir
        .path()
        .join("work")
        .display()
        .to_string()
        .replace('\\', "/");
    let data = dir
        .path()
        .join("data")
        .display()
        .to_string()
        .replace('\\', "/");
    fs::write(
        dir.path().join("config/settings.yaml"),
        format!("paths:\n  work_dir: '{work}'\n  data_dir: '{data}'\n  reports_dir: '{data}/reports'\n  db_dir: '{data}/db'\n  log_dir: '{data}/logs'\n"),
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    fs::create_dir_all(&config.paths.db_dir).unwrap();
    fs::create_dir_all(&config.paths.log_dir).unwrap();
    Store::open(&config.paths.db_dir.join("reports.sqlite3"), now()).unwrap();
    (dir, config)
}

#[test]
fn health_reports_error_when_daily_report_is_missing() {
    let (_dir, config) = setup();
    let report = health::check(
        &config,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        now(),
    )
    .unwrap();
    assert_eq!(report.status, "unhealthy");
    assert!(report
        .checks
        .iter()
        .any(|check| check.code == "REPORT_NOT_PUBLISHED" && check.level == "error"));
    assert_eq!(health::exit_code(&report), 2);
}

#[test]
fn health_reports_database_failures_as_unhealthy() {
    let (dir, config) = setup();
    fs::write(
        config.paths.db_dir.join("reports.sqlite3"),
        b"not a database",
    )
    .unwrap();
    let report = health::check(
        &config,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        now(),
    )
    .unwrap();
    assert_eq!(report.status, "unhealthy");
    assert!(report
        .checks
        .iter()
        .any(|check| check.code == "DATABASE_INTEGRITY" && check.level == "error"));
    let _ = dir;
}

#[test]
fn health_reports_empty_database_as_unhealthy() {
    let (_dir, config) = setup();
    fs::write(config.paths.db_dir.join("reports.sqlite3"), b"").unwrap();
    let report = health::check(
        &config,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        now(),
    )
    .unwrap();
    assert_eq!(report.status, "unhealthy");
    assert!(report
        .checks
        .iter()
        .any(|check| check.code == "DATABASE_INTEGRITY" && check.level == "error"));
    assert_eq!(health::exit_code(&report), 2);
}

fn insert_run(
    config: &AppConfig,
    id: &str,
    target_date: &str,
    status: &str,
    phase: &str,
    at: &str,
) {
    let connection =
        rusqlite::Connection::open(config.paths.db_dir.join("reports.sqlite3")).unwrap();
    connection
        .execute(
            "INSERT INTO report_runs(run_id,scope_id,target_date,timezone,run_kind,context_json,config_fingerprint,generation_id,phase,status,created_at,updated_at) VALUES (?1,'personal_work',?2,'Asia/Shanghai','report','{}','fp',?3,?4,?5,?6,?6)",
            rusqlite::params![id, target_date, format!("g{id}"), phase, status, at],
        )
        .unwrap();
}

fn attention(config: &AppConfig, target: &str) -> health::HealthCheck {
    let report = health::check(
        config,
        chrono::NaiveDate::parse_from_str(target, "%Y-%m-%d").unwrap(),
        now(),
    )
    .unwrap();
    report
        .checks
        .into_iter()
        .find(|check| check.code == "RUN_ATTENTION")
        .expect("RUN_ATTENTION 检查必须存在")
}

#[test]
fn task_names_only_include_supported_tasks() {
    let (_dir, config) = setup();
    let report = health::check(
        &config,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        now(),
    )
    .unwrap();
    let name = &config.schedule.task_name;
    assert_eq!(report.task_names.daily, *name);
    assert_eq!(report.task_names.health, format!("{name} Health"));
}

fn check_at(config: &AppConfig, target: &str, at: &str) -> health::HealthReport {
    health::check(
        config,
        chrono::NaiveDate::parse_from_str(target, "%Y-%m-%d").unwrap(),
        DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&Utc),
    )
    .unwrap()
}

#[test]
fn missing_report_waits_for_schedule_and_budget_then_alerts() {
    let (_dir, mut config) = setup();
    config.schedule.local_time = "09:00".into();
    for at in [
        "2026-10-18T00:50:00Z",
        "2026-10-18T01:00:00Z",
        "2026-10-18T01:04:59Z",
    ] {
        let result = check_at(&config, "2026-10-17", at);
        assert!(
            result
                .checks
                .iter()
                .any(|c| c.code == "REPORT_NOT_DUE" && c.level == "ok"),
            "{result:?}"
        );
        assert!(!result
            .checks
            .iter()
            .any(|c| c.code == "REPORT_NOT_PUBLISHED" && c.level == "error"));
    }
    let result = check_at(&config, "2026-10-17", "2026-10-18T01:05:00Z");
    assert!(result
        .checks
        .iter()
        .any(|c| c.code == "REPORT_NOT_PUBLISHED" && c.level == "error"));
}

#[test]
fn report_deadline_uses_configured_shanghai_time_across_midnight() {
    let (_dir, mut config) = setup();
    config.schedule.local_time = "23:59".into();
    let before = check_at(&config, "2026-10-17", "2026-10-18T16:03:59Z");
    assert!(before.checks.iter().any(|c| c.code == "REPORT_NOT_DUE"));
    let after = check_at(&config, "2026-10-17", "2026-10-18T16:04:00Z");
    assert!(after
        .checks
        .iter()
        .any(|c| c.code == "REPORT_NOT_PUBLISHED" && c.level == "error"));
}

#[test]
fn late_catchup_has_a_bounded_running_window_in_every_phase() {
    for phase in [
        "created",
        "scanning",
        "prepared",
        "generating",
        "db_committed",
    ] {
        let (_dir, config) = setup();
        insert_run(
            &config,
            "catchup",
            "2026-10-17",
            "running",
            phase,
            "2026-10-18T02:58:00Z",
        );
        let during = check_at(&config, "2026-10-17", "2026-10-18T03:00:00Z");
        assert!(
            during
                .checks
                .iter()
                .any(|c| c.code == "REPORT_IN_PROGRESS" && c.level == "ok"),
            "{phase}: {during:?}"
        );
        let attention = during
            .checks
            .iter()
            .find(|c| c.code == "RUN_ATTENTION")
            .unwrap();
        assert_eq!(attention.level, "ok", "{phase}: {attention:?}");
        assert_eq!(attention.evidence["active"], 1);
        let expired = check_at(&config, "2026-10-17", "2026-10-18T03:03:00Z");
        assert!(
            expired
                .checks
                .iter()
                .any(|c| c.code == "REPORT_NOT_PUBLISHED" && c.level == "error"),
            "{phase}: {expired:?}"
        );
    }
}

#[test]
fn active_run_does_not_hide_missing_older_report_or_failed_run() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "active",
        "2026-10-17",
        "running",
        "generating",
        "2026-10-18T02:58:00Z",
    );
    insert_run(
        &config,
        "failed",
        "2026-10-16",
        "failed",
        "prepared",
        "2026-10-18T02:59:00Z",
    );
    let result = check_at(&config, "2026-10-16", "2026-10-18T03:00:00Z");
    assert!(result
        .checks
        .iter()
        .any(|c| c.code == "REPORT_NOT_PUBLISHED" && c.level == "error"));
    let attention = result
        .checks
        .iter()
        .find(|c| c.code == "RUN_ATTENTION")
        .unwrap();
    assert_eq!(attention.level, "warning");
    assert_eq!(attention.evidence["pending"], 1);
    assert_eq!(attention.evidence["recent_failures"], 1);
}

#[test]
fn early_check_still_verifies_an_existing_publication() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "published",
        "2026-10-17",
        "succeeded",
        "published",
        "2026-10-18T00:00:00Z",
    );
    let connection =
        rusqlite::Connection::open(config.paths.db_dir.join("reports.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE report_runs SET expected_sha256='missing-file' WHERE run_id='published'",
            [],
        )
        .unwrap();
    let result = check_at(&config, "2026-10-17", "2026-10-18T00:50:00Z");
    assert!(result
        .checks
        .iter()
        .any(|c| c.code == "REPORT_HASH_MISMATCH" && c.level == "error"));
    assert!(!result.checks.iter().any(|c| c.code == "REPORT_NOT_DUE"));
}

#[test]
fn invalid_or_future_run_time_does_not_suppress_alerts() {
    for at in ["invalid-time", "2026-10-18T04:00:00Z"] {
        let (_dir, config) = setup();
        insert_run(
            &config,
            "invalid-clock",
            "2026-10-17",
            "running",
            "generating",
            at,
        );
        let result = check_at(&config, "2026-10-17", "2026-10-18T03:00:00Z");
        assert!(result
            .checks
            .iter()
            .any(|c| c.code == "REPORT_NOT_PUBLISHED" && c.level == "error"));
        assert!(result
            .checks
            .iter()
            .any(|c| c.code == "RUN_ATTENTION" && c.level == "warning"));
    }
}

#[test]
fn run_attention_marks_failure_resolved_by_later_published_report() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "failed-1",
        "2026-10-17",
        "failed",
        "created",
        "2026-10-17T10:00:00+00:00",
    );
    insert_run(
        &config,
        "published-1",
        "2026-10-17",
        "succeeded",
        "published",
        "2026-10-17T12:00:00+00:00",
    );
    let check = attention(&config, "2026-10-17");
    assert_eq!(check.level, "ok", "{check:?}");
    assert_eq!(check.evidence["recent_failures"], 0, "{check:?}");
    assert_eq!(check.evidence["resolved_failures"], 1, "{check:?}");
    assert_eq!(check.evidence["pending"], 0, "{check:?}");
    assert_eq!(
        check.evidence["failures"].as_array().unwrap().len(),
        0,
        "{check:?}"
    );
}

#[test]
fn run_attention_keeps_unresolved_failure() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "failed-1",
        "2026-10-17",
        "failed",
        "created",
        "2026-10-17T10:00:00+00:00",
    );
    let check = attention(&config, "2026-10-17");
    assert_eq!(check.level, "warning", "{check:?}");
    assert_eq!(check.evidence["recent_failures"], 1, "{check:?}");
    assert_eq!(check.evidence["resolved_failures"], 0, "{check:?}");
}

#[test]
fn run_attention_keeps_failure_published_under_another_date() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "failed-1",
        "2026-10-17",
        "failed",
        "created",
        "2026-10-17T10:00:00+00:00",
    );
    // 另一个目标日的正式发布不能证明当天的失败已恢复。
    insert_run(
        &config,
        "published-other",
        "2026-10-16",
        "succeeded",
        "published",
        "2026-10-17T12:00:00+00:00",
    );
    let check = attention(&config, "2026-10-17");
    assert_eq!(check.level, "warning", "{check:?}");
    assert_eq!(check.evidence["recent_failures"], 1, "{check:?}");
    assert_eq!(check.evidence["resolved_failures"], 0, "{check:?}");
}

#[test]
fn run_attention_keeps_failure_newer_than_publish() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "published-1",
        "2026-10-17",
        "succeeded",
        "published",
        "2026-10-17T08:00:00+00:00",
    );
    insert_run(
        &config,
        "failed-later",
        "2026-10-17",
        "failed",
        "created",
        "2026-10-17T10:00:00+00:00",
    );
    let check = attention(&config, "2026-10-17");
    assert_eq!(check.level, "warning", "{check:?}");
    assert_eq!(check.evidence["recent_failures"], 1, "{check:?}");
    assert_eq!(check.evidence["resolved_failures"], 0, "{check:?}");
}

#[test]
fn run_attention_ignores_failures_older_than_24_hours() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "old-failure",
        "2026-10-15",
        "failed",
        "created",
        "2026-10-15T10:00:00+00:00",
    );
    let check = attention(&config, "2026-10-17");
    assert_eq!(check.level, "ok", "{check:?}");
    assert_eq!(check.evidence["recent_failures"], 0, "{check:?}");
    assert_eq!(check.evidence["resolved_failures"], 0, "{check:?}");
}

#[test]
fn run_attention_keeps_failure_resolved_only_by_a_preview() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "failed-1",
        "2026-10-17",
        "failed",
        "created",
        "2026-10-17T10:00:00+00:00",
    );
    // 仅预览（phase=preview）不算正式发布，不能把失败记为已恢复。
    insert_run(
        &config,
        "preview-1",
        "2026-10-17",
        "succeeded",
        "preview",
        "2026-10-17T12:00:00+00:00",
    );
    let check = attention(&config, "2026-10-17");
    assert_eq!(check.level, "warning", "{check:?}");
    assert_eq!(check.evidence["recent_failures"], 1, "{check:?}");
    assert_eq!(check.evidence["resolved_failures"], 0, "{check:?}");
}

#[test]
fn run_attention_keeps_historical_failure_record() {
    let (_dir, config) = setup();
    insert_run(
        &config,
        "failed-1",
        "2026-10-17",
        "failed",
        "created",
        "2026-10-17T10:00:00+00:00",
    );
    insert_run(
        &config,
        "published-1",
        "2026-10-17",
        "succeeded",
        "published",
        "2026-10-17T12:00:00+00:00",
    );
    let _ = attention(&config, "2026-10-17");
    // 健康巡检只读：被恢复的失败记录必须原样保留在库里。
    let connection =
        rusqlite::Connection::open(config.paths.db_dir.join("reports.sqlite3")).unwrap();
    let status: String = connection
        .query_row(
            "SELECT status FROM report_runs WHERE run_id='failed-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "failed");
    let published: i64 = connection
        .query_row(
            "SELECT count(*) FROM report_runs WHERE run_id='published-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(published, 1);
}
