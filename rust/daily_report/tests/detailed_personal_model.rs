use ai_daily_report::{
    config::{AppConfig, Llm},
    evidence::EvidenceRef,
    model::{
        personal::{generate, prepare_detailed_scan, PersonalOutput, SCAN_PROMPT_DETAILED},
        plan::PlannedStep,
        transport::{HttpResponse, ModelTransport, TransportFailure},
        ModelRunner,
    },
    reports::{
        data::ReportSource,
        personal::{body_chars, Material, Selection, SelectionSummary},
    },
    snapshots::{digest, SnapshotStore},
    store::Store,
    types::RunContext,
    ReportError,
};
use chrono::{TimeZone, Utc};
use daily_parsers::contract::{SourceRange, SourceRangeUnit};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

fn llm() -> Llm {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::create_dir(dir.path().join("work")).unwrap();
    let settings = json!({"paths":{"work_dir":"work"},"llm":{"model_id":"offline-fixture",
        "temperature":0.2,"max_tokens":4096,"max_request_utf8_bytes":1_000_000,
        "context_window_tokens":1_000_000,"verified_max_output_tokens":65_536,
        "invalid_response_retries":0}});
    fs::write(
        dir.path().join("config/settings.yaml"),
        settings.to_string(),
    )
    .unwrap();
    AppConfig::load_with_env(dir.path(), None, &Default::default())
        .unwrap()
        .llm
}

fn context() -> RunContext {
    let now = Utc.with_ymd_and_hms(2026, 11, 12, 8, 0, 0).unwrap();
    RunContext::new("1".repeat(32), now, now.date_naive(), "a".repeat(64)).unwrap()
}

fn notes(text: &str) -> Material {
    Material {
        source: ReportSource::user(text, context().started_at),
        text: text.into(),
        evidence: vec![],
        priority: 0,
    }
}

fn row(path: &str, number: u64, text: &str) -> Material {
    let mut m = notes(text);
    let raw = digest(path.as_bytes());
    m.source.source_id = format!("src_{}", digest(format!("{path}:{number}").as_bytes()));
    m.source.source_kind = "file".into();
    m.source.relative_path = Some(path.into());
    m.source.content_sha256 = Some(raw.clone());
    m.priority = 1;
    m.evidence.push(EvidenceRef {
        source_id: m.source.source_id.clone(),
        version_id: format!("scan-content:{raw}"),
        extraction_id: format!("scan:{path}"),
        unit_id: format!("row:{number}"),
        side: "current".into(),
        raw_sha256: raw,
        unit_sha256: m.source.excerpt_sha256.clone(),
        quote: text.into(),
        quote_sha256: m.source.excerpt_sha256.clone(),
        unit_char_start: 0,
        input_start: 0,
        input_end: m.source.excerpt_chars,
        ranges: vec![SourceRange::new(
            SourceRangeUnit::SheetRow,
            number,
            number + 1,
            Some("金额".into()),
        )],
    });
    m
}

fn selection(materials: Vec<Material>) -> Selection {
    let files = materials
        .iter()
        .filter_map(|m| m.source.relative_path.as_ref())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    Selection {
        summary: SelectionSummary {
            observed_files: files,
            inspected_files: files,
            selected_files: files,
            selected_chars: materials.iter().map(|m| m.source.excerpt_chars).sum(),
            ..Default::default()
        },
        materials,
        gaps: vec![],
    }
}

fn item(m: &Material, title: &str) -> Value {
    json!({"title":title,"text":m.text,"source_ids":[m.source.source_id],"kind":"progress",
        "statement_kind":if m.source.source_kind=="user_statement" {"user_statement"} else {"observed"}})
}

struct Reply {
    request: Vec<u8>,
    response: Value,
    sent: AtomicUsize,
}

impl ModelTransport for Reply {
    fn send(&self, request: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        self.sent.fetch_add(1, Ordering::Relaxed);
        assert_eq!(request, self.request);
        Ok(HttpResponse {status: 200, retry_after: None, body: serde_json::to_vec(&json!({
            "choices":[{"finish_reason":"stop","message":{"content":self.response.to_string()}}],
            "usage":{"prompt_tokens":1234,"completion_tokens":567}
        })).unwrap()})
    }
}

