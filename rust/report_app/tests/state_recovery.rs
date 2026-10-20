use ai_daily_report::{
    snapshots::{canonical_json, digest, SnapshotRef, SnapshotStore},
    store::{locking::ReportLock, Store},
    types::RunContext,
};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::params;
use serde_json::{json, Value};
use std::{fs, time::Duration};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-12T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
fn context() -> RunContext {
    serde_json::from_str(include_str!("fixtures/context-v2.json")).unwrap()
}

#[test]
fn canonical_json_matches_frozen_python_bytes_and_hashes() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/canonical-json.json")).unwrap();
    for case in cases {
        let actual = canonical_json(&case["input"]).unwrap();
        assert_eq!(actual, case["utf8"].as_str().unwrap().as_bytes());
        assert_eq!(digest(&actual), case["sha256"]);
    }
    assert_eq!(
        String::from_utf8(
            canonical_json(&json!({"small":1e-7,"large":1e20,"negative_zero":-0.0})).unwrap()
        )
        .unwrap(),
        "{\"large\":1e+20,\"negative_zero\":-0.0,\"small\":1e-07}"
    );
}

#[test]
fn snapshots_are_immutable_bounded_and_keep_original_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let snapshots = SnapshotStore::new(directory.path().to_owned(), 1024).unwrap();
    let bytes = "{ \"中文\" : null }\n".as_bytes();
    let reference = snapshots
        .put_bytes(&"a".repeat(32), "response", bytes)
        .unwrap();
    assert_eq!(snapshots.read_bytes(&reference).unwrap(), bytes);
    assert_eq!(
        snapshots
            .put_bytes(&"a".repeat(32), "response", bytes)
            .unwrap(),
        reference
    );
    fs::write(directory.path().join(&reference.path), b"{}").unwrap();
    assert_eq!(
        snapshots.read(&reference).unwrap_err().code,
        "SNAPSHOT_HASH_MISMATCH"
    );
    let outside = SnapshotRef {
        path: "../other.json".into(),
        sha256: "a".repeat(64),
    };
    assert_eq!(
        snapshots.read(&outside).unwrap_err().code,
        "SNAPSHOT_PATH_INVALID"
    );
    let limited = SnapshotStore::new(directory.path().join("small"), 3).unwrap();
    assert_eq!(
        limited
            .put(&"a".repeat(32), "input", &json!({"text":"large"}))
            .unwrap_err()
            .code,
        "SNAPSHOT_CAPACITY_EXCEEDED"
    );
}

#[test]
fn existing_lock_file_is_reusable_but_an_active_owner_excludes_other_runs() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reports.sqlite3");
    let first = ReportLock::acquire(&path, "personal_work", Duration::ZERO).unwrap();
    assert!(
        matches!(ReportLock::acquire(&path, "personal_work", Duration::ZERO), Err(error) if error.code == "REPORT_ALREADY_RUNNING")
    );
    drop(first);
    assert!(ReportLock::acquire(&path, "personal_work", Duration::ZERO).is_ok());
}

#[test]
fn current_schema_opens_and_unknown_schema_is_rejected_without_alteration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reports.sqlite3");
    let store = Store::open(&path, now()).unwrap();
    let conn = store.connection().unwrap();
    conn.execute("INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json) VALUES ('2026-10-01','old text','','','{}')", []).unwrap();
    drop(conn);
    assert!(Store::open(&path, now()).is_ok());
    store
        .connection()
        .unwrap()
        .execute("ALTER TABLE report_runs ADD COLUMN future_version TEXT", [])
        .unwrap();
    let before = digest(&fs::read(&path).unwrap());
    assert_eq!(
        Store::open(&path, now()).unwrap_err().code,
        "REPORT_SCHEMA_UNSUPPORTED"
    );
    assert_eq!(digest(&fs::read(&path).unwrap()), before);
}

