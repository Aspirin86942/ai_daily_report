use ai_daily_report::{
    config::AppConfig,
    model::{
        budget::{charged, Reservation},
        contracts::{ComposedReport, FactsResponse, SelectedFact},
        plan::{self, DailyPlan},
        slicing::{union_chars, InputUnit, SourceInput},
        transport::{HttpResponse, HttpTransport, ModelTransport, TransportFailure},
        ModelRunner,
    },
    snapshots::{SnapshotRef, SnapshotStore},
    store::Store,
    types::RunContext,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::Mutex,
    time::{Duration, Instant},
};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-12T15:59:59Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Harness {
    _dir: tempfile::TempDir,
    config: AppConfig,
    store: Store,
    snapshots: SnapshotStore,
    context: RunContext,
}
impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::write(dir.path().join("config/settings.yaml"),"paths:\n  work_dir: work\nllm:\n  max_request_utf8_bytes: 64000\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\n  max_tokens: 4096\nrecovery:\n  enforce_model_budgets: false\n").unwrap();
        let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        let store = Store::open(&dir.path().join("reports.sqlite3"), now()).unwrap();
        let snapshots = SnapshotStore::new(dir.path().join("snapshots"), 8 * 1024 * 1024).unwrap();
        let context: RunContext =
            serde_json::from_str(include_str!("fixtures/context-v2.json")).unwrap();
        store
            .create(&context, "preview", &"a".repeat(32), None, now())
            .unwrap();
        Self {
            _dir: dir,
            config,
            store,
            snapshots,
            context,
        }
    }
    fn plan(&self, sources: &[SourceInput]) -> DailyPlan {
        let plan = plan::prepare(
            &self.config.llm,
            json!({"date":"2026-10-11"}),
            sources,
            json!({"available":sources.len()}),
        )
        .unwrap();
        let snapshot = self
            .snapshots
            .put(
                &self.context.run_id,
                "input",
                &serde_json::to_value(&plan).unwrap(),
            )
            .unwrap();
        self.store
            .prepare(
                &self.context.run_id,
                &snapshot,
                &snapshot.sha256,
                &[],
                now(),
            )
            .unwrap();
        plan
    }
    fn runner<'a>(&'a self, transport: &'a dyn ModelTransport) -> ModelRunner<'a> {
        let started = Instant::now();
        ModelRunner {
            store: &self.store,
            snapshots: &self.snapshots,
            transport,
            llm: &self.config.llm,
            recovery: &self.config.recovery,
            recovery_inputs: None,
            allow_new_calls: true,
            run_id: &self.context.run_id,
            started_at: now(),
            started,
            deadline: started + Duration::from_secs(15),
        }
    }
    fn calls(&self) -> Vec<Value> {
        self.store.calls(&self.context.run_id).unwrap()
    }
}
fn source(id: &str, text: &str) -> SourceInput {
    SourceInput {
        source_id: id.into(),
        source_kind: "md".into(),
        path: format!("{id}.md"),
        text: text.into(),
        units: vec![],
        history_priority: false,
        text_unverified: false,
    }
}
fn fact(id: &str) -> Value {
    json!({"topic":"采购审批调整","background":"减少重复审核","detail":"拟议采购额超过 5 万元时增加部门负责人审批。","conditions":["恰好 5 万元不触发新增审批","紧急采购仍需事后补签"],"status":"方案待确认，尚未生效","source_ids":[id],"uncertainty":null})
}
fn facts(id: &str) -> Value {
    json!({"schema_version":3,"facts":[fact(id)],"pending":[]})
}
fn composed() -> Value {
    let cited = json!({"text":"采购审批方案待确认：超过 5 万元增加部门负责人审批；恰好 5 万元不触发，紧急采购仍需事后补签。","fact_ids":["batch_000:000"],"source_ids":["s1"]});
    json!({"schema_version":3,"overview":[cited],"matters":[{"title":"采购审批调整","background":null,"details":[cited],"status":null}],"pending":[]})
}
fn composed_for(facts: &[SelectedFact]) -> Value {
    let cited = |fact: &SelectedFact| json!({"text":fact.fact.detail,"fact_ids":[fact.fact_id],"source_ids":fact.fact.source_ids});
    let details: Vec<_> = facts
        .iter()
        .filter(|fact| !fact.pending)
        .map(cited)
        .collect();
    let pending: Vec<_> = facts
        .iter()
        .filter(|fact| fact.pending)
        .map(cited)
        .collect();
    json!({"schema_version":3,"overview":details.iter().take(1).collect::<Vec<_>>(),"matters":if details.is_empty() { vec![] } else { vec![json!({"title":"采购审批调整","background":null,"details":details,"status":null})] },"pending":pending})
}
fn truncated() -> HttpResponse {
    HttpResponse {
        status: 200,
        retry_after: None,
        body: serde_json::to_vec(
            &json!({"choices":[{"finish_reason":"length","message":{"content":"{"}}]}),
        )
        .unwrap(),
    }
}
fn response(content: Value) -> HttpResponse {
    HttpResponse{status:200,retry_after:None,body:serde_json::to_vec_pretty(&json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}],"usage":{"prompt_tokens":100,"completion_tokens":80,"total_tokens":180}})).unwrap()}
}
struct Script {
    responses: Mutex<VecDeque<Result<HttpResponse, TransportFailure>>>,
    requests: Mutex<Vec<Vec<u8>>>,
}
impl Script {
    fn new(responses: Vec<Result<HttpResponse, TransportFailure>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl ModelTransport for Script {
    fn send(&self, request: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        self.requests.lock().unwrap().push(request.to_vec());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model call")
    }
}

#[test]
fn facts_plus_one_composition_are_audited_and_replay_makes_zero_requests() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "采购审批拟调整，超过五万元增加审批。")]);
    let raw = response(facts("s1"));
    let expected = raw.body.clone();
    let network = Script::new(vec![Ok(raw), Ok(response(composed()))]);
    let first = h.runner(&network).generate(&plan).unwrap();
    assert_eq!(first.steps.len(), 2);
    assert_eq!(network.count(), 2);
    let calls = h.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1]["logical_step"], "compose_000");
    assert_eq!(
        network.requests.lock().unwrap()[0],
        plan.batches[0].bytes().unwrap()
    );
    let reference = SnapshotRef {
        path: calls[0]["response_path"].as_str().unwrap().into(),
        sha256: calls[0]["response_sha256"].as_str().unwrap().into(),
    };
    assert_eq!(h.snapshots.read_bytes(&reference).unwrap(), expected);
    let replay = h.runner(&network).generate(&plan).unwrap();
    assert!(replay.steps.iter().all(|s| s.reused));
    assert_eq!(network.count(), 2);
    assert_eq!(h.calls().len(), 2);
}