fn prepared_stores(
    dir: &tempfile::TempDir,
    ctx: &RunContext,
    plan: &PlannedStep,
) -> (Store, SnapshotStore) {
    let store = Store::open(&dir.path().join("reports.sqlite3"), ctx.started_at).unwrap();
    let snapshots = SnapshotStore::new(dir.path().join("snapshots"), 16 * 1024 * 1024).unwrap();
    store
        .create_personal(ctx, "preview", &ctx.run_id, None, ctx.started_at)
        .unwrap();
    let input = snapshots.put(&ctx.run_id, "input", &json!(plan)).unwrap();
    store
        .prepare(&ctx.run_id, &input, &input.sha256, &[], ctx.started_at)
        .unwrap();
    (store, snapshots)
}

fn execute(
    llm: &Llm,
    plan: &PlannedStep,
    response: Value,
) -> (Result<PersonalOutput, ReportError>, Value) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context();
    let (store, snapshots) = prepared_stores(&dir, &ctx, plan);
    let reply = Reply {
        request: plan.bytes().unwrap(),
        response,
        sent: AtomicUsize::new(0),
    };
    let started = Instant::now();
    let runner = ModelRunner {
        store: &store,
        snapshots: &snapshots,
        transport: &reply,
        llm,
        allow_new_calls: true,
        run_id: &ctx.run_id,
        started_at: ctx.started_at,
        started,
        deadline: started + Duration::from_secs(60),
    };
    let result = generate(&runner, plan);
    if let Ok(output) = &result {
        let replay = ModelRunner {
            allow_new_calls: false,
            ..runner
        }
        .execute(plan)
        .unwrap();
        assert!(replay.reused);
        assert_eq!(json!(replay.output.items), json!(output.items));
    }
    let mut calls = store.calls(&ctx.run_id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(reply.sent.load(Ordering::Relaxed), 1);
    (result, calls.pop().unwrap())
}

#[test]
fn small_request_must_not_exclude_user_notes_to_make_room_for_files() {
    let mut config = llm();
    let file = row("a.xlsx", 1, "金额待复核");
    config.max_request_utf8_bytes =
        prepare_detailed_scan(&config, &context(), &selection(vec![file.clone()]))
            .unwrap()
            .bytes()
            .unwrap()
            .len();
    let full = selection(vec![file, notes(&"补".repeat(2_000))]);
    assert_eq!(
        prepare_detailed_scan(&config, &context(), &full)
            .unwrap_err()
            .code,
        "MODEL_REQUEST_TOO_LARGE"
    );
    assert_eq!(full.user_notes().chars().count(), 2_000);
}

#[test]
fn narrow_byte_and_context_limits_rotate_whole_blocks_and_charge_header_context() {
    let mut llm = llm();
    llm.provider = "openai".into();
    llm.reasoning_effort = Some("medium".into());
    llm.verified_max_output_tokens = Some(2048);
    let mut a = row("a.xlsx", 2, "报价 \"A\"：\n12.5 🦀\\箱");
    let header = json!([{"text":"预算（万元）\nA\\B", "ranges":a.evidence[0].ranges}]);
    a.source.coverage_limits["personal"] = json!({"context":header,"absolute_path":"LOCAL_ONLY"});
    let b = row("b.xlsx", 2, "相同预算按元记录。");
    let c = row("c.xlsx", 2, "附条件核准。");
    let note = notes("金额待复核。");
    let expected = selection(vec![a.clone(), b.clone(), c.clone(), note.clone()]);
    let wire_len = |s: &Selection| {
        prepare_detailed_scan(&llm, &context(), s)
            .unwrap()
            .bytes()
            .unwrap()
            .len()
    };
    let exact = wire_len(&expected);
    let single = wire_len(&selection(vec![row("a.xlsx", 1, "1"), note.clone()]));
    let large = row("a.xlsx", 1, &"1".repeat(exact - single - 16));
    let nearly_full = wire_len(&selection(vec![large.clone(), note.clone()]));
    assert!((exact - 32..=exact).contains(&nearly_full));
    let mut huge_header = row("b.xlsx", 1, "单位见表头");
    huge_header.source.coverage_limits["personal"] = json!({"context":[{
        "text":"表头\t\\\"".repeat(5000),"ranges":huge_header.evidence[0].ranges}]});
    let full = selection(vec![
        large,
        a,
        row("a.xlsx", 3, "另一行"),
        huge_header,
        b,
        c,
        note,
    ]);
    let original = json!(full);
    for context_limited in [false, true] {
        let mut narrow = llm.clone();
        if context_limited {
            narrow.context_window_tokens = Some(exact as u64 + 1024 + 2048);
        } else {
            narrow.max_request_utf8_bytes = exact;
        }
        let plan = prepare_detailed_scan(&narrow, &context(), &full).unwrap();
        plan.validate(&narrow).unwrap();
        assert_eq!(plan.bytes().unwrap().len(), exact);
        assert_eq!(plan.request["max_completion_tokens"], 2048);
        let sent: Selection = serde_json::from_value(plan.target["selection"].clone()).unwrap();
        sent.validate().unwrap();
        assert_eq!(json!(sent.materials), json!(expected.materials));
        assert_eq!(sent.summary.selected_files, 3);
        assert_eq!(sent.summary.selected_chars, expected.summary.selected_chars);
        assert_eq!(sent.summary.omissions["budget"], 3);
        let source_ids: BTreeSet<_> = plan.source_map.as_object().unwrap().keys().collect();
        assert_eq!(
            source_ids,
            sent.materials.iter().map(|m| &m.source.source_id).collect()
        );
        let content = plan.request["messages"][1]["content"].as_str().unwrap();
        assert!(!content.contains("LOCAL_ONLY"));
        let sources = &serde_json::from_str::<Value>(content).unwrap()["sources"];
        assert_eq!(sources[0]["context"], header);
        assert_eq!(sources[0]["text"], expected.materials[0].text);
        assert!(sources[1].get("context").is_none());
        narrow.max_request_utf8_bytes = exact - 1;
        let smaller = prepare_detailed_scan(&narrow, &context(), &full).unwrap();
        assert!(smaller.bytes().unwrap().len() < exact);
        let smaller_map = smaller.source_map.as_object().unwrap();
        assert!(smaller_map.contains_key("user_input"));
        assert_ne!(smaller_map.keys().collect::<BTreeSet<_>>(), source_ids);
        assert_eq!(smaller.target["selection"]["summary"]["selected_files"], 3);
    }
    assert_eq!(json!(full), original);
}

