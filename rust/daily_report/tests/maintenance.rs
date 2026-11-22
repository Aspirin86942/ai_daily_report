use ai_daily_report::{
    snapshots::{SnapshotRef, SnapshotStore},
    store::{CallCompletion, CallReservation, Store},
    types::RunContext,
};
use chrono::{DateTime, Utc};
use serde_json::json;
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Harness {
    _dir: tempfile::TempDir,
    store: Store,
    snapshots: SnapshotStore,
}
impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("reports.sqlite3"), now()).unwrap();
        let snapshots = SnapshotStore::new(dir.path().join("snapshots"), 8 * 1024 * 1024).unwrap();
        Self {
            _dir: dir,
            store,
            snapshots,
        }
    }
    fn prepare(&self, n: u32) -> (String, SnapshotRef) {
        let id = format!("{n:032x}");
        let context =
            RunContext::new(id.clone(), now(), now().date_naive(), "c".repeat(64)).unwrap();
        self.store
            .create(&context, "preview", &id, None, now())
            .unwrap();
        let snapshot = self.snapshots.put(&id, "input", &json!({"n":n})).unwrap();
        self.store
            .prepare(&id, &snapshot, &snapshot.sha256, &[], now())
            .unwrap();
        (id, snapshot)
    }
    fn reserve(
        &self,
        id: &str,
        request: &SnapshotRef,
    ) -> Result<i64, ai_daily_report::ReportError> {
        self.store.reserve_call(CallReservation {
            run_id: id,
            fingerprint: &request.sha256,
            logical_step: "batch_000",
            request,
            job_id: Some("coverage_test"),
            audit: &json!({"plan_sha256":"d".repeat(64)}),
            max_attempts: 3,
            now: now(),
        })
    }
    fn finish(&self, id: i64, request: &SnapshotRef, status: &str) {
        self.store
            .finish_call(CallCompletion {
                call_id: id,
                status,
                response: Some(request),
                usage: json!({"total_tokens":100,"job_id":"forged","request_audit":{"forged":true},"reserved_budget":{"forged":true},"budget_limits_enforced":true}),
                error_code: None,
                duration_ms: 1,
                now: now(),
            })
            .unwrap();
    }
}
#[test]
fn calls_preserve_audit_and_reported_usage_without_cumulative_limits() {
    let h = Harness::new();
    let (run, request) = h.prepare(1);
    let id = h.reserve(&run, &request).unwrap();
    h.finish(id, &request, "unknown");
    h.reserve(&run, &request).unwrap();
    h.reserve(&run, &request).unwrap();
    assert_eq!(
        h.reserve(&run, &request).unwrap_err().code,
        "MODEL_STEP_ATTEMPTS_EXHAUSTED"
    );
    let (next, request) = h.prepare(2);
    h.reserve(&next, &request).unwrap();
    let rows = h.store.job_calls("coverage_test").unwrap();
    assert_eq!(rows.len(), 4);
    let usage: serde_json::Value =
        serde_json::from_str(rows[0]["usage_json"].as_str().unwrap()).unwrap();
    assert_eq!(usage["total_tokens"], 100);
    assert_eq!(usage["request_audit"]["plan_sha256"], "d".repeat(64));
    assert_eq!(usage["job_id"], "coverage_test");
    assert!(usage.get("budget_limits_enforced").is_none());
    assert!(usage.get("reserved_budget").is_none());
}
#[test]
fn concurrent_calls_ignore_legacy_job_quota_but_completed_jobs_stay_closed() {
    let h = Harness::new();
    h.store
        .connection()
        .unwrap()
        .execute(
            "INSERT INTO report_metadata(key,value) VALUES(?,?)",
            [
                "backfill_budget:coverage_test",
                r#"{"baseline_call_id":0,"calls_limit":0}"#,
            ],
        )
        .unwrap();
    let prepared: Vec<_> = (1..=8).map(|i| h.prepare(i)).collect();
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = prepared
            .iter()
            .map(|(run, request)| scope.spawn(|| h.reserve(run, request)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(outcomes.iter().all(Result::is_ok));
    assert_eq!(h.store.job_calls("coverage_test").unwrap().len(), 8);
    h.store
        .connection()
        .unwrap()
        .execute(
            "INSERT INTO report_metadata(key,value) VALUES('backfill_complete:coverage_test','{}')",
            [],
        )
        .unwrap();
    let (run, request) = h.prepare(9);
    assert_eq!(
        h.reserve(&run, &request).unwrap_err().code,
        "BACKFILL_ALREADY_COMPLETE"
    );
}
#[test]
fn legacy_job_snapshots_and_call_audits_remain_readable() {
    use ai_daily_report::model::recovery::JobContext;
    for cost in [json!(0), json!(null)] {
        let retained = json!({"job_id":"coverage_test","tokens_limit":1,"calls_limit":0,"cost_limit_microunits":cost,"revision_sha256":null});
        let job: JobContext = serde_json::from_value(retained.clone()).unwrap();
        assert_eq!(serde_json::to_value(job).unwrap(), retained);
    }
    assert_eq!(
        serde_json::to_value(JobContext::new("coverage_test".into())).unwrap(),
        json!({"job_id":"coverage_test"})
    );
    let h = Harness::new();
    let (run, request) = h.prepare(1);
    let id = h.reserve(&run, &request).unwrap();
    let legacy=json!({"reserved_budget":{"job_id":"coverage_test","request_audit":{"old":true},"job_calls_limit":0}}).to_string();
    h.store
        .connection()
        .unwrap()
        .execute(
            "UPDATE report_model_calls SET usage_json=? WHERE call_id=?",
            rusqlite::params![legacy, id],
        )
        .unwrap();
    assert_eq!(
        h.store.job_calls("coverage_test").unwrap()[0]["usage_json"],
        legacy
    );
}
