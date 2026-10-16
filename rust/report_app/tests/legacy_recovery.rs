use ai_daily_report::{
    config::AppConfig,
    model::legacy::LegacyBatch,
    reports,
    run::{repair_output, run_report, App},
    snapshots::canonical_json,
    types::{Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-12T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Fixture {
    _dir: tempfile::TempDir,
    app: App,
    context: RunContext,
    request: ReportRequest,
}
impl Fixture {
    fn new(version: u32, successful: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::write(
            dir.path().join("config/settings.yaml"),
            "paths:\n  work_dir: nonexistent\n",
        )
        .unwrap();
        let app = App::open(
            AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap(),
            now(),
        )
        .unwrap();
        let mut input: Value =
            serde_json::from_str(include_str!("fixtures/input-snapshot-v2.json")).unwrap();
        input["version"] = json!(version);
        if version == 1 {
            let batch = &mut input["batches"][0];
            batch["response_schema_version"] = json!(1);
            for key in [
                "response_schema_sha256",
                "input_audit",
                "input_range_version",
            ] {
                batch[key] = Value::Null;
            }
        }
        let context: RunContext = serde_json::from_value(input["context"].clone()).unwrap();
        let id = &context.run_id;
        app.store
            .create(&context, "report", id, None, now())
            .unwrap();
        let reference = app.snapshots.put(id, "input", &input).unwrap();
        app.store
            .prepare(
                id,
                &reference,
                input["input_fingerprint"].as_str().unwrap(),
                input["sources"].as_array().unwrap(),
                now(),
            )
            .unwrap();
        let batch: LegacyBatch = serde_json::from_value(input["batches"][0].clone()).unwrap();
        batch.validate().unwrap();
        let request = app
            .snapshots
            .put_bytes(
                id,
                "model-request",
                &canonical_json(&batch.request()).unwrap(),
            )
            .unwrap();
        let response: Value =
            serde_json::from_str(include_str!("fixtures/model-response-v2.json")).unwrap();
        let response = app
            .snapshots
            .put(id, "model-response", &json!({"response":response}))
            .unwrap();
        let run = app.store.get(id).unwrap();
        app.store.connection().unwrap().execute("INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,logical_step,attempt,call_date,status,request_path,request_sha256,response_path,response_sha256,usage_json,started_at,finished_at,duration_ms) VALUES (?,?,?,'batch_000',1,'2026-10-12',?,?,?,?,?,'{}',?, ?,100)",rusqlite::params![id,id,batch.identity(&run).unwrap(),if successful{"succeeded"}else{"unknown"},request.path,request.sha256,response.path,response.sha256,now().to_rfc3339(),now().to_rfc3339()]).unwrap();
        app.store.fail(id, "RUN_INTERRUPTED", false, now()).unwrap();
        let day = context.target_date;
        let request = ReportRequest {
            period: Period {
                mode: ReportMode::Daily,
                source: ReportSource::Scan,
                start: day,
                end: day,
                label: day.to_string(),
            },
            context: RunContext::new("b".repeat(32), now(), day, String::new()).unwrap(),
            input: String::new(),
            save: true,
            dry_run: false,
            scheduled: false,
            regeneration_reason: None,
        };
        Self {
            _dir: dir,
            app,
            context,
            request,
        }
    }
}

#[test]
fn current_legacy_daily_template_computed_values_match_frozen_python_bytes() {
    let report: Value = serde_json::from_str(include_str!("fixtures/daily-v2.json")).unwrap();
    let rendered = reports::legacy::render(
        &report,
        include_str!("../../../templates/evidence_report_template.md"),
    )
    .unwrap();
    assert_eq!(rendered, include_str!("fixtures/daily-v2.md"));
}

#[test]
fn frozen_v1_and_v2_successes_recover_without_scanner_credentials_or_new_calls() {
    for version in [1, 2] {
        let f = Fixture::new(version, true);
        let output = run_report(f.request, &f.app).unwrap();
        assert_eq!(output.run_id, f.context.run_id);
        assert_eq!(output.model_calls, 0);
        assert!(output.markdown.contains("材料记载了工作安排"));
        assert!(!f.app.config.index_db_path.exists());
        assert_eq!(f.app.store.calls(&output.run_id).unwrap().len(), 1);
        let raw: Value = serde_json::from_str(
            f.app
                .store
                .current("2026-10-11")
                .unwrap()
                .unwrap()
                .report_json
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(raw["schema_version"], 2);
        fs::remove_file(output.output_path.unwrap()).unwrap();
        assert_eq!(
            repair_output(&f.app, "2026-10-11", now()).unwrap().markdown,
            output.markdown
        );
    }
}

#[test]
fn old_unknown_call_remains_charged_and_cannot_silently_restart_under_new_configuration() {
    let f = Fixture::new(2, false);
    let error = run_report(f.request, &f.app).unwrap_err();
    assert_eq!(error.code, "RECOVERY_CONFIG_CHANGED");
    let calls = f.app.store.calls(&f.context.run_id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["status"], "unknown");
    assert!(f.app.store.current("2026-10-11").unwrap().is_none());
}

#[test]
fn modified_legacy_response_snapshot_is_rejected() {
    let f = Fixture::new(2, true);
    let call = &f.app.store.calls(&f.context.run_id).unwrap()[0];
    fs::write(
        f.app
            .snapshots
            .root
            .join(call["response_path"].as_str().unwrap()),
        "{}",
    )
    .unwrap();
    assert_eq!(
        run_report(f.request, &f.app).unwrap_err().code,
        "SNAPSHOT_HASH_MISMATCH"
    );
}

struct LegacyNetwork;
impl ai_daily_report::model::transport::ModelTransport for LegacyNetwork {
    fn send(
        &self,
        request: &[u8],
        _: std::time::Duration,
    ) -> Result<
        ai_daily_report::model::transport::HttpResponse,
        ai_daily_report::model::transport::TransportFailure,
    > {
        let value: Value = serde_json::from_slice(request).unwrap();
        assert_eq!(value["model"], "deepseek-flash");
        assert!(value["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("summary_items"));
        Ok(ai_daily_report::model::transport::HttpResponse {
            status: 200,
            retry_after: None,
            body: include_bytes!("fixtures/model-response-v2.json").to_vec(),
        })
    }
}
#[test]
fn explicitly_recovered_legacy_input_sends_only_missing_batch_and_reuses_foreign_generation() {
    use ai_daily_report::{
        model::recovery::FrozenCall,
        run::{prepare_snapshot, resume_frozen},
        snapshots::fingerprint,
    };
    let mut f = Fixture::new(2, true);
    f.app.config.llm.model_id = "deepseek-flash".into();
    f.app.config.llm.context_window_tokens = Some(100000);
    f.app.config.llm.verified_max_output_tokens = Some(8192);
    f.app.transport = Box::new(LegacyNetwork);
    let parent = f.app.store.get(&f.context.run_id).unwrap();
    let pin = FrozenCall::freeze(&parent, &f.app.store.calls(&parent.run_id).unwrap()[0]).unwrap();
    let mut input: Value =
        serde_json::from_str(include_str!("fixtures/input-snapshot-v2.json")).unwrap();
    let mut second = input["batches"][0].clone();
    second["logical_step"] = json!("batch_001");
    input["batches"].as_array_mut().unwrap().push(second);
    input["context"] = serde_json::to_value(
        RunContext::new(
            "e".repeat(32),
            now(),
            f.context.target_date,
            f.app.fingerprint(false).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    input["rust_recovery_version"] = json!(3);
    input["reused_calls"] = json!({"batch_000":pin});
    input["prior_calls"] = json!([pin]);
    input["recovery_fingerprint"] = json!(f.app.fingerprint(true).unwrap());
    input["input_fingerprint"] = json!(fingerprint(&input).unwrap());
    let id = prepare_snapshot(&f.app, &input, false, "明确恢复旧输入").unwrap();
    let outcome = resume_frozen(&f.app, &id, now()).unwrap();
    assert_eq!(outcome.model_calls, 1);
    assert!(outcome.markdown.contains("材料记载了工作安排"));
    assert_eq!(f.app.store.calls(&id).unwrap().len(), 1);
    assert_eq!(resume_frozen(&f.app, &id, now()).unwrap().model_calls, 0);
    assert_eq!(f.app.store.calls(&parent.run_id).unwrap().len(), 1);
}
