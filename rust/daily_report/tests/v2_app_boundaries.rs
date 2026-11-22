use ai_daily_report::{
    config::AppConfig,
    health,
    maintenance::retention::cleanup,
    operations::task_plan_kind,
    processing_cache::ProcessingSession,
    publish::Publisher,
    run::App,
    scan::cache::ScanCache,
    snapshots::digest,
    store::{Store, SCHEMA},
    types::{new_run_id, RunContext, RunPhase},
};
use chrono::{TimeZone, Utc};
use rusqlite::params;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    time::{Duration, Instant},
};

#[test]
fn staged_period_recovery_commits_original_rows_and_restores_saved_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc.with_ymd_and_hms(2026, 11, 1, 3, 0, 0).unwrap();
    let store = Store::open(&dir.path().join("reports.sqlite3"), now).unwrap();
    let publisher = Publisher::new(&dir.path().join("reports"), &store).unwrap();

    for (kind, label, raw, markdown, summary_written) in [
        (
            "weekly",
            "2026-W41",
            include_str!("fixtures/weekly-v1.json"),
            include_str!("fixtures/weekly-v1.md"),
            false,
        ),
        (
            "monthly",
            "2026-10",
            include_str!("fixtures/monthly-v1.json"),
            include_str!("fixtures/monthly-v1.md"),
            true,
        ),
    ] {
        let id = new_run_id();
        let context =
            RunContext::new(id.clone(), now, "2026-10-31".parse().unwrap(), "old".into()).unwrap();
        store.create(&context, kind, &id, None, now).unwrap();
        let report: Value = serde_json::from_str(raw).unwrap();
        let target = publisher.period_path(kind, &report).unwrap();
        let expected = digest(markdown.as_bytes());
        store
            .connection()
            .unwrap()
            .execute(
                "UPDATE report_runs SET phase='db_committed', quality='complete', report_json=?,
                markdown=?, output_path=?, expected_sha256=? WHERE run_id=?",
                params![raw, markdown, target.to_str().unwrap(), expected, id],
            )
            .unwrap();
        if summary_written {
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, markdown).unwrap();
        }
        assert!(store.current_period(kind, label).unwrap().is_none());

        assert_eq!(publisher.publish(&id, now).unwrap(), target);
        let published = store.current_period(kind, label).unwrap().unwrap();
        assert_eq!(published.run_id, id);
        assert_eq!(published.phase, RunPhase::Published);
        assert_eq!(published.report_json.as_deref(), Some(raw));

        let connection = store.connection().unwrap();
        let statement = format!("SELECT completed_work,raw_json FROM {kind}_reports");
        let (completed, saved): (String, String) = connection
            .query_row(&statement, [], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        assert_eq!(report["completed_work"], completed);
        assert_eq!(saved, raw);
        assert!(store.calls(&id).unwrap().is_empty());

        fs::remove_file(&target).unwrap();
        publisher.publish(&id, now).unwrap();
        assert_eq!(fs::read(&target).unwrap(), markdown.as_bytes());
    }
}

#[test]
fn health_checks_the_current_publication_and_the_v4_schema() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    fs::create_dir(&config_dir).unwrap();
    fs::write(
        config_dir.join("settings.yaml"),
        "paths:\n  work_dir: work\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), Some(&config_dir), &BTreeMap::new()).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 11, 1, 3, 0, 0).unwrap();
    let date = "2026-10-31";
    let store = Store::open(&config.paths.db_dir.join("reports.sqlite3"), now).unwrap();
    let publisher = Publisher::new(&config.paths.reports_dir, &store).unwrap();
    let target = publisher.output_path(date).unwrap();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, b"current report").unwrap();
    let connection = store.connection().unwrap();

    for (id, at, hash) in [
        ("current", "2026-11-01T01:00:00Z", digest(b"current report")),
        (
            "newer-run",
            "2026-11-01T02:00:00Z",
            digest(b"different body"),
        ),
    ] {
        connection.execute(
            "INSERT INTO report_runs(run_id,scope_id,target_date,timezone,context_json,config_fingerprint,
                generation_id,phase,status,created_at,updated_at,quality,expected_sha256,output_path)
             VALUES(?1,'personal_work',?2,'Asia/Shanghai','{}','old',?1,'published','succeeded',
                ?3,?3,'complete',?4,?5)",
            params![id,date,at,hash,target.to_str().unwrap()],
        ).unwrap();
    }
    connection
        .execute(
            "INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,current_run_id)
             VALUES(?,'','','','{}','current')",
            [date],
        )
        .unwrap();
    ScanCache::open(
        config.index_db_path.parent().unwrap(),
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap()
    .initialize()
    .unwrap();

    let health_report = health::check(&config, date.parse().unwrap(), now).unwrap();
    assert!(health_report
        .checks
        .iter()
        .any(|check| check.code == "REPORT_NOT_PUBLISHED"
            && check.level == "ok"
            && check.message.contains("run current")));
    assert!(health_report
        .checks
        .iter()
        .any(|check| check.code == "SCAN_INDEX_INTEGRITY" && check.level == "ok"));

    let cache = rusqlite::Connection::open(&config.index_db_path).unwrap();
    cache.execute("DROP TABLE parse_cache", []).unwrap();
    let health_report = health::check(&config, date.parse().unwrap(), now).unwrap();
    assert!(health_report
        .checks
        .iter()
        .any(|check| check.code == "SCAN_INDEX_INTEGRITY" && check.level == "error"));
}