#[test]
fn parallel_facts_finish_out_of_order_but_composition_is_stable_and_bounded() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Parallel {
        active: AtomicUsize,
        peak: AtomicUsize,
        entered: AtomicUsize,
        second_finished: AtomicBool,
    }
    impl ModelTransport for Parallel {
        fn send(&self, bytes: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            let input: Value =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            if let Some(facts) = input["facts"].as_array() {
                assert_eq!(self.active.load(Ordering::SeqCst), 0);
                assert_eq!(facts[0]["fact"]["source_ids"][0], "s1");
                assert_eq!(facts[1]["fact"]["source_ids"][0], "s2");
                let cited = json!({"text":"两份材料提出审批调整。","fact_ids":["batch_000:000","batch_001:000"],"source_ids":["s1","s2"]});
                return Ok(response(
                    json!({"schema_version":3,"overview":[cited],"matters":[{"title":"审批调整","background":null,"details":[cited],"status":null}],"pending":[]}),
                ));
            }
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let id = input["sources"][0]["source_id"].as_str().unwrap();
            self.entered.fetch_add(1, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while self.entered.load(Ordering::SeqCst) < 2
                || (id == "s1" && !self.second_finished.load(Ordering::SeqCst))
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fact requests did not overlap"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if id == "s2" {
                self.second_finished.store(true, Ordering::SeqCst);
            }
            Ok(response(facts(id)))
        }
    }
    let mut h = Harness::new();
    h.config.llm.max_sources_per_batch = 1;
    let plan = h.plan(&[
        source("s1", "第一份审批提案。"),
        source("s2", "第二份审批提案。"),
    ]);
    assert_eq!(plan.batches.len(), 2);
    let network = Parallel {
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        entered: AtomicUsize::new(0),
        second_finished: AtomicBool::new(false),
    };
    let result = h.runner(&network).generate_with_workers(&plan, 2).unwrap();
    assert_eq!(result.steps.len(), 3);
    assert_eq!(network.peak.load(Ordering::SeqCst), 2);
    assert_eq!(h.calls().len(), 3);
}

#[test]
fn composition_failure_reuses_the_successful_fact_batch() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "方案未生效。")]);
    let network = Script::new(vec![
        Ok(response(facts("s1"))),
        Ok(HttpResponse {
            status: 400,
            retry_after: None,
            body: b"{}".to_vec(),
        }),
        Ok(response(composed())),
    ]);
    assert_eq!(
        h.runner(&network).generate(&plan).unwrap_err().code,
        "MODEL_HTTP_ERROR"
    );
    let completed = h.runner(&network).generate(&plan).unwrap();
    assert!(completed.steps[0].reused);
    assert!(!completed.steps[1].reused);
    assert_eq!(network.count(), 3);
    assert_eq!(
        h.calls()
            .iter()
            .filter(|c| c["logical_step"] == "batch_000")
            .count(),
        1
    );
}

