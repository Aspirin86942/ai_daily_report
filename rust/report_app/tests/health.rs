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
fn health_baseline_window_is_not_a_collection_gap() {
    use ai_daily_report::archive::store::ArchiveStore;
    let (_dir, config) = setup();
    let store = ArchiveStore::open(&config, true).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO metadata(key,value) VALUES('baseline','2026-10-17T05:06:34+00:00')",
            [],
        )
        .unwrap();
    let start = DateTime::parse_from_rfc3339("2026-10-16T16:00:00+00:00")
        .unwrap()
        .with_timezone(&Utc);
    let end = DateTime::parse_from_rfc3339("2026-10-17T16:00:00+00:00")
        .unwrap()
        .with_timezone(&Utc);
    let baseline = DateTime::parse_from_rfc3339("2026-10-17T05:06:34+00:00")
        .unwrap()
        .with_timezone(&Utc);
    let mut at = start;
    while at <= end {
        if at >= baseline - chrono::Duration::minutes(10) {
            store
                .conn
                .execute(
                    "INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES(?,?,?,'complete',0)",
                    rusqlite::params![format!("c{}", at.timestamp()), at.to_rfc3339(), at.to_rfc3339()],
                )
                .unwrap();
        }
        at += chrono::Duration::minutes(30);
    }
    let report = health::check(
        &config,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        now(),
    )
    .unwrap();
    assert!(
        !report
            .checks
            .iter()
            .any(|check| check.code == "COLLECTION_GAP" && check.level == "error"),
        "{:?}",
        report.checks
    );
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

#[test]
fn health_previous_day_tail_gap_is_not_today_gap() {
    use ai_daily_report::archive::store::ArchiveStore;
    let (_dir, config) = setup();
    let store = ArchiveStore::open(&config, true).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO metadata(key,value) VALUES('baseline','2026-10-16T00:00:00+00:00')",
            [],
        )
        .unwrap();
    // 前一日最后一轮在目标窗口开始前 1 小时；目标日采集完整，不应把这段算作当日缺口。
    store
        .conn
        .execute(
            "INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES('prev','2026-10-16T15:00:00+00:00','2026-10-16T15:00:00+00:00','complete',0)",
            [],
        )
        .unwrap();
    let start = DateTime::parse_from_rfc3339("2026-10-16T16:00:00+00:00")
        .unwrap()
        .with_timezone(&Utc);
    let end = DateTime::parse_from_rfc3339("2026-10-17T16:00:00+00:00")
        .unwrap()
        .with_timezone(&Utc);
    let mut at = start;
    while at < end {
        store
            .conn
            .execute(
                "INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES(?,?,?,'complete',0)",
                rusqlite::params![format!("t{}", at.timestamp()), at.to_rfc3339(), at.to_rfc3339()],
            )
            .unwrap();
        at += chrono::Duration::minutes(30);
    }
    let report = health::check(
        &config,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        now(),
    )
    .unwrap();
    assert!(
        !report
            .checks
            .iter()
            .any(|check| check.code == "COLLECTION_GAP" && check.level == "error"),
        "{:?}",
        report.checks
    );
}