#[test]
fn failed_cache_ledger_writes_roll_back_tracking_and_only_the_failed_reservation() {
    let root = tempfile::tempdir().unwrap();
    let session = ProcessingSession::at(
        root.path(),
        Utc::now(),
        Instant::now() + Duration::from_secs(10),
        32_768,
        8_192,
        Duration::ZERO,
    )
    .unwrap();
    let quota = &session.quota;
    let candidate = quota.path("candidate").unwrap();
    let blocked_ledger = quota.root.join(".cache-ledger.next");
    let mut request = quota.reserve(4_096).unwrap();
    let other = quota.reserve(8_192).unwrap();

    fs::create_dir(&blocked_ledger).unwrap();
    assert!(request.track(&candidate).is_err());
    fs::remove_dir(&blocked_ledger).unwrap();
    request.track(&candidate).unwrap();
    fs::write(&candidate, b"pending material").unwrap();

    fs::create_dir(&blocked_ledger).unwrap();
    assert!(request.commit(std::slice::from_ref(&candidate)).is_err());
    assert!(!candidate.exists());
    assert_eq!(quota.used_bytes().unwrap(), 8_192);
    assert_eq!(quota.available_bytes().unwrap(), 16_384);
    fs::remove_dir(&blocked_ledger).unwrap();
    drop(other);
    assert_eq!(quota.available_bytes().unwrap(), 24_576);
    quota.write(&candidate, b"retry").unwrap();
    assert_eq!(fs::read(candidate).unwrap(), b"retry");
}

#[test]
fn task_plans_keep_distinct_triggers_commands_and_limits() {
    let dir = tempfile::Builder::new()
        .prefix("task & ")
        .tempdir()
        .unwrap();
    let config_dir = dir.path().join("config");
    fs::create_dir(&config_dir).unwrap();
    fs::write(
        config_dir.join("settings.yaml"),
        "paths:\n  work_dir: work\n",
    )
    .unwrap();
    fs::write(dir.path().join("ai-daily-report.exe"), b"plan fixture").unwrap();
    let mut config =
        AppConfig::load_with_env(dir.path(), Some(&config_dir), &BTreeMap::new()).unwrap();
    config.schedule.restart_count = 2;
    config.schedule.logon_catchup = true;
    let now = Utc.with_ymd_and_hms(2026, 11, 1, 3, 0, 0).unwrap();

    let daily = task_plan_kind(&config, "S-1-5-21-123", now, "daily").unwrap();
    let daily_xml = daily["xml"].as_str().unwrap();
    assert_eq!(daily_xml.matches("<CalendarTrigger ").count(), 1);
    assert!(daily_xml.contains("09:00:00+08:00"));
    assert!(daily_xml.contains("<ExecutionTimeLimit>PT6M</ExecutionTimeLimit>"));
    assert!(daily_xml.contains("daily --scheduled"));
    assert!(daily_xml.contains("&amp;"));
    assert!(!daily_xml.contains("LogonTrigger"));
    assert!(!daily_xml.contains("RestartOnFailure"));

    let health = task_plan_kind(&config, "S-1-5-21-123", now, "health").unwrap();
    let health_xml = health["xml"].as_str().unwrap();
    assert_eq!(health["kind"], "health");
    assert_eq!(health_xml.matches("<CalendarTrigger ").count(), 2);
    assert!(health_xml.contains("11:00:00+08:00"));
    assert!(health_xml.contains("23:00:00+08:00"));
    assert!(health_xml.contains("<Command>powershell.exe</Command>"));
    assert!(health_xml.contains("<ExecutionTimeLimit>PT10M</ExecutionTimeLimit>"));
    assert!(health_xml.contains("health_notify.ps1"));
    assert!(health_xml.contains("&amp;"));
    assert!(!config.paths.db_dir.exists());
}

