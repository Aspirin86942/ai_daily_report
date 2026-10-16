use ai_daily_report::{
    config::AppConfig,
    model::budget::Reservation,
    snapshots::{fingerprint, SnapshotRef, SnapshotStore},
    store::{revisions::JobBudgetRevision, CallCompletion, CallReservation, Store},
    types::RunContext,
};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{collections::BTreeMap, fs};
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Harness {
    _dir: tempfile::TempDir,
    config: AppConfig,
    store: Store,
    snapshots: SnapshotStore,
}
impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::write(
            dir.path().join("config/settings.yaml"),
            "paths:\n  work_dir: work\nrecovery:\n  enforce_model_budgets: false\n",
        )
        .unwrap();
        let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        let store = Store::open(&dir.path().join("reports.sqlite3"), now()).unwrap();
        let snapshots = SnapshotStore::new(dir.path().join("snapshots"), 8 * 1024 * 1024).unwrap();
        Self {
            _dir: dir,
            config,
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
    fn budget(&self, calls: u64) -> Reservation {
        let mut b = Reservation::new(&self.config.llm, &self.config.recovery, 10, 10, json!({}));
        b.job_id = Some("coverage_test".into());
        b.job_calls_limit = Some(calls);
        b.job_tokens_limit = Some(1_000_000);
        b
    }
    fn reserve(
        &self,
        id: &str,
        request: &SnapshotRef,
        b: &Reservation,
    ) -> Result<i64, ai_daily_report::ReportError> {
        self.store.reserve_call(CallReservation {
            run_id: id,
            fingerprint: &request.sha256,
            logical_step: "batch_000",
            request,
            budget: b,
            recovery: &self.config.recovery,
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
                usage: json!({"total_tokens":100}),
                error_code: None,
                duration_ms: 1,
                now: now(),
            })
            .unwrap();
    }
    fn revision(&self, old: u64, new: u64) -> JobBudgetRevision {
        let rows = self.store.job_calls("coverage_test").unwrap();
        JobBudgetRevision {
            version: 1,
            job_id: "coverage_test".into(),
            plan_sha256: "d".repeat(64),
            previous_calls_limit: old,
            calls_limit: new,
            tokens_limit: 1_000_000,
            cost_limit_microunits: None,
            pricing: None,
            retained_call_ids: rows
                .iter()
                .map(|r| r["call_id"].as_i64().unwrap())
                .collect(),
            retained_calls_sha256: fingerprint(&json!(rows)).unwrap(),
            authorization_note: "测试批准增加调用次数，保留已有消耗".into(),
            previous_revision_sha256: None,
            previous_tokens_limit: None,
        }
    }
}
#[test]
fn job_revisions_keep_calls_and_validate_the_complete_chain() {
    let h = Harness::new();
    let (run, request) = h.prepare(1);
    let initial = h.budget(1);
    let id = h.reserve(&run, &request, &initial).unwrap();
    h.finish(id, &request, "failed");
    assert_eq!(
        h.reserve(&run, &request, &initial).unwrap_err().code,
        "MODEL_CALL_BUDGET_EXHAUSTED"
    );
    let anchor = h.revision(1, 2);
    let first = h.store.register_job_budget_revision(&anchor).unwrap();
    assert_eq!(
        h.store.register_job_budget_revision(&anchor).unwrap(),
        first
    );
    assert_eq!(
        h.reserve(&run, &request, &initial).unwrap_err().code,
        "MODEL_JOB_BUDGET_CHANGED"
    );
    let mut increased = h.budget(2);
    increased.request_audit["job_calls_revision_sha256"] = json!(first);
    let id = h.reserve(&run, &request, &increased).unwrap();
    h.finish(id, &request, "succeeded");
    let mut next = h.revision(2, 4);
    next.version = 2;
    next.previous_revision_sha256 = Some(first);
    next.previous_tokens_limit = Some(1_000_000);
    next.tokens_limit = 2_000_000;
    let second = h.store.register_job_budget_revision(&next).unwrap();
    let mut final_budget = h.budget(4);
    final_budget.job_tokens_limit = Some(2_000_000);
    final_budget.request_audit["job_calls_revision_sha256"] = json!(second);
    let (run2, request2) = h.prepare(2);
    h.reserve(&run2, &request2, &final_budget).unwrap();
    assert_eq!(h.store.job_calls("coverage_test").unwrap().len(), 3);
    h.store
        .connection()
        .unwrap()
        .execute(
            "UPDATE report_model_calls SET error_code='changed' WHERE call_id=1",
            [],
        )
        .unwrap();
    assert_eq!(
        h.reserve(&run2, &request2, &final_budget).unwrap_err().code,
        "MODEL_JOB_BUDGET_CHANGED"
    );
}
#[test]
fn unknown_calls_prevent_revisions_and_continue_consuming_the_job_limit() {
    let h = Harness::new();
    let (run, request) = h.prepare(1);
    let budget = h.budget(1);
    let id = h.reserve(&run, &request, &budget).unwrap();
    h.finish(id, &request, "unknown");
    assert_eq!(
        h.store
            .register_job_budget_revision(&h.revision(1, 2))
            .unwrap_err()
            .code,
        "MODEL_JOB_BUDGET_CHANGED"
    );
    assert_eq!(
        h.reserve(&run, &request, &budget).unwrap_err().code,
        "MODEL_CALL_BUDGET_EXHAUSTED"
    );
}
#[test]
fn concurrent_reservations_obey_explicit_job_budget_with_daily_limits_disabled() {
    let h = Harness::new();
    let prepared: Vec<_> = (1..=8).map(|i| h.prepare(i)).collect();
    let budget = h.budget(4);
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = prepared
            .iter()
            .map(|(run, request)| scope.spawn(|| h.reserve(run, request, &budget)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 4);
    for error in outcomes.into_iter().filter_map(Result::err) {
        assert_eq!(error.code, "MODEL_CALL_BUDGET_EXHAUSTED");
    }
    assert_eq!(h.store.job_calls("coverage_test").unwrap().len(), 4);
}
