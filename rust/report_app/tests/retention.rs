use ai_daily_report::{
    config::AppConfig,
    maintenance::retention,
    run::App,
    snapshots::canonical_json,
    types::{new_run_id, RunContext},
};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{collections::BTreeMap, fs};
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
fn fixture() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: work\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    let app = App::open(config, now()).unwrap();
    (dir, app)
}
fn preview(
    app: &App,
    age: i64,
    reason: Option<&str>,
    payload: serde_json::Value,
) -> (String, std::path::PathBuf) {
    let date = ai_daily_report::types::parse_date("2026-08-01").unwrap();
    let time = now() - chrono::Duration::days(age);
    let id = new_run_id();
    let context = RunContext::new(id.clone(), time, date, "f".repeat(64)).unwrap();
    app.store
        .create(&context, "preview", &id, reason, time)
        .unwrap();
    let snapshot = app.snapshots.put(&id, "input", &payload).unwrap();
    app.store
        .prepare(&id, &snapshot, &snapshot.sha256, &[], time)
        .unwrap();
    app.store.finish_preview(&id,&json!({"schema_version":3,"date":date.to_string(),"window_start_utc":context.window_start_utc.to_rfc3339(),"quality":"complete","sources":[]}),"候选",time).unwrap();
    (id, app.snapshots.root.join(snapshot.path))
}
#[test]
fn cleanup_keeps_unknown_modified_unfinished_and_cross_generation_inputs() {
    let (_dir, app) = fixture();
    let (known, path) = preview(&app, 60, None, json!({"old":true}));
    let (_, unknown) = preview(&app, 60, None, json!({"unknown_neighbor":true}));
    fs::write(unknown.parent().unwrap().join("user-notes.txt"), "用户笔记").unwrap();
    let (_, modified) = preview(&app, 60, None, json!({"modified":true}));
    fs::write(&modified, "人工修改").unwrap();
    let (_, coverage) = preview(
        &app,
        60,
        Some("coverage_candidate:job"),
        json!({"coverage":true}),
    );
    let (parent, parent_file) = preview(&app, 60, None, json!({"frozen_parent":true}));
    preview(
        &app,
        1,
        None,
        json!({"recovery_inputs":{"prior_calls":[{"run_id":parent}]}}),
    );
    let dry = retention::cleanup(&app, now(), false).unwrap();
    assert_eq!(dry.planned_files, 1);
    assert!(path.exists());
    let done = retention::cleanup(&app, now(), true).unwrap();
    assert_eq!(done.deleted_files, 1);
    assert_eq!(done.deleted_runs, 1);
    assert!(!path.exists());
    assert_eq!(app.store.get(&known).unwrap_err().code, "RUN_NOT_FOUND");
    for path in [unknown, modified, coverage, parent_file] {
        assert!(path.exists());
    }
    assert_eq!(
        retention::cleanup(&app, now(), true).unwrap().deleted_files,
        0
    );
}
#[test]
fn current_report_survives_snapshot_expiration_and_maintenance_record_is_idempotent() {
    let (_dir, app) = fixture();
    let (id, path) = preview(&app, 20, None, json!({"expired":true}));
    app.store
        .connection()
        .unwrap()
        .execute(
            "INSERT INTO report_metadata(key,value) VALUES('current_period:weekly:2026-W31',?)",
            [&id],
        )
        .unwrap();
    let done = retention::cleanup(&app, now(), true).unwrap();
    assert_eq!(done.deleted_files, 1);
    assert!(!path.exists());
    assert!(app.store.get(&id).unwrap().snapshot_path.is_none());
    let body = app.store.get(&id).unwrap().markdown.unwrap();
    assert_eq!(body, "候选");
    assert_eq!(
        retention::cleanup(&app, now() + chrono::Duration::days(30), true)
            .unwrap()
            .deleted_runs,
        0
    );
    assert!(!canonical_json(&json!({"body":body})).unwrap().is_empty());
}

#[test]
fn unfinished_backfill_keeps_call_evidence_without_quota_metadata() {
    let (_dir, app) = fixture();
    let (id, path) = preview(&app, 60, Some("backfill:job"), json!({"job":"job"}));
    let conn = app.store.connection().unwrap();
    conn.execute(
        "INSERT INTO report_metadata(key,value) VALUES('backfill_state:job','{}')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,logical_step,attempt,call_date,status,usage_json,started_at) VALUES(?,?,'request','batch_000',1,'2026-08-01','succeeded','{\"job_id\":\"job\"}','2026-08-01T00:00:00Z')",rusqlite::params![id,id]).unwrap();
    assert_eq!(
        retention::cleanup(&app, now(), true).unwrap().deleted_files,
        0
    );
    assert!(path.is_file());
    assert_eq!(app.store.calls(&id).unwrap().len(), 1);
    conn.execute(
        "INSERT INTO report_metadata(key,value) VALUES('backfill_complete:job','{}')",
        [],
    )
    .unwrap();
    assert_eq!(
        retention::cleanup(&app, now(), true).unwrap().deleted_files,
        1
    );
    assert!(!path.exists());
}
