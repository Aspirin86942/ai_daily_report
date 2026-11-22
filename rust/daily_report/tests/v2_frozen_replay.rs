use ai_daily_report::{
    config::AppConfig,
    model::{
        personal::generate,
        transport::{HttpResponse, ModelTransport, TransportFailure},
        ModelRunner,
    },
    run::personal::PersonalInput,
    snapshots::{digest, SnapshotRef, SnapshotStore},
    store::Store,
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Offline;

impl ModelTransport for Offline {
    fn prepare(&self) -> Result<(), ai_daily_report::ReportError> {
        panic!("历史回放必须使用已审计的响应")
    }

    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        unreachable!()
    }
}

#[test]
#[ignore = "需要 AI_DAILY_TEST_REPLAY_FIXTURES 指向旧包合成验收目录"]
fn three_historical_inputs_replay_identical_summary_and_attachment_bytes() {
    let fixture_root = PathBuf::from(std::env::var_os("AI_DAILY_TEST_REPLAY_FIXTURES").unwrap());
    let manifest: Value =
        serde_json::from_slice(&fs::read(fixture_root.join("verification.json")).unwrap()).unwrap();
    assert_eq!(manifest["remote_model_requests"], 0);
    let cases = manifest["saved_replay"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    for (golden, fixture) in
        cases
            .iter()
            .zip(["rich/facts-base", "rich/facts-final", "once/facts-final"])
    {
        let fixture = fixture_root.join("history-inputs").join(fixture);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join("config")).unwrap();
        fs::create_dir(root.join("work")).unwrap();
        let settings: Value =
            serde_json::from_slice(&fs::read(fixture.join("config/settings.yaml")).unwrap())
                .unwrap();
        assert!(settings["llm"]["base_url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:"));
        fs::write(
            root.join("config/settings.yaml"),
            json!({
                "paths":{"work_dir":"work"}, "llm":settings["llm"],
            })
            .to_string(),
        )
        .unwrap();
        let config = AppConfig::load_with_env(root, None, &BTreeMap::new()).unwrap();
        let database = root.join("isolated.sqlite3");
        fs::copy(fixture.join("db/reports.sqlite3"), &database).unwrap();
        let now = Utc::now();
        let store = Store::open(&database, now).unwrap();
        let run = store.current("2026-11-10").unwrap().unwrap();
        let id = &run.run_id;
        let audits = store.calls(id).unwrap();
        let call = &audits[0];
        assert_eq!(call["response_sha256"], golden["response_sha256"]);
        let snapshot_ref = SnapshotRef {
            path: run.snapshot_path.clone().unwrap(),
            sha256: run.snapshot_sha256.clone().unwrap(),
        };
        assert_eq!(snapshot_ref.sha256, golden["input_sha256"]);
        let source = fixture.join("data/daily_cache/snapshots").join(id);
        let snapshots = SnapshotStore::new(root.join("snapshots"), 16 * 1024 * 1024).unwrap();
        let isolated_run = snapshots.root.join(id);
        fs::create_dir(&isolated_run).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            fs::copy(entry.path(), isolated_run.join(entry.file_name())).unwrap();
        }
        let input: PersonalInput =
            serde_json::from_value(snapshots.read(&snapshot_ref).unwrap()).unwrap();
        assert_eq!(input.version, golden["version"]);
        let plan = input.plan.as_ref().unwrap();
        let started = Instant::now();
        let runner = ModelRunner {
            store: &store,
            snapshots: &snapshots,
            transport: &Offline,
            llm: &config.llm,
            allow_new_calls: false,
            run_id: id,
            started_at: now,
            started,
            deadline: started + Duration::from_secs(30),
        };
        let generated = generate(&runner, plan).unwrap();
        let mut report = input.report(&config.report.root_alias, Some(Ok(generated)));
        let markdown = input.render_output(&mut report).unwrap();
        let attachment = report.evidence_attachment.unwrap();
        assert_eq!(
            digest(markdown.as_bytes()),
            golden["body_sha256"],
            "{}",
            golden["case"]
        );
        assert_eq!(
            attachment.sha256, golden["attachment_sha256"],
            "{}",
            golden["case"]
        );
        let original = fixture.join("reports/2026-11");
        assert_eq!(
            markdown.as_bytes(),
            fs::read(original.join("2026-11-10.md")).unwrap()
        );
        assert_eq!(
            attachment.markdown.as_bytes(),
            fs::read(original.join(attachment.link("2026-11-10"))).unwrap()
        );
        assert_eq!(store.calls(id).unwrap(), audits);
    }
}
