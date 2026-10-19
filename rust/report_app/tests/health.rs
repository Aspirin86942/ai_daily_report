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
fn parse_queue_reports_wait_age_and_latest_deferral_reason() {
    use ai_daily_report::archive::store::ArchiveStore;
    let (_dir, config) = setup();
    let store = ArchiveStore::open(&config, true).unwrap();
    store.conn.execute_batch(r#"
        INSERT INTO collections(id,started_at,status,baseline) VALUES('capture','2026-10-17T10:00:00Z','partial',0);
        INSERT INTO collections(id,started_at,status,baseline,metrics_json) VALUES('parse-run','2026-10-18T02:00:00Z','parse_partial',0,'{"operation":"parse","queued":1,"reason_codes":["MEMORY_RESERVE"]}');
        INSERT INTO documents(id,root,path,extension,baseline) VALUES('doc','root','queued.pdf','.pdf',0);
        INSERT INTO objects VALUES('raw',1,'2026-10-17T10:00:00Z');
        INSERT INTO versions(id,document_id,sha256,path,extension,size_bytes,modified_at,observed_at,collection_id,baseline) VALUES('version','doc','raw','queued.pdf','.pdf',1,'2026-10-17T10:00:00Z','2026-10-17T10:00:00Z','capture',0);
        INSERT INTO extractions(id,sha256,profile,extension,status) VALUES('job','raw','profile','.pdf','queued');
        INSERT INTO version_extractions VALUES('version','job');
    "#).unwrap();
    let report = health::check(
        &config,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        now(),
    )
    .unwrap();
    let check = report
        .checks
        .iter()
        .find(|check| check.code == "PARSE_QUEUE")
        .unwrap();
    assert_eq!(check.evidence["queued"], 1);
    assert_eq!(check.evidence["oldest_observed_at"], "2026-10-17T10:00:00Z");
    assert_eq!(check.evidence["oldest_age_seconds"], 61200);
    assert_eq!(
        check.evidence["last_parse_run"]["reason_codes"],
        serde_json::json!(["MEMORY_RESERVE"])
    );
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
fn task_names_include_parse_task() {
    let (_dir, config) = setup();
    let report = health::check(
        &config,
        chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        now(),
    )
    .unwrap();
    let name = &config.schedule.task_name;
    assert_eq!(report.task_names.parse, format!("{name} Parse"));
    assert_eq!(report.task_names.daily, *name);
    assert_eq!(report.task_names.collect, format!("{name} Collect"));
    assert_eq!(report.task_names.health, format!("{name} Health"));
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