#[test]
fn rate_limit_and_unknown_connection_are_retried_once_by_the_application() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "材料。")]);
    let network = Script::new(vec![
        Ok(HttpResponse {
            status: 429,
            retry_after: Some("0".into()),
            body: b"{}".to_vec(),
        }),
        Err(TransportFailure::Connection),
        Ok(response(facts("s1"))),
    ]);
    h.runner(&network).execute(&plan.batches[0]).unwrap();
    let calls = h.calls();
    assert_eq!(
        calls
            .iter()
            .map(|v| v["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["failed", "unknown", "succeeded"]
    );
    assert_eq!(calls[2]["attempt"], 3);
    assert_eq!(network.count(), 3);
    let reservation: Reservation = serde_json::from_value(
        serde_json::from_str::<Value>(calls[1]["usage_json"].as_str().unwrap()).unwrap()
            ["reserved_budget"]
            .clone(),
    )
    .unwrap();
    assert!(reservation.total() > 0);
}

#[test]
fn permanent_and_invalid_output_errors_do_not_retry() {
    for (reply, code) in [
        (
            HttpResponse {
                status: 401,
                retry_after: None,
                body: b"{}".to_vec(),
            },
            "MODEL_HTTP_ERROR",
        ),
        (response(facts("unknown-source")), "MODEL_RESPONSE_INVALID"),
        (
            response(json!({"schema_version":999,"facts":[],"pending":[]})),
            "MODEL_RESPONSE_INVALID",
        ),
    ] {
        let h = Harness::new();
        let plan = h.plan(&[source("s1", "材料。")]);
        let network = Script::new(vec![Ok(reply)]);
        assert_eq!(
            h.runner(&network)
                .execute(&plan.batches[0])
                .unwrap_err()
                .code,
            code
        );
        assert_eq!(network.count(), 1);
        assert_eq!(h.calls()[0]["status"], "failed");
    }
}

#[test]
fn unknown_results_keep_reservations_and_total_attempt_limit_survives_resume() {
    let mut h = Harness::new();
    h.config.llm.max_retries = 1;
    let plan = h.plan(&[source("s1", "材料。")]);
    let network = Script::new(vec![Err(TransportFailure::Timeout)]);
    assert_eq!(
        h.runner(&network)
            .execute(&plan.batches[0])
            .unwrap_err()
            .code,
        "MODEL_TIMEOUT"
    );
    h.store
        .fail(&h.context.run_id, "MODEL_TIMEOUT", false, now())
        .unwrap();
    h.store
        .resume(&h.context.run_id, now() + chrono::Duration::days(1))
        .unwrap();
    assert_eq!(
        h.runner(&network)
            .execute(&plan.batches[0])
            .unwrap_err()
            .code,
        "MODEL_STEP_BUDGET_EXHAUSTED"
    );
    assert_eq!(network.count(), 1);
    assert_eq!(h.calls()[0]["status"], "unknown");
    let usage: Value = serde_json::from_str(h.calls()[0]["usage_json"].as_str().unwrap()).unwrap();
    let budget: Reservation = serde_json::from_value(usage["reserved_budget"].clone()).unwrap();
    assert_eq!(
        charged("unknown", &budget, &json!({"total_tokens":1})).0,
        budget.total()
    );
}

#[test]
fn response_persistence_failure_never_resends_the_request() {
    let mut h = Harness::new();
    let plan = h.plan(&[source("s1", "材料。")]);
    h.snapshots = SnapshotStore::new(h._dir.path().join("snapshots"), 80_000).unwrap();
    let network = Script::new(vec![Ok(HttpResponse {
        status: 200,
        retry_after: None,
        body: vec![b' '; 100_000],
    })]);
    assert_eq!(
        h.runner(&network)
            .execute(&plan.batches[0])
            .unwrap_err()
            .code,
        "SNAPSHOT_CAPACITY_EXCEEDED"
    );
    assert_eq!(network.count(), 1);
    assert_eq!(h.calls()[0]["status"], "unknown");
}

#[test]
fn corrupted_successful_response_is_rejected_without_network() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "材料。")]);
    let network = Script::new(vec![Ok(response(facts("s1")))]);
    h.runner(&network).execute(&plan.batches[0]).unwrap();
    let calls = h.calls();
    fs::write(
        h._dir
            .path()
            .join("snapshots")
            .join(calls[0]["response_path"].as_str().unwrap()),
        b"{}",
    )
    .unwrap();
    assert_eq!(
        h.runner(&network)
            .execute(&plan.batches[0])
            .unwrap_err()
            .code,
        "SNAPSHOT_HASH_MISMATCH"
    );
    assert_eq!(network.count(), 1);
}

#[test]
fn truncation_splits_all_input_and_replay_reuses_the_complete_tree() {
    let h = Harness::new();
    let text = "采购规则的适用条件及例外尚待确认。\n".repeat(600);
    let plan = h.plan(&[source("s1", &text)]);
    let truncated = HttpResponse {
        status: 200,
        retry_after: None,
        body: serde_json::to_vec(
            &json!({"choices":[{"finish_reason":"length","message":{"content":"{"}}]}),
        )
        .unwrap(),
    };
    let network = Script::new(vec![
        Ok(truncated),
        Ok(response(facts("s1"))),
        Ok(response(facts("s1"))),
    ]);
    let results = h
        .runner(&network)
        .execute_complete(&plan.batches[0])
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].effective.logical_step, "batch_000.0");
    assert_eq!(results[1].effective.logical_step, "batch_000.1");
    let ranges: Vec<_> = results
        .iter()
        .flat_map(|r| &r.effective.entries)
        .flat_map(|e| e.text_ranges.clone())
        .collect();
    let original: Vec<_> = plan.batches[0]
        .entries
        .iter()
        .flat_map(|e| e.text_ranges.clone())
        .collect();
    assert_eq!(union_chars(&ranges), union_chars(&original));
    let replay = h
        .runner(&network)
        .execute_complete(&plan.batches[0])
        .unwrap();
    assert!(replay.iter().all(|result| result.reused));
    assert_eq!(network.count(), 3);
    assert_eq!(h.calls()[1]["attempt"], 1);
}