#[test]
fn frozen_inputs_cannot_be_replaced_and_failed_calls_remain_auditable() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("reports.sqlite3"), now()).unwrap();
    let snapshots = SnapshotStore::new(directory.path().join("snapshots"), 1024 * 1024).unwrap();
    let context = context();
    store
        .create(&context, "report", "generation", None, now())
        .unwrap();
    store.set_scanning(&context.run_id, now()).unwrap();
    let input = snapshots
        .put(&context.run_id, "input", &json!({"version":2}))
        .unwrap();
    store
        .prepare(&context.run_id, &input, "fingerprint", &[], now())
        .unwrap();
    assert_eq!(
        store
            .prepare(&context.run_id, &input, "different", &[], now())
            .unwrap_err()
            .code,
        "RUN_INPUT_ALREADY_FROZEN"
    );
    let conn = store.connection().unwrap();
    for (attempt, status) in [(1, "succeeded"), (2, "reserved")] {
        conn.execute("INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,logical_step,attempt,call_date,status,started_at,usage_json) VALUES (?,'generation','request','batch',?,'2026-10-12',?,'2026-10-12T01:00:00+00:00','{\"total_tokens\":160}')", params![context.run_id,attempt,status]).unwrap();
    }
    drop(conn);
    store
        .fail(&context.run_id, "RUN_CANCELLED", true, now())
        .unwrap();
    let resumed = store.resume(&context.run_id, now()).unwrap();
    assert_eq!(resumed.generation_id, "generation");
    assert_eq!(resumed.input_fingerprint.as_deref(), Some("fingerprint"));
    let calls = store.calls(&context.run_id).unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["status"], "succeeded");
    assert_eq!(calls[1]["status"], "unknown");
    assert_eq!(calls[0]["usage_json"], "{\"total_tokens\":160}");
    assert_eq!(
        store
            .successful_call("generation", "request")
            .unwrap()
            .unwrap()["call_id"],
        calls[0]["call_id"]
    );
}

#[test]
fn startup_recovery_preserves_prepared_runs_and_marks_early_interruptions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reports.sqlite3");
    let lock = ReportLock::acquire(&path, "personal_work", Duration::ZERO).unwrap();
    let store = Store::open_locked(&path, now(), &lock).unwrap();
    let context = context();
    store
        .create(&context, "report", "generation", None, now())
        .unwrap();
    store.recover_interrupted(now(), &lock).unwrap();
    let record = store.get(&context.run_id).unwrap();
    assert_eq!(record.error_code.as_deref(), Some("RUN_INTERRUPTED"));
    assert!(store.resume(&context.run_id, now()).is_err());
}

#[test]
fn staged_body_is_durable_before_history_pointer_moves() {
    use ai_daily_report::store::StagedReport;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("reports.sqlite3"), now()).unwrap();
    let context = context();
    let report: Value = serde_json::from_str(include_str!("fixtures/daily-v2.json")).unwrap();
    let markdown = include_str!("fixtures/daily-v2.md");
    let snapshots = SnapshotStore::new(directory.path().join("snapshots"), 1024 * 1024).unwrap();
    store
        .create(&context, "report", "generation", None, now())
        .unwrap();
    let reference = snapshots
        .put(&context.run_id, "input", &json!({"version":2}))
        .unwrap();
    store
        .prepare(&context.run_id, &reference, "input", &[], now())
        .unwrap();
    store
        .stage_report(
            &context.run_id,
            &StagedReport {
                report: &report,
                markdown,
                output_path: "report.md",
                publication_fingerprint: "publication",
            },
            now(),
        )
        .unwrap();
    assert!(store.current("2026-10-11").unwrap().is_none());
    assert_eq!(
        store.get(&context.run_id).unwrap().markdown.as_deref(),
        Some(markdown)
    );
    assert_eq!(store.pending(None).unwrap().len(), 1);
    assert_eq!(
        store
            .mark_published(&context.run_id, &"0".repeat(64), now())
            .unwrap_err()
            .code,
        "PUBLICATION_NOT_VERIFIED"
    );
    store
        .mark_published(&context.run_id, &digest(markdown.as_bytes()), now())
        .unwrap();
    assert_eq!(
        store.current("2026-10-11").unwrap().unwrap().run_id,
        context.run_id
    );
    assert!(store.pending(None).unwrap().is_empty());
    assert_eq!(store.list_reports().unwrap(), ["2026-10-11"]);
    assert_eq!(
        store
            .reports_in_range(context.target_date, context.target_date)
            .unwrap()
            .0[0],
        report
    );
}

