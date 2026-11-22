use ai_daily_report::{
    snapshots::SnapshotRef,
    store::{CallCompletion, CallReservation, Store},
    types::{new_run_id, RunContext},
};
use chrono::Utc;
use serde_json::json;
use std::sync::{Arc, Barrier};

#[test]
fn concurrent_calls_and_unknown_results_remain_counted_without_blocking_after_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let store = Store::open(&dir.path().join("reports.sqlite3"), now).unwrap();
    let reference = SnapshotRef {
        path: "input.json".into(),
        sha256: "a".repeat(64),
    };
    let mut ids = Vec::new();
    for _ in 0..8 {
        let id = new_run_id();
        let context = RunContext::new(id.clone(), now, now.date_naive(), "b".repeat(64)).unwrap();
        store.create(&context, "preview", &id, None, now).unwrap();
        store.register_personal_run(&id).unwrap();
        store
            .prepare(&id, &reference, &reference.sha256, &[], now)
            .unwrap();
        ids.push(id);
    }
    let barrier = Arc::new(Barrier::new(ids.len()));
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = ids
            .iter()
            .map(|id| {
                let barrier = Arc::clone(&barrier);
                let store = &store;
                let reference = &reference;
                scope.spawn(move || {
                    barrier.wait();
                    store.reserve_call(CallReservation {
                        run_id: id,
                        fingerprint: &reference.sha256,
                        logical_step: "personal_summary",
                        request: reference,
                        job_id: None,
                        audit: &json!({"fixture":"personal"}),
                        max_attempts: 3,
                        now,
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(results.iter().all(Result::is_ok));
    let calls: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
    for (call, status) in calls
        .iter()
        .zip(["failed", "unknown", "succeeded"].into_iter().cycle())
    {
        if status == "unknown" {
            continue;
        } // Simulate process exit after durable reservation.
        store
            .finish_call(CallCompletion {
                call_id: *call,
                status,
                response: Some(&reference),
                usage: json!({}),
                error_code: if status == "failed" {
                    Some("MODEL_TIMEOUT")
                } else {
                    None
                },
                duration_ms: 1,
                now,
            })
            .unwrap();
    }
    {
        use ai_daily_report::store::locking::ReportLock;
        let lock =
            ReportLock::acquire(&store.path, "personal_work", std::time::Duration::ZERO).unwrap();
        store.recover_interrupted(now, &lock).unwrap();
        assert_eq!(
            store
                .connection()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM report_model_calls WHERE status='unknown'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            3
        );
    }
    for id in &ids {
        store
            .fail(
                id,
                "RUN_INTERRUPTED",
                false,
                now + chrono::Duration::seconds(2),
            )
            .unwrap();
    }
    store
        .compact_personal(now + chrono::Duration::days(31), dir.path())
        .unwrap();
    drop(store);
    let store = Store::open(&dir.path().join("reports.sqlite3"), now).unwrap();
    let summary: String = store
        .connection()
        .unwrap()
        .query_row(
            "SELECT value FROM report_metadata WHERE key=?",
            [format!(
                "personal_summary:personal_work:{}",
                now.date_naive()
            )],
            |r| r.get(0),
        )
        .unwrap();
    let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(summary["runs"], 8);
    assert_eq!(summary["failures"], 8);
    assert_eq!(summary["calls"], 8);
    assert_eq!(summary["elapsed_ms_total"], 16_000);
    assert_eq!(summary["last_run"]["elapsed_ms"], 2_000);
    assert_eq!(
        store
            .personal_calls("personal_work", &now.date_naive().to_string())
            .unwrap(),
        8
    );
    assert_eq!(
        store
            .connection()
            .unwrap()
            .query_row("SELECT count(*) FROM report_model_calls", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let id = new_run_id();
    let context = RunContext::new(id.clone(), now, now.date_naive(), "b".repeat(64)).unwrap();
    store.create(&context, "preview", &id, None, now).unwrap();
    store.register_personal_run(&id).unwrap();
    store
        .prepare(&id, &reference, &reference.sha256, &[], now)
        .unwrap();
    let reserve = || {
        store.reserve_call(CallReservation {
            run_id: &id,
            fingerprint: &reference.sha256,
            logical_step: "personal_summary",
            request: &reference,
            job_id: None,
            audit: &json!({"fixture":"personal"}),
            max_attempts: 3,
            now,
        })
    };
    reserve().unwrap();
    assert_eq!(
        store
            .personal_calls("personal_work", &now.date_naive().to_string())
            .unwrap(),
        9
    );
    assert_eq!(store.calls(&id).unwrap().len(), 1);
}