#[test]
fn extraction_capacity_is_independent_of_batch_count_and_composition() {
    let mut h = Harness::new();
    h.config.llm.max_request_utf8_bytes = 10000;
    h.config.llm.max_sources_per_batch = 1;
    let sources = (0..8)
        .map(|n| source(&format!("s{n}"), "完整事实。"))
        .collect::<Vec<_>>();
    let planned = plan::prepare(&h.config.llm, json!({}), &sources, json!({})).unwrap();
    assert_eq!(planned.batches.len(), 8);
    assert!(
        planned
            .batches
            .iter()
            .all(|step| step.request["max_tokens"] == 8192
                && step.output_capacity_bytes == 128 * 1024)
    );
    assert!(h.calls().is_empty());
    h.config.llm.max_request_utf8_bytes = 64000;
    let plan = h.plan(&sources[..1]);
    let mut step = plan.batches[0].clone();
    step.prompt_sha256 = "f".repeat(64);
    assert_eq!(
        step.validate(&h.config.llm).unwrap_err().code,
        "MODEL_PLAN_IDENTITY_MISMATCH"
    );
}

#[test]
fn facts_use_16384_tokens_and_large_reservations_remain_valid() {
    let mut h = Harness::new();
    h.config.llm.verified_max_output_tokens = Some(32768);
    let plan = h.plan(&[source("s1", "材料。")]);
    assert_eq!(plan.batches[0].request["max_tokens"], 16384);
    let reservation = Reservation::new(
        &h.config.llm,
        &h.config.recovery,
        h.config.llm.max_request_utf8_bytes,
        16384,
        json!({}),
    );
    reservation.validate().unwrap();
}

#[test]
fn split_recovery_in_a_new_generation_reuses_parent_failure_and_successful_child() {
    use ai_daily_report::model::recovery::{FrozenCall, RecoveryInputs};
    let h = Harness::new();
    let plan = h.plan(&[source("s1", &"采购审批完整条件。\n".repeat(100))]);
    let network = Script::new(vec![
        Ok(truncated()),
        Ok(response(facts("s1"))),
        Ok(HttpResponse {
            status: 400,
            retry_after: None,
            body: vec![],
        }),
    ]);
    assert_eq!(
        h.runner(&network)
            .execute_complete(&plan.batches[0])
            .unwrap_err()
            .code,
        "MODEL_HTTP_ERROR"
    );
    let parent = h.store.get(&h.context.run_id).unwrap();
    let mut recovery = RecoveryInputs::default();
    for row in h.calls() {
        let pin = FrozenCall::freeze(&parent, &row).unwrap();
        if pin.status == "succeeded" {
            recovery
                .reused_calls
                .insert(pin.logical_step.clone(), pin.clone());
        }
        recovery.prior_calls.push(pin);
    }
    let mut next = h.context.clone();
    next.run_id = "b".repeat(32);
    h.store
        .create(&next, "preview", &"c".repeat(32), None, now())
        .unwrap();
    let snapshot = h
        .snapshots
        .put(&next.run_id, "input", &serde_json::to_value(&plan).unwrap())
        .unwrap();
    h.store
        .prepare(&next.run_id, &snapshot, &snapshot.sha256, &[], now())
        .unwrap();
    let rest = Script::new(vec![Ok(response(facts("s1")))]);
    let mut runner = h.runner(&rest);
    runner.run_id = &next.run_id;
    runner.recovery_inputs = Some(&recovery);
    let results = runner.execute_complete(&plan.batches[0]).unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].reused);
    assert!(!results[1].reused);
    assert_eq!(rest.count(), 1);
    assert_eq!(runner.audited_calls().unwrap().len(), 4);
    runner.execute_complete(&plan.batches[0]).unwrap();
    assert_eq!(rest.count(), 1);
}

#[test]
fn composition_truncation_processes_both_halves_and_validates_all_fact_ids() {
    let h = Harness::new();
    let plan = h.plan(&[source(
        "s1",
        "完整事项一。\n完整事项二。\n完整事项三。\n完整事项四。",
    )]);
    let raw = json!({"schema_version":3,"facts":vec![fact("s1");4],"pending":[]});
    let selected = serde_json::from_value::<FactsResponse>(raw.clone())
        .unwrap()
        .select(
            "batch_000",
            &BTreeSet::from(["s1".into()]),
            &BTreeSet::new(),
            128 * 1024,
        )
        .unwrap();
    let network = Script::new(vec![
        Ok(response(raw)),
        Ok(truncated()),
        Ok(response(composed_for(&selected[..2]))),
        Ok(response(composed_for(&selected[2..]))),
    ]);
    let result = h.runner(&network).generate(&plan).unwrap();
    assert_eq!(result.facts.len(), 4);
    let report = result.report.unwrap();
    assert_eq!(report.matters.len(), 1);
    assert_eq!(report.matters[0].details.len(), 4);
    report.validate(&result.facts).unwrap();
    h.runner(&network).generate(&plan).unwrap();
    assert_eq!(network.count(), 4);
}

#[test]
fn grouped_composition_keeps_more_than_64_facts_and_pending_items() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "材料。")]);
    let mut selected = Vec::new();
    for n in 0..150 {
        let mut fact: ai_daily_report::model::contracts::Fact =
            serde_json::from_value(fact("s1")).unwrap();
        fact.detail = format!("事项 {n}：{}", "完整金额、条件和例外。".repeat(20));
        selected.push(SelectedFact {
            matter_id: None,
            evidence: vec![],
            fact_id: format!("batch_{n:03}:000"),
            pending: n >= 75,
            fact,
        });
    }
    let groups = plan
        .compose_groups(&h.config.llm, selected.clone())
        .unwrap();
    assert!(groups.len() > 2);
    let mut seen = BTreeSet::new();
    let mut parts = Vec::new();
    for group in groups {
        group.validate(&h.config.llm).unwrap();
        assert!(group
            .facts
            .iter()
            .all(|fact| fact.pending == group.facts[0].pending));
        for fact in &group.facts {
            assert!(seen.insert(fact.fact_id.clone()));
        }
        let part: ComposedReport = serde_json::from_value(composed_for(&group.facts)).unwrap();
        part.validate_part(&group.facts).unwrap();
        parts.push(part);
    }
    assert_eq!(seen.len(), 150);
    let merged = ComposedReport::merge(parts, &selected).unwrap();
    assert_eq!(merged.matters[0].details.len(), 75);
    assert_eq!(merged.pending.len(), 75);
    merged.validate(&selected).unwrap();
    assert!(merged.validate_part(&selected).is_err());
}