#[test]
fn thousands_of_blocks_share_a_single_request_across_files() {
    let mut llm = llm();
    let full = selection(
        (0..32)
            .flat_map(|file| {
                let path = format!("budget-{file:02}.xlsx");
                (1..=64).map(move |number| row(&path, number, "金额仍待审批"))
            })
            .collect(),
    );
    let one_per_file = selection(full.materials.iter().step_by(64).cloned().collect());
    llm.max_request_utf8_bytes = prepare_detailed_scan(&llm, &context(), &one_per_file)
        .unwrap()
        .bytes()
        .unwrap()
        .len();
    let plan = prepare_detailed_scan(&llm, &context(), &full).unwrap();
    assert_eq!(plan.source_map.as_object().unwrap().len(), 32);
    let sent: Selection = serde_json::from_value(plan.target["selection"].clone()).unwrap();
    assert_eq!(json!(sent.materials), json!(one_per_file.materials));
    assert_eq!(sent.summary.omissions["budget"], 2016);
    assert_eq!(full.materials.len(), 2048);
}

#[test]
fn eight_detailed_items_over_the_legacy_body_and_wire_limits_survive_one_call_and_replay() {
    let mut llm = llm();
    llm.max_tokens = 65_536;
    let full = selection(
        (1..=8)
            .map(|i| {
                row(
                    "明细.xlsx",
                    i,
                    &format!("第 {i} 项。{}", "逐项核实金额及单位。".repeat(650)),
                )
            })
            .collect(),
    );
    let plan = prepare_detailed_scan(&llm, &context(), &full).unwrap();
    assert_eq!(plan.version, 7);
    assert_eq!(plan.target["summary_prompt_version"], 8);
    assert_eq!(plan.prompt_sha256, digest(SCAN_PROMPT_DETAILED.as_bytes()));
    let prompt = plan.request["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("每日一次扫描") && prompt.contains("事项详述"));
    let items: Vec<_> = full
        .materials
        .iter()
        .enumerate()
        .map(|(i, m)| item(m, &format!("第 {} 笔金额已核实", i + 1)))
        .collect();
    let response = json!({"schema_version":5,"items":items});
    assert!(response.to_string().len() > 128 * 1024);
    let (output, call) = execute(&llm, &plan, response);
    let output = output.unwrap();
    assert_eq!(json!(output.items), json!(items));
    assert!(body_chars(&output.items) > 1500);
    assert_eq!(output.omitted, 0);
    assert!(output.compaction_error.is_none());
    assert_eq!(call["status"], "succeeded");
    let usage: Value = serde_json::from_str(call["usage_json"].as_str().unwrap()).unwrap();
    assert_eq!(usage["prompt_tokens"], 1234);
    assert_eq!(usage["completion_tokens"], 567);
}

#[test]
fn omitted_real_ids_and_invented_ids_cannot_support_a_detailed_item() {
    let mut llm = llm();
    let small = row("a.xlsx", 1, "目标日期待定");
    let huge = row("b.xlsx", 1, &"金额".repeat(5000));
    llm.max_request_utf8_bytes =
        prepare_detailed_scan(&llm, &context(), &selection(vec![small.clone()]))
            .unwrap()
            .bytes()
            .unwrap()
            .len();
    let plan = prepare_detailed_scan(
        &llm,
        &context(),
        &selection(vec![small.clone(), huge.clone()]),
    )
    .unwrap();
    let mut forged = plan.clone();
    forged.source_map[&huge.source.source_id] = json!(huge.source);
    assert_eq!(
        forged.validate(&llm).unwrap_err().code,
        "MODEL_PLAN_IDENTITY_MISMATCH"
    );
    for id in [huge.source.source_id, format!("src_{}", "f".repeat(64))] {
        let mut claim = item(&small, "日期待定");
        claim["source_ids"] = json!([small.source.source_id, id]);
        let (result, call) = execute(&llm, &plan, json!({"schema_version":5,"items":[claim]}));
        assert_eq!(result.unwrap_err().code, "MODEL_RESPONSE_INVALID");
        assert_eq!(call["status"], "failed");
    }
    let (result, _) = execute(&llm, &plan, json!({"schema_version":5,"items":[]}));
    assert!(result.unwrap().items.is_empty());
}

#[test]
fn only_detailed_plans_reject_titles_longer_than_160_unicode_characters() {
    let llm = llm();
    let full = selection(vec![notes("调整金额仍待审批。")]);
    let plan = prepare_detailed_scan(&llm, &context(), &full).unwrap();
    let short = "🦀".repeat(160);
    let long = format!("{short}元");
    let response =
        |title: &str| json!({"schema_version":5,"items":[item(&full.materials[0], title)]});
    let allowed = response(&short);
    let oversized = response(&long);
    let output = execute(&llm, &plan, allowed).0.unwrap();
    assert_eq!(output.items[0].title, short);
    let (rejected, call) = execute(&llm, &plan, oversized.clone());
    assert_eq!(rejected.unwrap_err().code, "MODEL_RESPONSE_INVALID");
    assert_eq!(call["status"], "failed");
    let legacy = serde_json::from_str(include_str!("fixtures/retired/plan-v6-title.json")).unwrap();
    assert_eq!(
        execute(&llm, &legacy, oversized).0.unwrap().items[0].title,
        long
    );
}

#[test]
fn lower_output_limit_replays_frozen_v6_and_v7_calls_and_bounds_new_sends() {
    let ctx = context();
    let full = selection(vec![notes("预算仍待审批。")]);
    let response = json!({"schema_version":5,"items":[item(&full.materials[0], "预算待审批")]});
    for detailed in [false, true] {
        let mut original = llm();
        let token_field = if detailed {
            original.provider = "openai".into();
            original.reasoning_effort = Some("medium".into());
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let prepare = |config: &Llm| {
            if detailed {
                prepare_detailed_scan(config, &ctx, &full).unwrap()
            } else {
                serde_json::from_str(if config.verified_max_output_tokens == Some(2048) {
                    include_str!("fixtures/retired/plan-v6-tokens2048.json")
                } else {
                    include_str!("fixtures/retired/plan-v6-tokens4096.json")
                })
                .unwrap()
            }
        };
        let plan = prepare(&original);
        assert_eq!(plan.request[token_field], 4096);
        let mut lower = original.clone();
        lower.verified_max_output_tokens = Some(2048);
        let dir = tempfile::tempdir().unwrap();
        let (store, snapshots) = prepared_stores(&dir, &ctx, &plan);
        let reply = Reply {
            request: plan.bytes().unwrap(),
            response: response.clone(),
            sent: AtomicUsize::new(0),
        };
        let started = Instant::now();
        let runner = ModelRunner {
            store: &store,
            snapshots: &snapshots,
            transport: &reply,
            llm: &original,
            allow_new_calls: true,
            run_id: &ctx.run_id,
            started_at: ctx.started_at,
            started,
            deadline: started + Duration::from_secs(60),
        };
        let mut recovery = ModelRunner {
            llm: &lower,
            allow_new_calls: false,
            ..runner
        };

        // A pending frozen request cannot silently change bytes or its authorization.
        assert_eq!(
            recovery.execute(&plan).unwrap_err().code,
            "RECOVERY_CONFIG_CHANGED"
        );
        recovery.allow_new_calls = true;
        assert_eq!(
            recovery.execute(&plan).unwrap_err().code,
            "MODEL_OUTPUT_LIMIT_INVALID"
        );
        assert!(store.calls(&ctx.run_id).unwrap().is_empty());
        assert_eq!(reply.sent.load(Ordering::Relaxed), 0);

        let first = runner.execute(&plan).unwrap();
        let calls = store.calls(&ctx.run_id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["request_sha256"], digest(&reply.request));
        recovery.allow_new_calls = false;
        let restored = recovery.execute(&plan).unwrap();
        assert!(restored.reused);
        assert_eq!(restored.call_id, first.call_id);
        assert_eq!(restored.effective.bytes().unwrap(), reply.request);
        assert_eq!(json!(restored.output.items), response["items"]);

        let mut altered = plan.clone();
        altered.request[token_field] = json!(2048);
        assert_eq!(
            recovery.execute(&altered).unwrap_err().code,
            "MODEL_PLAN_IDENTITY_MISMATCH"
        );
        let mut other_model = lower.clone();
        other_model.model_id.push_str("-other");
        assert_eq!(
            ModelRunner {
                llm: &other_model,
                ..recovery
            }
            .execute(&plan)
            .unwrap_err()
            .code,
            "MODEL_PLAN_IDENTITY_MISMATCH"
        );
        assert_eq!(store.calls(&ctx.run_id).unwrap(), calls);
        assert_eq!(reply.sent.load(Ordering::Relaxed), 1);

        let new_plan = prepare(&lower);
        assert_eq!(new_plan.request[token_field], 2048);
        assert_eq!(new_plan.request["messages"], plan.request["messages"]);
        assert!(execute(&lower, &new_plan, response.clone()).0.is_ok());
    }
}

#[test]
fn file_turn_prefers_an_affordable_total_and_keeps_original_display_order() {
    let mut config = llm();
    let mut ordinary = row("a.xlsx", 1, "普通行");
    ordinary.priority = 2;
    let other_file = row("b.xlsx", 1, "另一份报表的行");
    let total = row("a.xlsx", 9, "合计 120 万元");
    let full = selection(vec![ordinary, other_file.clone(), total.clone()]);
    let expected = selection(vec![other_file, total]);
    config.max_request_utf8_bytes = prepare_detailed_scan(&config, &context(), &expected)
        .unwrap()
        .bytes()
        .unwrap()
        .len();
    let plan = prepare_detailed_scan(&config, &context(), &full).unwrap();
    let sent: Selection = serde_json::from_value(plan.target["selection"].clone()).unwrap();
    assert_eq!(json!(sent.materials), json!(expected.materials));
    assert_eq!(sent.summary.selected_files, 2);
    assert_eq!(sent.summary.omissions["budget"], 1);
    let wire: Value =
        serde_json::from_str(plan.request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        wire["sources"][0]["source_id"],
        expected.materials[0].source.source_id
    );
    assert_eq!(
        wire["sources"][1]["source_id"],
        expected.materials[1].source.source_id
    );
}

#[test]
fn frozen_v6_request_bytes_and_response_rules_remain_authoritative() {
    let llm = llm();
    let full = selection(vec![notes("Budget 42")]);
    let frozen: PlannedStep =
        serde_json::from_str(include_str!("fixtures/retired/plan-v6-budget.json")).unwrap();
    frozen.validate(&llm).unwrap();
    assert_eq!(frozen.version, 6);
    assert_eq!(frozen.target["summary_prompt_version"], 6);
    // Historical v6 wire bytes for this date, source, model and temperature.
    assert_eq!(
        digest(&frozen.bytes().unwrap()),
        "ee3c8b66e53cdd91fa1e69688ef5d429842f0d34465c70d0717109fbff27de6f"
    );
    let claim = item(&full.materials[0], "Budget confirmed");
    assert_eq!(
        execute(&llm, &frozen, json!({"schema_version":5,"items":[claim]}))
            .0
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        execute(&llm, &frozen, json!({"schema_version":5,"items":[]}))
            .0
            .unwrap_err()
            .code,
        "MODEL_RESPONSE_INVALID"
    );
    let mut altered = frozen;
    altered.version = 7;
    assert!(altered.validate(&llm).is_err());
    let mut detailed = prepare_detailed_scan(&llm, &context(), &full).unwrap();
    detailed.target["summary_limits"] = json!({"max_chars":1500,"max_items":5});
    assert!(detailed.validate(&llm).is_err());
}
