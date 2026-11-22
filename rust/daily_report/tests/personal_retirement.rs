use ai_daily_report::{
    cli::Cli,
    config::{AppConfig, ReportProfile},
    publish::Publisher,
    run::{repair_by_id, App},
    snapshots::digest,
    store::{locking::ReportLock, Store},
    types::{new_run_id, RunContext},
};
use chrono::{TimeZone, Utc};
use clap::Parser;
use rusqlite::params;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, process::Command, time::Duration};

#[test]
fn retired_commands_fail_before_loading_configuration_or_sending_requests() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["weekly", "2026-W41", "--source", "db"],
        vec!["monthly", "2026-10", "--source", "scan"],
        vec!["backfill", "--workers", "4"],
        vec!["coverage", "recover", "--plan", "unused"],
        vec!["ocr", "run", "--engine", "unused"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_ai-daily-report"))
            .arg("--config-dir")
            .arg(dir.path().join("missing"))
            .args(&args)
            .output()
            .unwrap();
        assert!(!result.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&result.stderr).unwrap();
        assert_eq!(error["error_code"], "FEATURE_RETIRED", "{args:?}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    let help = Command::new(env!("CARGO_BIN_EXE_ai-daily-report"))
        .arg("--help")
        .output()
        .unwrap();
    let text = String::from_utf8(help.stdout).unwrap();
    for name in ["weekly", "monthly", "backfill", "coverage", "ocr"] {
        assert!(!text.lines().any(|line| line.trim_start().starts_with(name)));
    }
}

#[test]
fn repair_requires_exactly_one_selector() {
    assert!(Cli::try_parse_from(["app", "repair-output"]).is_err());
    assert!(Cli::try_parse_from([
        "app",
        "repair-output",
        "--target-date",
        "2026-10-30",
        "--run-id",
        "old"
    ])
    .is_err());
    assert!(Cli::try_parse_from(["app", "repair-output", "--run-id", "old"]).is_ok());
}

#[test]
fn personal_default_and_explicit_evidence_generation_boundary() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    let path = dir.path().join("config/settings.yaml");
    fs::write(&path, "paths:\n  work_dir: work\n").unwrap();
    let personal = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    assert_eq!(personal.report.profile, ReportProfile::Personal);
    fs::write(
        &path,
        "paths:\n  work_dir: work\nreport:\n  profile: evidence\n",
    )
    .unwrap();
    let evidence = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    assert_eq!(
        ai_daily_report::operations::task_plan(&evidence, "S-1-5-21-1", Utc::now())
            .unwrap_err()
            .code,
        "FEATURE_RETIRED"
    );
    let cli = Cli::try_parse_from(["app", "daily"]).unwrap();
    assert_eq!(
        cli.command
            .unwrap()
            .report_request(&evidence, Utc::now(), new_run_id(), String::new())
            .unwrap_err()
            .code,
        "FEATURE_RETIRED"
    );
}