#[test]
fn indivisible_truncation_stops_without_clipping_or_repeating_the_call() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "同一完整段落。金额为 5 万元；特批需要补签。")]);
    assert_eq!(plan.batches[0].entries.len(), 1);
    let network = Script::new(vec![Ok(truncated())]);
    for _ in 0..2 {
        assert_eq!(
            h.runner(&network)
                .execute_complete(&plan.batches[0])
                .unwrap_err()
                .code,
            "MODEL_CONTENT_UNIT_TOO_LARGE"
        );
    }
    assert_eq!(network.count(), 1);
}

#[test]
fn repeated_splits_preserve_original_offsets_headers_and_complete_paragraphs() {
    let original = source("s1", "# 审批规定\n第一条完整规定。保留第二句。\n第二条：金额 5 万元。\n第三条：例外需要补签。\n第四条：尚未生效。\n");
    let mut pending = original.chunks().unwrap();
    let mut leaves = Vec::new();
    while let Some(entry) = pending.pop() {
        if entry.prefix_ends().len() > 1 {
            pending.extend(entry.split().unwrap());
        } else {
            leaves.push(entry);
        }
    }
    assert_eq!(leaves.len(), 4);
    let mut ranges = Vec::new();
    for entry in leaves {
        assert!(entry.text.contains("# 审批规定"));
        let expected = entry
            .text_ranges
            .iter()
            .map(|range| {
                ai_daily_report::model::slicing::slice(&original.text, range.start, range.end)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(entry.text, expected);
        assert_eq!(entry.prefix_ends().len(), 1);
        ranges.extend(entry.text_ranges);
    }
    assert_eq!(union_chars(&ranges), original.text.chars().count());
}

#[test]
fn table_rows_headers_and_unicode_offsets_remain_complete() {
    let mut s = source(
        "s1",
        "金额规则\n单位万元\n0 < 金额 ≤ 5：主管审批\n5 < 金额 ≤ 10：主管和经理审批\n",
    );
    let lines = s.text.split_inclusive('\n').collect::<Vec<_>>();
    let mut cursor = 0;
    s.units = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let start = cursor;
            cursor += line.chars().count();
            InputUnit {
                start,
                end: cursor,
                kind: if i < 2 { "table_header" } else { "table_row" }.into(),
                ranges: vec![json!({"kind":"xlsx_row","start":i+1,"end":i+1})],
            }
        })
        .collect();
    let chunks = s.chunks().unwrap();
    assert_eq!(chunks.len(), 1);
    let row_end = s.units[2].end;
    let clipped = chunks[0].clip(row_end).unwrap();
    assert!(clipped.text.contains("0 < 金额 ≤ 5"));
    assert!(!clipped.text.contains("5 < 金额 ≤ 10"));
    assert_eq!(union_chars(&clipped.text_ranges), row_end);
    assert!(chunks[0].clip(row_end - 1).is_err());
    let [left, right] = chunks[0].split().unwrap();
    assert!(left.text.contains("0 < 金额 ≤ 5"));
    assert!(!right.text.contains("0 < 金额 ≤ 5"));
    assert!(right.text.contains("5 < 金额 ≤ 10"));
    assert!(right.text.contains("单位万元"));
    assert!(right.text.contains("金额规则"));
    let ranges: Vec<_> = left
        .text_ranges
        .into_iter()
        .chain(right.text_ranges)
        .collect();
    assert_eq!(union_chars(&ranges), s.text.chars().count());
}

#[test]
fn composition_must_cover_every_fact_and_match_sources() {
    let input: FactsResponse = serde_json::from_value(facts("s1")).unwrap();
    let selected = input
        .select(
            "batch_000",
            &BTreeSet::from(["s1".into()]),
            &BTreeSet::new(),
            20_000,
        )
        .unwrap();
    let report: ComposedReport = serde_json::from_value(composed()).unwrap();
    report.validate(&selected).unwrap();
    let mut wrong = composed();
    wrong["matters"][0]["details"][0]["source_ids"] = json!(["other"]);
    assert!(serde_json::from_value::<ComposedReport>(wrong)
        .unwrap()
        .validate(&selected)
        .is_err());
    let mut missing = composed();
    missing["matters"] = json!([]);
    assert!(serde_json::from_value::<ComposedReport>(missing)
        .unwrap()
        .validate(&selected)
        .is_err());
    let mut pending = selected.clone();
    pending[0].pending = true;
    assert!(report.validate(&pending).is_err());
    let input: FactsResponse = serde_json::from_value(facts("s1")).unwrap();
    assert!(input
        .select(
            "batch_000",
            &BTreeSet::from(["s1".into()]),
            &BTreeSet::from(["s1".into()]),
            20_000
        )
        .is_err());
}