#[test]
fn activated_history_reads_mixed_v2_v3_v4_and_lists_natural_missing_days() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("reports.sqlite3"), now()).unwrap();
    let v4: Value =
        serde_json::from_str(include_str!("fixtures/readability/v4-input.json")).unwrap();
    assert_eq!(v4["schema_version"].as_u64(), Some(4));
    let conn = store.connection().unwrap();
    for (date, report) in [
        (
            "2026-10-10",
            json!({"schema_version": 2, "date": "2026-10-10"}),
        ),
        (
            "2026-10-11",
            json!({"schema_version": 3, "date": "2026-10-11"}),
        ),
        ("2026-10-17", v4.clone()),
    ] {
        conn.execute(
            "INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json) VALUES (?,'','','',?)",
            params![date, report.to_string()],
        )
        .unwrap();
    }
    drop(conn);
    let first = NaiveDate::from_ymd_opt(2026, 10, 10).unwrap();
    store.activate(first).unwrap();
    let (reports, _) = store
        .reports_in_range(first, NaiveDate::from_ymd_opt(2026, 10, 19).unwrap())
        .unwrap();
    assert_eq!(reports.len(), 3);
    assert_eq!(reports[2]["date"], "2026-10-17");
    assert_eq!(
        reports[2]["content"]["overview"][0]["text"],
        "采购审批方案(新)集中说明现状与建议，保留金额变化+38909%。"
    );
    let missing = store
        .missed_dates(NaiveDate::from_ymd_opt(2026, 10, 20).unwrap())
        .unwrap();
    assert_eq!(
        missing.join("、"),
        "2026-10-12、2026-10-13、2026-10-14、2026-10-15、2026-10-16、2026-10-18、2026-10-19"
    );
}

#[test]
fn unknown_history_schema_versions_remain_rejected() {
    for (version, expected) in [
        (0, "REPORT_SCHEMA_UNSUPPORTED"),
        (6, "REPORT_SCHEMA_UNSUPPORTED"),
        (5, "JSON_INVALID"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("reports.sqlite3"), now()).unwrap();
        store
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json) VALUES ('2026-10-17','','','',?)",
                [json!({"schema_version": version, "date": "2026-10-17"}).to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .reports_in_range(
                    NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
                    NaiveDate::from_ymd_opt(2026, 10, 19).unwrap(),
                )
                .unwrap_err()
                .code,
            expected
        );
    }
}

#[test]
fn v4_history_report_date_must_match_its_index_row() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("reports.sqlite3"), now()).unwrap();
    store
        .connection()
        .unwrap()
        .execute(
            "INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json) VALUES ('2026-10-17','','','',?)",
            [json!({"schema_version": 4, "date": "2026-10-16"}).to_string()],
        )
        .unwrap();
    assert_eq!(
        store
            .reports_in_range(
                NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
                NaiveDate::from_ymd_opt(2026, 10, 19).unwrap(),
            )
            .unwrap_err()
            .code,
        "REPORT_CONTEXT_MISMATCH"
    );
}

#[test]
fn preview_retains_body_and_never_creates_a_formal_history_row() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("reports.sqlite3"), now()).unwrap();
    let context = context();
    store
        .create(&context, "preview", "generation", None, now())
        .unwrap();
    let report: Value = serde_json::from_str(include_str!("fixtures/daily-v2.json")).unwrap();
    let markdown = include_str!("fixtures/daily-v2.md");
    store
        .finish_preview(&context.run_id, &report, markdown, now())
        .unwrap();
    assert!(store.list_reports().unwrap().is_empty());
    assert_eq!(
        store
            .get(&context.run_id)
            .unwrap()
            .expected_sha256
            .as_deref(),
        Some(digest(markdown.as_bytes()).as_str())
    );
}