#[test]
fn historical_periods_restore_saved_bytes_without_workers_or_processing_cache() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: missing-work\nreport:\n  profile: evidence\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 10, 31, 3, 0, 0).unwrap();
    let app = App::open_history(config, now).unwrap();
    let publisher = Publisher::new(&app.config.paths.reports_dir, &app.store).unwrap();
    for (kind, fixture, fields) in [
        (
            "weekly",
            include_str!("fixtures/weekly-v1.json"),
            vec![
                "week_label",
                "date_range",
                "completed_work",
                "self_growth",
                "improvement_actions",
                "work_summary",
                "next_plan",
                "support_needed",
                "other_notes",
            ],
        ),
        (
            "monthly",
            include_str!("fixtures/monthly-v1.json"),
            vec![
                "year_month",
                "overview",
                "completed_work",
                "work_summary",
                "next_plan",
            ],
        ),
    ] {
        let report: Value = serde_json::from_str(fixture).unwrap();
        let id = new_run_id();
        let context = RunContext::new(
            id.clone(),
            now,
            now.date_naive().pred_opt().unwrap(),
            "historical".into(),
        )
        .unwrap();
        app.store.create(&context, kind, &id, None, now).unwrap();
        let body = format!("# 保留的{kind}\r\n原样正文 120 元\r\n");
        let path = publisher.period_path(kind, &report).unwrap();
        let conn = app.store.connection().unwrap();
        conn.execute("UPDATE report_runs SET phase='published',status='succeeded',quality='complete',report_json=?,markdown=?,expected_sha256=?,output_path=? WHERE run_id=?",
            params![report.to_string(),body,digest(body.as_bytes()),path.to_str().unwrap(),id]).unwrap();
        let mut columns = fields.clone();
        columns.push("raw_json");
        let mut values: Vec<String> = fields
            .iter()
            .map(|field| report[*field].as_str().unwrap().to_owned())
            .collect();
        values.push(report.to_string());
        conn.execute(
            &format!(
                "INSERT INTO {kind}_reports({}) VALUES({})",
                columns.join(","),
                vec!["?"; columns.len()].join(",")
            ),
            rusqlite::params_from_iter(values),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO report_metadata(key,value) VALUES(?,?)",
            params![
                format!(
                    "current_period:{kind}:{}",
                    report[fields[0]].as_str().unwrap()
                ),
                id
            ],
        )
        .unwrap();
        drop(conn);
        let before = fs::read(&app.store.path).unwrap();
        let result = repair_by_id(&app, &id, now).unwrap();
        assert_eq!(result.model_calls, 0);
        assert_eq!(fs::read(&path).unwrap(), body.as_bytes());
        assert_eq!(fs::read(&app.store.path).unwrap(), before);
        fs::write(&path, "人工修改").unwrap();
        assert!(repair_by_id(&app, &id, now).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "人工修改");
    }
    assert!(!app.config.paths.data_dir.join("daily_cache").exists());
    assert!(!app.config.paths.data_dir.join("run_snapshots").exists());
    assert!(!app.config.index_db_path.exists());
}

#[test]
fn personal_startup_preserves_unfinished_legacy_runs_and_call_ledgers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reports.sqlite3");
    let lock = ReportLock::acquire(&path, "personal_work", Duration::ZERO).unwrap();
    let now = Utc::now();
    let store = Store::open_locked(&path, now, &lock).unwrap();
    let context = RunContext::new(
        new_run_id(),
        now,
        now.date_naive().pred_opt().unwrap(),
        "legacy".into(),
    )
    .unwrap();
    store
        .create(&context, "weekly", &context.run_id, None, now)
        .unwrap();
    let conn = store.connection().unwrap();
    conn.execute("INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,logical_step,attempt,call_date,status,started_at) VALUES(?,?,'retained','batch_000',1,'2026-10-30','reserved','2026-10-30T00:00:00Z')",params![context.run_id,context.run_id]).unwrap();
    let before = fs::read(&path).unwrap();
    store.recover_interrupted(now, &lock).unwrap();
    assert_eq!(store.get(&context.run_id).unwrap().error_code, None);
    assert_eq!(
        store.calls(&context.run_id).unwrap()[0]["status"],
        json!("reserved")
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn new_personal_runs_are_registered_atomically_and_partial_inserts_roll_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reports.sqlite3");
    let lock = ReportLock::acquire(&path, "personal_work", Duration::ZERO).unwrap();
    let now = Utc::now();
    let store = Store::open_locked(&path, now, &lock).unwrap();
    let context = RunContext::new(
        new_run_id(),
        now,
        now.date_naive().pred_opt().unwrap(),
        "personal".into(),
    )
    .unwrap();
    let key = format!("personal_run:{}", context.run_id);
    store
        .connection()
        .unwrap()
        .execute(
            "INSERT INTO report_metadata(key,value) VALUES(?,'retained')",
            [&key],
        )
        .unwrap();
    assert!(store
        .create_personal(&context, "report", &context.run_id, None, now)
        .is_err());
    assert_eq!(
        store.get(&context.run_id).unwrap_err().code,
        "RUN_NOT_FOUND"
    );
    store
        .connection()
        .unwrap()
        .execute("DELETE FROM report_metadata WHERE key=?", [&key])
        .unwrap();
    store
        .create_personal(&context, "report", &context.run_id, None, now)
        .unwrap();
    assert!(store.is_personal_run(&context.run_id).unwrap());
    store.recover_interrupted(now, &lock).unwrap();
    assert_eq!(
        store.get(&context.run_id).unwrap().error_code.as_deref(),
        Some("RUN_INTERRUPTED")
    );
}