#[test]
fn uncertain_facts_are_preserved_as_pending_without_retrying_the_model() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "审批方案尚待确认。")]);
    let mut uncertain = fact("s1");
    uncertain["uncertainty"] = json!("材料没有确认生效日期。");
    let content = json!({"schema_version":3,"facts":[fact("s1"),uncertain],"pending":[fact("s1")]});
    let selected = serde_json::from_value::<FactsResponse>(content.clone())
        .unwrap()
        .select(
            "batch_000",
            &BTreeSet::from(["s1".into()]),
            &BTreeSet::new(),
            20_000,
        )
        .unwrap();
    assert_eq!(selected.len(), 3);
    assert!(!selected[0].pending);
    assert!(selected[1].pending);
    assert!(selected[2].pending);
    assert_eq!(selected[1].fact_id, "batch_000:001");
    assert_eq!(serde_json::to_value(&selected[1].fact).unwrap(), uncertain);
    let network = Script::new(vec![
        Ok(response(content)),
        Ok(response(composed_for(&selected))),
    ]);
    let generated = h.runner(&network).generate_with_workers(&plan, 1).unwrap();
    assert_eq!(generated.facts, selected);
    assert_eq!(generated.report.unwrap().pending.len(), 2);
    assert_eq!(network.count(), 2);
    assert!(h.calls().iter().all(|call| call["status"] == "succeeded"));
}

#[test]
fn uncertain_facts_still_require_valid_structure_and_source_references() {
    for version in [3, 4] {
        let mut valid = facts("s1");
        valid["schema_version"] = json!(version);
        valid["facts"][0]["uncertainty"] = json!("尚未确认。");
        let select = |value| {
            serde_json::from_value::<FactsResponse>(value)
                .unwrap()
                .select(
                    "batch_000",
                    &BTreeSet::from(["s1".into()]),
                    &BTreeSet::new(),
                    20_000,
                )
        };
        assert!(select(valid.clone()).unwrap()[0].pending);
        for (field, value) in [
            ("source_ids", json!(["unknown"])),
            ("source_ids", json!(["s1", "s1"])),
            ("source_ids", json!([])),
            ("uncertainty", json!("")),
            ("detail", json!("")),
            ("conditions", json!([""])),
        ] {
            let mut invalid = valid.clone();
            invalid["facts"][0][field] = value;
            assert_eq!(select(invalid).unwrap_err().code, "MODEL_RESPONSE_INVALID");
        }
    }
}

#[test]
fn disabled_cumulative_budgets_still_keep_audited_usage() {
    let mut h = Harness::new();
    h.config.recovery.max_tokens_per_run = Some(1);
    h.config.recovery.max_calls_per_run = 0;
    let plan = h.plan(&[source("s1", "材料。")]);
    let network = Script::new(vec![Ok(response(facts("s1")))]);
    h.runner(&network).execute(&plan.batches[0]).unwrap();
    let usage: Value = serde_json::from_str(h.calls()[0]["usage_json"].as_str().unwrap()).unwrap();
    assert_eq!(usage["total_tokens"], 180);
    assert_eq!(usage["budget_limits_enforced"], false);
}

#[test]
fn enabled_budget_is_reserved_transactionally_before_network() {
    let mut h = Harness::new();
    h.config.recovery.enforce_model_budgets = true;
    h.config.recovery.max_tokens_per_run = Some(1);
    let plan = h.plan(&[source("s1", "材料。")]);
    let network = Script::new(vec![]);
    assert_eq!(
        h.runner(&network)
            .execute(&plan.batches[0])
            .unwrap_err()
            .code,
        "MODEL_TOKEN_BUDGET_EXHAUSTED"
    );
    assert_eq!(network.count(), 0);
    assert!(h.calls().is_empty());
}

#[test]
fn retry_after_respects_deadline_and_composition_does_not_shrink() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "材料。")]);
    let network = Script::new(vec![Ok(HttpResponse {
        status: 429,
        retry_after: Some("600".into()),
        body: b"{}".to_vec(),
    })]);
    assert_eq!(
        h.runner(&network)
            .execute(&plan.batches[0])
            .unwrap_err()
            .code,
        "MODEL_RUN_DEADLINE_EXCEEDED"
    );
    assert_eq!(network.count(), 1);
    assert_eq!(
        ai_daily_report::model::retry_after(Some("Mon, 12 Oct 2026 16:00:03 GMT"), 1, now()),
        Duration::from_secs(4)
    );
}

#[test]
fn real_http_client_preserves_bytes_disables_redirects_and_nested_thinking() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let expected =
        b"{ \"messages\": [], \"extra_body\": {\"thinking\": {\"type\": \"disabled\"}} }\n"
            .to_vec();
    let bytes = expected.clone();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut input = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = socket.read(&mut buffer).unwrap();
            assert!(count > 0);
            input.extend_from_slice(&buffer[..count]);
            if let Some(header_end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&input[..header_end]);
                let len: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                if input.len() >= header_end + 4 + len {
                    assert_eq!(&input[header_end + 4..], &bytes);
                    break;
                }
            }
        }
        socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{address}/other\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(listener.accept().is_err());
    });
    let transport = HttpTransport::with_endpoint(
        &format!("http://{address}/chat/completions"),
        "synthetic-test-key",
        "",
        "",
    )
    .unwrap();
    assert_eq!(
        transport
            .send(&expected, Duration::from_secs(3))
            .unwrap()
            .status,
        302
    );
    worker.join().unwrap();
}