#[test]
fn cleanup_keeps_revision_calls_and_calls_after_the_oldest_incomplete_job() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    fs::create_dir(&config_dir).unwrap();
    fs::write(
        config_dir.join("settings.yaml"),
        "paths:\n  work_dir: work\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), Some(&config_dir), &BTreeMap::new()).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 11, 1, 3, 0, 0).unwrap();
    let app = App::open_history(config, now).unwrap();
    let connection = app.store.connection().unwrap();
    let mut ids = Vec::new();
    for call_id in 1..=4 {
        let id = new_run_id();
        let then = now - chrono::Duration::days(60);
        let context = RunContext::new(id.clone(), then, then.date_naive(), "old".into()).unwrap();
        app.store
            .create(&context, "preview", &id, None, then)
            .unwrap();
        connection
            .execute(
                "UPDATE report_runs SET phase='preview',status='succeeded',finished_at=created_at
             WHERE run_id=?",
                [&id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO report_model_calls(call_id,run_id,generation_id,request_fingerprint,
                 logical_step,attempt,call_date,status,usage_json,started_at)
             VALUES(?1,?2,?2,'req','report',1,'2026-09-01','succeeded','{}',?3)",
                params![call_id, id, then.to_rfc3339()],
            )
            .unwrap();
        ids.push(id);
    }
    connection
        .execute_batch(
            "INSERT INTO report_metadata(key,value) VALUES
            ('job_calls_revision:prior','{\"revision\":{\"retained_call_ids\":[1]}}'),
            ('backfill_budget:complete','{\"baseline_call_id\":0}'),
            ('backfill_complete:complete','{}'),
            ('backfill_budget:first','{\"baseline_call_id\":2}'),
            ('backfill_budget:second','{\"baseline_call_id\":3}');",
        )
        .unwrap();

    let result = cleanup(&app, now, true).unwrap();
    assert_eq!(result.deleted_runs, 1);
    assert_eq!(app.store.get(&ids[1]).unwrap_err().code, "RUN_NOT_FOUND");
    for index in [0, 2, 3] {
        assert_eq!(app.store.calls(&ids[index]).unwrap().len(), 1);
    }
}

#[test]
fn report_backup_includes_wal_and_retains_the_unmarked_v1_schema() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("reports.sqlite3");
    let backup = dir.path().join("backup.sqlite3");
    let old = rusqlite::Connection::open(&source).unwrap();
    old.execute_batch(SCHEMA).unwrap();
    old.execute_batch(
        "PRAGMA journal_mode=WAL;
         INSERT INTO report_schema_migrations VALUES(1,'2026-11-01T00:00:00Z');
         INSERT INTO report_metadata(key,value) VALUES
             ('scope_id','personal_work'),('timezone','Asia/Shanghai');",
    )
    .unwrap();
    let now = Utc.with_ymd_and_hms(2026, 11, 1, 3, 0, 0).unwrap();
    let store = Store::open(&source, now).unwrap();
    old.execute_batch(
        "INSERT INTO report_runs(run_id,scope_id,target_date,timezone,context_json,config_fingerprint,
             generation_id,phase,status,created_at,updated_at,markdown)
         VALUES('legacy','personal_work','2026-10-31','Asia/Shanghai','{}','old','legacy',
             'published','succeeded','2026-11-01T01:00:00Z','2026-11-01T01:00:00Z','原字节');
         INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,current_run_id)
         VALUES('2026-10-31','旧字段','','','{}','legacy');
         INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,logical_step,attempt,
             call_date,status,usage_json,started_at)
         VALUES('legacy','legacy','req','report',1,'2026-10-31','unknown','{}','2026-11-01T01:00:00Z');"
    ).unwrap();
    assert!(
        fs::metadata(source.with_extension("sqlite3-wal"))
            .unwrap()
            .len()
            > 32
    );

    let hash = store.backup(&backup).unwrap();
    assert_eq!(digest(&fs::read(&backup).unwrap()), hash);
    assert_eq!(
        store.backup(&backup).unwrap_err().code,
        "BACKUP_ALREADY_EXISTS"
    );
    let restored = Store::open(&backup, now).unwrap();
    let run = restored.current("2026-10-31").unwrap().unwrap();
    assert_eq!(run.markdown.as_deref(), Some("原字节"));
    assert_eq!(restored.calls(&run.run_id).unwrap()[0]["status"], "unknown");

    for connection in [old, restored.connection().unwrap()] {
        let row: (i64, String) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM sqlite_schema WHERE type='trigger'),completed_work
             FROM daily_reports WHERE date='2026-10-31'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (0, "旧字段".into()));
    }
}

#[test]
fn report_schema_rejects_a_v1_column_constraint_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reports.sqlite3");
    let modified = SCHEMA.replace("scope_id='personal_work'", "scope_id='other_work'");
    assert_ne!(modified, SCHEMA);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(&modified)
        .unwrap();
    let before = fs::read(&path).unwrap();

    let error = Store::open(&path, Utc::now()).unwrap_err();
    assert_eq!(error.code, "REPORT_SCHEMA_UNSUPPORTED");
    assert_eq!(fs::read(path).unwrap(), before);
}
