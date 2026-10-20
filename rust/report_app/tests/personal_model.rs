use ai_daily_report::{
    config::AppConfig,
    model::budget::Reservation,
    snapshots::SnapshotRef,
    store::{CallCompletion, CallReservation, Store},
    types::{new_run_id, RunContext},
};
use chrono::Utc;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Barrier},
};

#[test]
fn concurrent_reservations_unknown_results_and_compaction_never_refund_date_budget() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: work\nrecovery:\n  enforce_model_budgets: false\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
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
                let config = &config;
                let reference = &reference;
                scope.spawn(move || {
                    let budget = Reservation::new(&config.llm, &config.recovery, 10, 10, json!({}));
                    barrier.wait();
                    store.reserve_call(CallReservation {
                        run_id: id,
                        fingerprint: &reference.sha256,
                        logical_step: "personal_summary",
                        request: reference,
                        budget: &budget,
                        recovery: &config.recovery,
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
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
    assert!(results
        .iter()
        .filter_map(|r| r.as_ref().err())
        .all(|e| e.code == "PERSONAL_CALL_LIMIT"));
    let calls: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
    for (call, status) in calls.iter().zip(["failed", "unknown", "succeeded"]) {
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
            1
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
    assert_eq!(summary["calls"], 3);
    assert_eq!(summary["elapsed_ms_total"], 16_000);
    assert_eq!(summary["last_run"]["elapsed_ms"], 2_000);
    assert_eq!(
        store
            .personal_calls("personal_work", &now.date_naive().to_string())
            .unwrap(),
        3
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
    let budget = Reservation::new(&config.llm, &config.recovery, 10, 10, json!({}));
    let reserve = || {
        store.reserve_call(CallReservation {
            run_id: &id,
            fingerprint: &reference.sha256,
            logical_step: "personal_summary",
            request: &reference,
            budget: &budget,
            recovery: &config.recovery,
            max_attempts: 3,
            now,
        })
    };
    assert_eq!(reserve().unwrap_err().code, "PERSONAL_CALL_LIMIT");
    assert!(store.calls(&id).unwrap().is_empty());
}