#[test]
fn pinned_success_can_be_reused_by_a_new_generation_but_modified_rows_cannot() {
    use ai_daily_report::model::recovery::{FrozenCall, RecoveryInputs};
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "采购审批方案待确认。")]);
    let network = Script::new(vec![Ok(response(facts("s1")))]);
    h.runner(&network).execute(&plan.batches[0]).unwrap();
    let run = h.store.get(&h.context.run_id).unwrap();
    let pin = FrozenCall::freeze(&run, &h.calls()[0]).unwrap();
    let recovery = RecoveryInputs {
        job_budget: None,
        reused_calls: BTreeMap::from([("batch_000".into(), pin.clone())]),
        prior_calls: vec![pin],
    };
    let mut next = h.context.clone();
    next.run_id = "b".repeat(32);
    h.store
        .create(&next, "preview", &"c".repeat(32), None, now())
        .unwrap();
    let snapshot = h
        .snapshots
        .put(&next.run_id, "input", &serde_json::to_value(&plan).unwrap())
        .unwrap();
    h.store
        .prepare(&next.run_id, &snapshot, &snapshot.sha256, &[], now())
        .unwrap();
    let mut runner = h.runner(&network);
    runner.run_id = &next.run_id;
    runner.recovery_inputs = Some(&recovery);
    assert!(runner.execute(&plan.batches[0]).unwrap().reused);
    assert_eq!(network.count(), 1);
    assert!(h.store.calls(&next.run_id).unwrap().is_empty());
    assert_eq!(runner.audited_calls().unwrap().len(), 1);
    h.store
        .connection()
        .unwrap()
        .execute(
            "UPDATE report_model_calls SET duration_ms=duration_ms+1",
            [],
        )
        .unwrap();
    assert_eq!(
        runner.execute(&plan.batches[0]).unwrap_err().code,
        "MODEL_REUSE_CALL_MISMATCH"
    );
    assert_eq!(network.count(), 1);
}

#[test]
fn unknown_money_uses_the_frozen_rate_and_success_uses_reported_usage() {
    use ai_daily_report::{config::ModelPricing, model::budget::cost};
    let h = Harness::new();
    let mut recovery = h.config.recovery.clone();
    let pricing = ModelPricing {
        rate_version: "fixture-v1".into(),
        currency: "CNY".into(),
        input_per_million_microunits: 1_000_000,
        output_per_million_microunits: 2_000_000,
    };
    recovery.model_pricing = Some(pricing.clone());
    let reservation = Reservation::new(&h.config.llm, &recovery, 100, 200, json!({}));
    reservation.validate().unwrap();
    assert_eq!(
        charged(
            "unknown",
            &reservation,
            &json!({"prompt_tokens":1,"completion_tokens":1})
        ),
        (1324, Some(cost(1124, 200, &pricing)))
    );
    assert_eq!(
        charged(
            "succeeded",
            &reservation,
            &json!({"prompt_tokens":100,"completion_tokens":80})
        ),
        (180, Some(260))
    );
    let usage = charged(
        "succeeded",
        &reservation,
        &json!({"prompt_tokens":100,"completion_tokens":80,"total_tokens":3000}),
    );
    assert_eq!(usage.0, 3000);
    assert!(usage.1.unwrap() >= reservation.cost_upper_microunits.unwrap());
}

#[test]
fn model_shrink_and_compose_capacity_never_discard_selected_facts() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "材料。")]);
    let fact_response: FactsResponse = serde_json::from_value(facts("s1")).unwrap();
    let selected = fact_response
        .select(
            "batch_000",
            &BTreeSet::from(["s1".into()]),
            &BTreeSet::new(),
            20000,
        )
        .unwrap();
    let step = plan.compose(&h.config.llm, selected.clone()).unwrap();
    let network = Script::new(vec![Ok(HttpResponse {
        status: 200,
        retry_after: None,
        body: serde_json::to_vec(
            &json!({"choices":[{"finish_reason":"length","message":{"content":"{"}}]}),
        )
        .unwrap(),
    })]);
    assert_eq!(
        h.runner(&network).execute(&step).unwrap_err().code,
        "MODEL_OUTPUT_TRUNCATED"
    );
    assert_eq!(network.count(), 1);
    let mut huge = selected;
    huge[0].fact.detail = "完整条件不得丢弃。".repeat(10_000);
    assert_eq!(
        plan.compose(&h.config.llm, huge).unwrap_err().code,
        "MODEL_COMPOSE_CAPACITY_INSUFFICIENT"
    );
    assert_eq!(network.count(), 1);
}

#[test]
fn planner_stops_before_calls_when_all_material_cannot_be_scheduled() {
    let mut h = Harness::new();
    h.config.llm.max_batches = 1;
    h.config.llm.max_sources_per_batch = 8;
    let sources = vec![
        source("s1", &"第一份材料的大段背景。\n".repeat(10000)),
        source("s2", "第二份材料含关键例外。"),
        source("s3", "第三份材料明确尚未生效。"),
    ];
    assert_eq!(
        plan::prepare(&h.config.llm, json!({}), &sources, json!({}))
            .unwrap_err()
            .code,
        "MODEL_BATCH_BUDGET_EXHAUSTED"
    );
    assert!(h.calls().is_empty());
}

#[test]
fn thinking_location_and_request_identity_are_explicit() {
    let mut h = Harness::new();
    h.config.llm.thinking_mode = Some("disabled".into());
    h.config.llm.thinking_parameter_location = Some("extra_body".into());
    let plan = plan::prepare(
        &h.config.llm,
        json!({}),
        &[source("s1", "材料。")],
        json!({}),
    )
    .unwrap();
    let old = &plan.batches[0];
    assert!(old.request.get("thinking").is_none());
    assert_eq!(old.request["extra_body"]["thinking"]["type"], "disabled");
    h.config.llm.thinking_parameter_location = Some("top_level".into());
    let plan = plan::prepare(
        &h.config.llm,
        json!({}),
        &[source("s1", "材料。")],
        json!({}),
    )
    .unwrap();
    let current = &plan.batches[0];
    assert!(current.request.get("extra_body").is_none());
    assert_eq!(current.request["thinking"]["type"], "disabled");
    assert_ne!(old.identity().unwrap(), current.identity().unwrap());
}

#[test]
fn call_dates_follow_actual_shanghai_time_instead_of_report_date() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "材料。")]);
    let network = Script::new(vec![Ok(response(facts("s1"))), Ok(response(composed()))]);
    h.runner(&network).execute(&plan.batches[0]).unwrap();
    let facts = serde_json::from_value::<FactsResponse>(facts("s1"))
        .unwrap()
        .select(
            "batch_000",
            &BTreeSet::from(["s1".into()]),
            &BTreeSet::new(),
            20_000,
        )
        .unwrap();
    let mut runner = h.runner(&network);
    runner.started_at = now() + chrono::Duration::seconds(2);
    runner
        .execute(&plan.compose(&h.config.llm, facts).unwrap())
        .unwrap();
    let calls = h.calls();
    assert_eq!(calls[0]["call_date"], "2026-10-12");
    assert_eq!(calls[1]["call_date"], "2026-10-13");
    assert_ne!(calls[0]["call_date"], h.context.target_date.to_string());
}

#[test]
fn cached_responses_need_no_credentials_and_missing_credentials_reserve_no_calls() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "材料。")]);
    let offline = HttpTransport::new(&h.config);
    assert_eq!(
        h.runner(&offline)
            .execute(&plan.batches[0])
            .unwrap_err()
            .code,
        "MODEL_CREDENTIAL_MISSING"
    );
    assert!(h.calls().is_empty());
    let network = Script::new(vec![Ok(response(facts("s1")))]);
    h.runner(&network).execute(&plan.batches[0]).unwrap();
    assert!(h.runner(&offline).execute(&plan.batches[0]).unwrap().reused);
    assert_eq!(h.calls().len(), 1);
}

#[test]
fn global_merge_ranks_last_group_and_unifies_stable_matter_ids() {
    use ai_daily_report::model::contracts::{ComposedReport, SelectedFact};
    let make_fact = |id: &str, matter: &str, detail: &str| -> SelectedFact {
        serde_json::from_value(json!({"fact_id":id,"matter_id":matter,"pending":false,"fact":{"topic":detail,"background":null,"detail":detail,"conditions":[],"status":null,"source_ids":["s1"],"uncertainty":null}})).unwrap()
    };
    let mut facts: Vec<_> = (0..6)
        .map(|i| make_fact(&format!("f{i}"), &format!("m{i}"), "一般资料整理"))
        .collect();
    facts.push(make_fact(
        "important",
        "approval",
        "审批权限金额由5万元调整为10万元",
    ));
    facts.push(make_fact("same", "approval", "审批条件更新"));
    let parts:Vec<ComposedReport>=facts.iter().enumerate().map(|(i,f)| {
        let cite=json!({"text":f.fact.detail,"fact_ids":[f.fact_id],"source_ids":["s1"]});
        serde_json::from_value(json!({"schema_version":4,"overview":[cite],"matters":[{"matter_id":f.matter_id,"title":format!("不同标题{i}"),"background":null,"details":[cite],"status":null}],"pending":[]})).unwrap()
    }).collect();
    let report = ComposedReport::merge(parts, &facts).unwrap();
    assert!(report.overview.len() <= 5);
    assert!(report.overview[0].text.contains("审批"));
    let same: Vec<_> = report
        .matters
        .iter()
        .filter(|m| m.matter_id.as_deref() == Some("approval"))
        .collect();
    assert_eq!(same.len(), 1);
    assert_eq!(same[0].details.len(), 2);
}

#[test]
fn global_overview_considers_facts_in_status_and_background() {
    use ai_daily_report::model::contracts::{ComposedReport, SelectedFact};
    let facts: Vec<SelectedFact> = ["资料整理", "审批权限调整"].into_iter().enumerate().map(|(i,text)| {
        serde_json::from_value(json!({"fact_id":format!("f{i}"),"matter_id":"m1","pending":false,"fact":{"topic":text,"background":null,"detail":text,"conditions":[],"status":null,"source_ids":["s1"],"uncertainty":null}})).unwrap()
    }).collect();
    let cite = |i: usize| json!({"text":facts[i].fact.detail,"fact_ids":[facts[i].fact_id],"source_ids":["s1"]});
    let part:ComposedReport=serde_json::from_value(json!({"schema_version":4,"overview":[cite(0)],"matters":[{"matter_id":"m1","title":"工作进展","background":null,"details":[cite(0)],"status":cite(1)}],"pending":[]})).unwrap();
    part.validate_part(&facts).unwrap();
    let merged = ComposedReport::merge(vec![part], &facts).unwrap();
    assert_eq!(merged.overview[0].fact_ids, vec!["f1"]);
}
