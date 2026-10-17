#![cfg(windows)]

use ai_daily_report::{
    archive::{collect, diff, maintenance, store::ArchiveStore},
    config::AppConfig,
};
use chrono::Utc;
use std::{collections::BTreeMap, fs, path::PathBuf};

fn setup() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("work")).unwrap();
    fs::create_dir_all(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: work\narchive:\n  parse_workers: 1\n",
    )
    .unwrap();
    let mut config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug");
    config.office_worker_path = target.join("ai-daily-office-parser.exe");
    config.pdf_worker_path = target.join("ai-daily-pdf-parser.exe");
    (dir, config)
}

#[test]
fn versions_reuse_bytes_and_parses_and_unchanged_does_no_body_io() {
    let (_dir, config) = setup();
    let path = config.paths.work_dir.join("policy.txt");
    fs::write(&path, "审批门槛5万元\n完成状态：待确认\n").unwrap();
    let first = collect::run(&config, true, Utc::now()).unwrap();
    assert_eq!(first.versions, 1);
    assert_eq!(first.parse_jobs, 1);
    let diagnostics = ArchiveStore::open(&config, false).unwrap();
    let details: String = diagnostics
        .conn
        .query_row(
            "SELECT coalesce(group_concat(detail), '') FROM gaps",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(first.errors, 0, "{first:?}; {details}");
    let warm = collect::run(&config, false, Utc::now()).unwrap();
    assert_eq!(
        (
            warm.versions,
            warm.bytes_read,
            warm.parse_jobs,
            warm.worker_starts
        ),
        (0, 0, 0, 0)
    );
    fs::write(&path, "审批门槛10万元\n完成状态：待确认\n").unwrap();
    let second = collect::run(&config, false, Utc::now()).unwrap();
    assert_eq!(
        (second.versions, second.parse_jobs, second.errors),
        (1, 1, 0)
    );
    fs::write(&path, "审批门槛5万元\n完成状态：待确认\n").unwrap();
    let third = collect::run(&config, false, Utc::now()).unwrap();
    assert_eq!(
        (third.versions, third.parse_jobs, third.parse_reused),
        (1, 0, 1)
    );
    let store = ArchiveStore::open(&config, false).unwrap();
    let versions = store.versions(Some(&path.to_string_lossy())).unwrap();
    assert_eq!(versions.len(), 3);
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM objects", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(maintenance::verify(&store).unwrap()["status"], "verified");
    drop(store);
    let store = ArchiveStore::open(&config, true).unwrap();
    store
        .conn
        .execute(
            "UPDATE extractions SET manifest_json=json_set(manifest_json,'$.raw_sha256',?)",
            ["f".repeat(64)],
        )
        .unwrap();
    assert_eq!(maintenance::verify(&store).unwrap()["status"], "failed");
}

#[test]
fn rename_copy_and_missing_require_complete_inventory() {
    let (_dir, config) = setup();
    let first = config.paths.work_dir.join("a.txt");
    let second = config.paths.work_dir.join("b.txt");
    fs::write(&first, "原文\n").unwrap();
    collect::run(&config, true, Utc::now()).unwrap();
    fs::rename(&first, &second).unwrap();
    let moved = collect::run(&config, false, Utc::now()).unwrap();
    assert_eq!(moved.versions, 0);
    fs::copy(&second, &first).unwrap();
    let copy = collect::run(&config, false, Utc::now()).unwrap();
    assert_eq!((copy.versions, copy.parse_jobs), (1, 0));
    fs::remove_file(&first).unwrap();
    collect::run(&config, false, Utc::now()).unwrap();
    let store = ArchiveStore::open(&config, false).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("SELECT max(missing_count) FROM documents", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(store);
    collect::run(&config, false, Utc::now()).unwrap();
    let store = ArchiveStore::open(&config, false).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("SELECT max(missing_count) FROM documents", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM gaps WHERE code='FILE_MISSING' AND resolved_at IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn full_text_exceeds_old_location_limit_and_insert_does_not_change_every_row() {
    let (_dir, config) = setup();
    let path = config.paths.work_dir.join("records.csv");
    let text = format!(
        "编号,金额\n{}",
        (0..800).map(|i| format!("ID{i},{i}\n")).collect::<String>()
    );
    fs::write(&path, &text).unwrap();
    collect::run(&config, true, Utc::now()).unwrap();
    fs::write(&path, text.replace("ID20,20\n", "ID20,20\nNEW,100\n")).unwrap();
    collect::run(&config, false, Utc::now()).unwrap();
    let store = ArchiveStore::open(&config, false).unwrap();
    let (before, after): (String, String) = store
        .conn
        .query_row(
            "SELECT before_extraction,extraction_id FROM changes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let result = diff::compare(&store, Some(&before), &after, &config.archive.table_keys).unwrap();
    assert_eq!(result.hunks.len(), 1);
    assert!(
        store.read_manifest(&after).unwrap()["units"]
            .as_u64()
            .unwrap()
            > 512
    );
}

struct LocalModel;
impl ai_daily_report::model::transport::ModelTransport for LocalModel {
    fn send(
        &self,
        bytes: &[u8],
        _timeout: std::time::Duration,
    ) -> Result<
        ai_daily_report::model::transport::HttpResponse,
        ai_daily_report::model::transport::TransportFailure,
    > {
        use serde_json::{json, Value};
        let wire: Value = serde_json::from_slice(bytes).unwrap();
        let input: Value =
            serde_json::from_str(wire["messages"][1]["content"].as_str().unwrap()).unwrap();
        let response = if let Some(facts) = input["facts"].as_array() {
            let mut matters = vec![];
            let mut pending = vec![];
            let mut overview = vec![];
            for fact in facts {
                let cited = json!({"text":fact["fact"]["detail"],"fact_ids":[fact["fact_id"]],"source_ids":fact["fact"]["source_ids"]});
                if fact["pending"] == true {
                    pending.push(cited);
                } else {
                    if overview.len() < 5 {
                        overview.push(cited.clone());
                    }
                    matters.push(json!({"matter_id":fact["matter_id"],"title":fact["fact"]["topic"],"background":null,"details":[cited],"status":null}));
                }
            }
            json!({"schema_version":4,"overview":overview,"matters":matters,"pending":pending})
        } else {
            let source = &input["sources"][0];
            json!({"schema_version":4,"facts":[{"topic":"审批条件","background":null,"detail":"审批门槛由5万元调整为10万元，生效状态待原文件确认。","conditions":[],"status":null,"source_ids":[source["source_id"]],"uncertainty":null}],"pending":[]})
        };
        Ok(ai_daily_report::model::transport::HttpResponse{status:200,retry_after:None,body:serde_json::to_vec(&json!({"choices":[{"finish_reason":"stop","message":{"content":response.to_string()}}],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150}})).unwrap()})
    }
}

fn request(app: &ai_daily_report::run::App, save: bool) -> ai_daily_report::types::ReportRequest {
    use clap::Parser;
    let command = ai_daily_report::cli::Cli::parse_from(if save {
        vec!["app", "daily", "--source", "archive"]
    } else {
        vec!["app", "daily", "--source", "archive", "--no-save"]
    })
    .command
    .unwrap();
    command
        .report_request(
            &app.config,
            Utc::now(),
            ai_daily_report::types::new_run_id(),
            app.fingerprint(false).unwrap(),
        )
        .unwrap()
}

#[test]
fn archive_daily_preview_publication_evidence_and_permissions() {
    use ai_daily_report::{
        archive::model_input,
        run::{self, App},
    };
    let (_dir, mut config) = setup();
    config.llm.context_window_tokens = Some(128000);
    config.llm.verified_max_output_tokens = Some(32768);
    let path = config.paths.work_dir.join("approval.txt");
    fs::write(&path, "审批门槛5万元\n").unwrap();
    collect::run(&config, true, Utc::now() - chrono::Duration::days(2)).unwrap();
    fs::write(&path, "审批门槛10万元\n").unwrap();
    collect::run(&config, false, Utc::now() - chrono::Duration::days(1)).unwrap();
    let mut app = App::open(config.clone(), Utc::now()).unwrap();
    app.transport = Box::new(LocalModel);
    let preview = run::run_report(request(&app, false), &app).unwrap();
    assert_eq!(preview.action, "preview");
    assert!(preview.markdown.contains("修改前"));
    let frozen = run::prepare_archive_input(&app, &request(&app, false)).unwrap();
    assert_eq!(frozen.version, 4);
    assert_eq!(frozen.plan.version, 5);
    assert!(!frozen.sources.is_empty());
    let published = run::run_report(request(&app, true), &app).unwrap();
    assert_eq!(published.action, "published");
    let stored = app.store.get(&published.run_id).unwrap();
    let report: ai_daily_report::reports::data::DailyReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    assert_eq!(report.schema_version, 4);
    assert_eq!(report.facts[0].evidence.len(), 2);
    model_input::validate_evidence(&app.config, &report.facts).unwrap();
    let mut tampered = report.facts.clone();
    tampered[0].evidence[0].quote = "伪造依据".into();
    assert!(model_input::validate_evidence(&app.config, &tampered).is_err());
    let pending = model_input::prepare(&app.config, &request(&app, false)).unwrap();
    assert!(pending.changes.is_empty());
    let saved = model_input::read_frozen(
        &app.config,
        &published.run_id,
        stored.snapshot_sha256.as_deref().unwrap(),
    )
    .unwrap();
    assert_eq!(saved["version"], 4);
    config.archive.allow_work_dir_for_model = false;
    assert_eq!(
        model_input::validate_permissions(&config, &frozen)
            .unwrap_err()
            .code,
        "ARCHIVE_MODEL_PERMISSION_DENIED"
    );
    let backup = _dir.path().join("backup");
    maintenance::backup(&app.config, &backup).unwrap();
    let restored = ArchiveStore::at(&backup, false).unwrap();
    assert_eq!(
        maintenance::verify(&restored).unwrap()["status"],
        "verified"
    );
}

#[test]
fn archive_dry_run_keeps_frozen_identity_and_existing_report_does_not_create_a_run() {
    use ai_daily_report::{
        run::{self, App},
        snapshots::SnapshotRef,
        types::RunPhase,
    };
    let (_dir, config) = setup();
    collect::run(&config, true, Utc::now() - chrono::Duration::days(2)).unwrap();
    let app = App::open(config, Utc::now()).unwrap();
    let mut preview_request = request(&app, true);
    preview_request.dry_run = true;
    let preview = run::run_daily(preview_request, &app).unwrap();
    let stored = app.store.get(&preview.run_id).unwrap();
    let reference = SnapshotRef {
        path: stored.snapshot_path.unwrap(),
        sha256: stored.snapshot_sha256.unwrap(),
    };
    let snapshot = app.snapshots.read(&reference).unwrap();
    assert!(reference.path.contains("archive-preview"));
    assert_eq!(snapshot["version"], 4);
    assert_eq!(snapshot["plan"]["version"], 5);
    assert_eq!(
        snapshot["input_fingerprint"],
        stored.input_fingerprint.unwrap()
    );
    assert_eq!(snapshot["quality"], stored.quality.unwrap());
    assert_eq!(stored.phase, RunPhase::Prepared);
    assert!(matches!(
        stored.status,
        ai_daily_report::store::RunStatus::Succeeded
    ));
    assert_eq!(preview.quality, "scan_only");
    assert_eq!(
        preview.markdown,
        serde_json::to_string_pretty(&snapshot["sources"]).unwrap()
    );
    assert!(app.store.calls(&preview.run_id).unwrap().is_empty());
    assert!(app.store.current(&preview.target_date).unwrap().is_none());

    let published = run::run_daily(request(&app, true), &app).unwrap();
    let next_request = request(&app, true);
    let unused_run_id = next_request.context.run_id.clone();
    let existing = run::run_daily(next_request, &app).unwrap();
    assert_eq!(existing.action, "existing");
    assert_eq!(existing.run_id, published.run_id);
    assert_eq!(existing.markdown, published.markdown);
    assert_eq!(existing.model_calls, 0);
    assert!(app.store.get(&unused_run_id).is_err());
}

#[test]
fn failed_daily_input_preparation_preserves_the_error_and_failed_run_audit() {
    use ai_daily_report::{
        run::{self, App},
        store::RunStatus,
        types::RunPhase,
    };
    let (_dir, config) = setup();
    ArchiveStore::open(&config, true).unwrap();
    let app = App::open(config, Utc::now()).unwrap();
    let mut request = request(&app, true);
    request.dry_run = true;
    let run_id = request.context.run_id.clone();
    let error = run::run_daily(request, &app).unwrap_err();
    let stored = app.store.get(&run_id).unwrap();
    assert_eq!(error.code, "ARCHIVE_BASELINE_REQUIRED");
    assert_eq!(stored.phase, RunPhase::Scanning);
    assert!(matches!(stored.status, RunStatus::Failed));
    assert_eq!(stored.error_code.as_deref(), Some(error.code));
    assert!(stored.snapshot_path.is_none());
    assert!(app.store.calls(&run_id).unwrap().is_empty());
}

#[test]
fn archive_recovery_skips_old_scan_runs_and_preserves_their_evidence() {
    use ai_daily_report::{
        archive::model_input,
        run::{self, App},
        types::{new_run_id, RunContext},
    };
    let (_dir, config) = setup();
    collect::run(&config, true, Utc::now() - chrono::Duration::days(2)).unwrap();
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(LocalModel);
    let mut next = request(&app, true);
    next.scheduled = true;
    let old = RunContext::new(
        new_run_id(),
        Utc::now() - chrono::Duration::days(1),
        next.context.target_date - chrono::Duration::days(1),
        "old-config".into(),
    )
    .unwrap();
    app.store
        .create(&old, "report", &old.run_id, None, old.started_at)
        .unwrap();
    let reference = app
        .snapshots
        .put(&old.run_id, "input", &serde_json::json!({"version":3}))
        .unwrap();
    app.store
        .prepare(
            &old.run_id,
            &reference,
            &reference.sha256,
            &[],
            old.started_at,
        )
        .unwrap();
    app.store
        .fail(&old.run_id, "MODEL_RESPONSE_INVALID", false, old.started_at)
        .unwrap();
    let old_state = serde_json::to_value(app.store.get(&old.run_id).unwrap()).unwrap();

    let pending_request = request(&app, true);
    let frozen = run::prepare_archive_input(&app, &pending_request).unwrap();
    let archived = &pending_request.context;
    app.store
        .create(
            archived,
            "report",
            &archived.run_id,
            None,
            archived.started_at,
        )
        .unwrap();
    model_input::pin(&app.config, &frozen).unwrap();
    let reference = app
        .snapshots
        .put(
            &archived.run_id,
            "input",
            &serde_json::to_value(&frozen).unwrap(),
        )
        .unwrap();
    app.store
        .prepare(
            &archived.run_id,
            &reference,
            &frozen.input_fingerprint,
            &[],
            archived.started_at,
        )
        .unwrap();
    app.store
        .fail(
            &archived.run_id,
            "RUN_INTERRUPTED",
            false,
            archived.started_at,
        )
        .unwrap();
    let pending = app.store.pending(None).unwrap();
    assert_eq!(
        model_input::recovery_runs(&app.config, pending.clone(), false)
            .unwrap()
            .iter()
            .map(|r| &r.run_id)
            .collect::<Vec<_>>(),
        vec![&old.run_id]
    );
    assert_eq!(
        model_input::recovery_runs(&app.config, pending, true)
            .unwrap()
            .iter()
            .map(|r| &r.run_id)
            .collect::<Vec<_>>(),
        vec![&archived.run_id]
    );

    let result = run::run_report(next, &app).unwrap();
    assert_eq!(result.run_id, archived.run_id);
    assert_eq!(result.action, "published");
    assert_eq!(result.model_calls, 0);
    assert_eq!(
        serde_json::to_value(app.store.get(&old.run_id).unwrap()).unwrap(),
        old_state
    );
    assert_eq!(
        app.snapshots
            .read(&ai_daily_report::snapshots::SnapshotRef {
                path: old_state["snapshot_path"].as_str().unwrap().into(),
                sha256: old_state["snapshot_sha256"].as_str().unwrap().into(),
            })
            .unwrap(),
        serde_json::json!({"version":3})
    );
}

#[test]
fn archive_reports_explain_the_baseline_boundary_without_model_calls() {
    use ai_daily_report::run::{self, App};
    let (_dir, config) = setup();
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(LocalModel);
    let req = request(&app, true);
    let baseline = req.context.window_start_utc + chrono::Duration::hours(6);
    collect::run(&app.config, true, baseline).unwrap();
    let result = run::run_report(req, &app).unwrap();
    assert_eq!(result.model_calls, 0);
    assert_eq!(result.quality, "partial");
    assert!(result.markdown.contains("ARCHIVE_BASELINE_WINDOW"));
    assert!(result.markdown.contains("已有内容不计为新增成果"));
    assert!(result.markdown.contains(
        &baseline
            .with_timezone(&ai_daily_report::types::shanghai())
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    ));
}

#[test]
fn archive_reports_require_an_initialized_baseline() {
    use ai_daily_report::run::{self, App};
    let (_dir, config) = setup();
    ArchiveStore::open(&config, true).unwrap();
    let app = App::open(config, Utc::now()).unwrap();
    assert_eq!(
        run::prepare_archive_input(&app, &request(&app, false))
            .unwrap_err()
            .code,
        "ARCHIVE_BASELINE_REQUIRED"
    );
}

#[test]
fn archive_reports_expose_stopped_failed_and_unfinished_collection_without_model_calls() {
    use ai_daily_report::run::{self, App};
    for case in [
        "stopped",
        "resumed_later",
        "failed",
        "running",
        "finishes_later",
        "no_completed_round",
    ] {
        let (_dir, config) = setup();
        let mut app = App::open(config, Utc::now()).unwrap();
        app.transport = Box::new(LocalModel);
        let req = request(&app, true);
        let start = req.context.window_start_utc;
        let store = ArchiveStore::open(&app.config, true).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO metadata VALUES('baseline',?)",
                [(start - chrono::Duration::days(1)).to_rfc3339()],
            )
            .unwrap();
        for slot in 0..48 {
            if case == "no_completed_round"
                || (matches!(case, "stopped" | "resumed_later") && slot > 28)
            {
                continue;
            }
            let at = start + chrono::Duration::minutes(slot * 30);
            let status = match (case, slot) {
                ("failed", 28) => "interrupted",
                ("running", 28) => "running",
                _ => "complete",
            };
            let finished = if case == "finishes_later" && slot == 47 {
                req.context.started_at + chrono::Duration::seconds(1)
            } else {
                at + chrono::Duration::seconds(2)
            };
            store.conn.execute(
                "INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES(?,?,?,?,0)",
                rusqlite::params![slot.to_string(), at.to_rfc3339(), finished.to_rfc3339(), status],
            ).unwrap();
        }
        if case == "resumed_later" {
            store.conn.execute(
                "INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES('recovered',?,?,'complete',0)",
                rusqlite::params![req.context.window_end_utc.to_rfc3339(), (req.context.window_end_utc + chrono::Duration::seconds(2)).to_rfc3339()],
            ).unwrap();
        }
        drop(store);
        let result = run::run_report(req, &app).unwrap();
        assert_eq!(result.model_calls, 0, "{case}");
        assert_eq!(result.quality, "partial", "{case}");
        assert!(result.markdown.contains("ARCHIVE_COLLECTION_GAP"), "{case}");
        assert!(
            !result.markdown.contains("在目标日期内未发现修改的文件"),
            "{case}"
        );
        if matches!(case, "stopped" | "resumed_later") {
            assert!(result.markdown.contains("14:30:00"), "{case}");
        }
        let saved = app.store.get(&result.run_id).unwrap();
        let report: ai_daily_report::reports::data::DailyReport =
            serde_json::from_str(saved.report_json.as_deref().unwrap()).unwrap();
        assert!(
            report
                .gaps
                .iter()
                .any(|gap| gap.code == "ARCHIVE_COLLECTION_GAP"),
            "{case}"
        );
    }
}

#[test]
fn archive_collection_coverage_uses_the_report_day_and_preserves_partial_rounds() {
    use ai_daily_report::{archive::model_input, run::App};
    let (_dir, config) = setup();
    let app = App::open(config, Utc::now()).unwrap();
    let req = request(&app, false);
    let start = req.context.window_start_utc;
    let store = ArchiveStore::open(&app.config, true).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO metadata VALUES('baseline',?)",
            [(start - chrono::Duration::days(1)).to_rfc3339()],
        )
        .unwrap();
    // No collection has run today. Yesterday's completed half-hourly records still cover yesterday.
    for slot in 0..48 {
        let at = start + chrono::Duration::minutes(slot * 30);
        let status = if slot == 28 { "partial" } else { "complete" };
        store.conn.execute(
            "INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES(?,?,?,?,0)",
            rusqlite::params![slot.to_string(), at.to_rfc3339(), (at + chrono::Duration::seconds(2)).to_rfc3339(), status],
        ).unwrap();
    }
    store
        .gap("28", None, "ARCHIVE_PARSE_REJECTED", "unreadable file")
        .unwrap();
    drop(store);
    let prepared = model_input::prepare(&app.config, &req).unwrap();
    assert!(!prepared
        .gaps
        .iter()
        .any(|gap| gap.code == "ARCHIVE_COLLECTION_GAP"));
    assert!(prepared
        .gaps
        .iter()
        .any(|gap| gap.code == "ARCHIVE_CAPTURE_GAP"));
}

#[test]
fn offline_root_is_not_deletion_and_daily_audit_reads_without_parsing() {
    let (dir, config) = setup();
    let path = config.paths.work_dir.join("a.txt");
    fs::write(&path, "保留原件\n").unwrap();
    let now = Utc::now();
    collect::run(&config, true, now).unwrap();
    let audit = collect::run(&config, false, now + chrono::Duration::days(1)).unwrap();
    assert_eq!(
        (audit.audit_files, audit.parse_jobs, audit.versions),
        (1, 0, 0)
    );
    assert!(audit.bytes_read > 0);
    fs::rename(&config.paths.work_dir, dir.path().join("offline")).unwrap();
    for _ in 0..2 {
        let result = collect::run(&config, false, now).unwrap();
        assert!(result.errors > 0);
    }
    let store = ArchiveStore::open(&config, false).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("SELECT max(missing_count) FROM documents", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(store);
    fs::rename(dir.path().join("offline"), &config.paths.work_dir).unwrap();
    let resumed = collect::run(&config, false, now + chrono::Duration::days(2)).unwrap();
    assert!(resumed
        .warnings
        .iter()
        .any(|v| v == "COLLECTION_INTERVAL_GAP"));
    let store = ArchiveStore::open(&config, false).unwrap();
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM gaps WHERE code='ROOT_UNAVAILABLE' AND resolved_at IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM gaps WHERE code='COLLECTION_INTERVAL_GAP'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
            > 0
    );
}

#[test]
fn busy_baseline_retries_without_turning_existing_content_into_new_work() {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    let (_dir, config) = setup();
    let path = config.paths.work_dir.join("locked.txt");
    fs::write(&path, "基线内容\n").unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    let first = collect::run(&config, true, Utc::now()).unwrap();
    assert_eq!((first.versions, first.errors), (0, 1));
    drop(lock);
    let retry = collect::run(&config, false, Utc::now()).unwrap();
    assert_eq!((retry.versions, retry.errors), (1, 0));
    let store = ArchiveStore::open(&config, false).unwrap();
    assert!(store.versions(None).unwrap()[0].baseline);
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn journal_recovers_observed_bytes_even_after_source_is_overwritten() {
    use serde_json::json;
    let (_dir, config) = setup();
    let path = config.paths.work_dir.join("history.txt");
    fs::write(&path, "A\n").unwrap();
    let first = collect::run(&config, true, Utc::now()).unwrap();
    let store = ArchiveStore::open(&config, true).unwrap();
    let prior = store.versions(None).unwrap().remove(0);
    let raw = b"B\n";
    let hash = ai_daily_report::snapshots::digest(raw);
    let temp = store.root.join("staging/recovered-original");
    fs::write(&temp, raw).unwrap();
    let version = ai_daily_report::archive::store::Version {
        id: "b".repeat(64),
        parent_id: Some(prior.id.clone()),
        sha256: hash,
        observed_at: Utc::now().to_rfc3339(),
        modified_at: Utc::now().to_rfc3339(),
        size_bytes: raw.len() as u64,
        baseline: false,
        ..prior
    };
    fs::write(store.root.join("staging/capture-recovery.json"),serde_json::to_vec(&json!({"version":version,"run":first.run_id,"day":"2000-01-01","fs_id":null,"guard":null,"temp":temp})).unwrap()).unwrap();
    drop(store);
    fs::write(&path, "C\n").unwrap();
    collect::run(&config, false, Utc::now()).unwrap();
    let store = ArchiveStore::open(&config, false).unwrap();
    assert_eq!(store.versions(None).unwrap().len(), 3);
    assert_eq!(
        fs::read(store.object(&version.sha256).unwrap()).unwrap(),
        raw
    );
    assert_eq!(maintenance::verify(&store).unwrap()["status"], "verified");
}

#[test]
fn disk_floor_defers_new_raw_bytes_and_input_limit_keeps_collected_original() {
    let (_dir, mut config) = setup();
    fs::write(config.paths.work_dir.join("small.txt"), "原始正文\n").unwrap();
    config.archive.reserve_free_mib = 1_000_000_000;
    let result = collect::run(&config, true, Utc::now()).unwrap();
    assert_eq!(
        (result.versions, result.bytes_written, result.parse_jobs),
        (0, 0, 0)
    );
    config.archive.reserve_free_mib = 5120;
    config.archive.max_input_mib = 0;
    let result = collect::run(&config, false, Utc::now()).unwrap();
    assert_eq!(result.versions, 1);
    let store = ArchiveStore::open(&config, false).unwrap();
    let version = store.versions(None).unwrap().remove(0);
    store.verify_object(&version.sha256).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("SELECT error_code FROM extractions", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "FILE_TOO_LARGE"
    );
}

#[test]
fn readable_version_after_failed_predecessor_is_reported_as_pending() {
    use ai_daily_report::{
        archive::model_input,
        run::{self, App},
    };
    let (_dir, mut config) = setup();
    config.llm.context_window_tokens = Some(128000);
    config.llm.verified_max_output_tokens = Some(32768);
    let path = config.paths.work_dir.join("repaired.txt");
    fs::write(&path, [0xff, 0xfe, 0xff]).unwrap();
    let baseline = collect::run(&config, true, Utc::now() - chrono::Duration::days(2)).unwrap();
    assert_eq!(baseline.errors, 1);
    fs::write(&path, "审批门槛10万元\n").unwrap();
    collect::run(&config, false, Utc::now() - chrono::Duration::days(1)).unwrap();
    let mut app = App::open(config, Utc::now()).unwrap();
    app.transport = Box::new(LocalModel);
    let prepared = model_input::prepare(&app.config, &request(&app, false)).unwrap();
    assert_eq!(prepared.sources.len(), 1);
    assert_eq!(
        prepared.sources[0].coverage_limits["archive"]["comparison_incomplete"],
        true
    );
    let outcome = run::run_report(request(&app, false), &app).unwrap();
    let stored = app.store.get(&outcome.run_id).unwrap();
    let report: ai_daily_report::reports::data::DailyReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    assert!(report.facts[0].pending);
    assert!(report
        .gaps
        .iter()
        .any(|g| g.code == "ARCHIVE_COMPARISON_INCOMPLETE"));
}

#[test]
fn unchanged_partial_text_does_not_hide_changes_outside_output_limit() {
    use ai_daily_report::{
        archive::{model_input, stats},
        run::App,
    };
    let (_dir, mut config) = setup();
    config.archive.max_output_mib = 1;
    let path = config.paths.work_dir.join("large.txt");
    let mut content = "完整保留原件，正文按预算输出。\n".repeat(100_000);
    fs::write(&path, &content).unwrap();
    collect::run(&config, true, Utc::now() - chrono::Duration::days(2)).unwrap();
    content.push_str("末尾新增审批条件。\n");
    fs::write(&path, content).unwrap();
    collect::run(&config, false, Utc::now() - chrono::Duration::days(1)).unwrap();
    let app = App::open(config, Utc::now()).unwrap();
    let prepared = model_input::prepare(&app.config, &request(&app, false)).unwrap();
    assert_eq!(prepared.continued, 0);
    assert!(prepared.sources.is_empty());
    assert!(prepared
        .gaps
        .iter()
        .any(|g| g.code == "ARCHIVE_COMPARISON_INCOMPLETE"));
    let statistics = stats::query(&app.config, None, None, None).unwrap();
    assert_eq!(statistics["parse_coverage"]["statuses"]["partial"], 2);
    assert_eq!(
        statistics["parse_coverage"]["gap_counts"]["ARCHIVE_OUTPUT_LIMIT"],
        2
    );
    let store = ArchiveStore::open(&app.config, false).unwrap();
    let version = store.versions(None).unwrap().pop().unwrap();
    drop(store);
    ai_daily_report::archive::cli::execute(
        &app.config,
        ai_daily_report::archive::cli::ArchiveCommand::Retry {
            version: version.id,
        },
    )
    .unwrap();
    let waiting = stats::query(&app.config, None, None, None).unwrap();
    assert_eq!(waiting["parse_coverage"]["statuses"]["partial"], 2);
    assert_eq!(waiting["storage"]["parse_queue"], 1);
    maintenance::backup(&app.config, &_dir.path().join("retry-backup")).unwrap();
    let retried = collect::run(&app.config, false, Utc::now()).unwrap();
    assert_eq!(
        (retried.parse_jobs, retried.errors, retried.queued),
        (1, 0, 0)
    );
}

#[test]
fn prune_protects_old_versions_still_waiting_for_parse_resources() {
    let (_dir, mut config) = setup();
    config.archive.system_reserve_mib = u64::MAX;
    let path = config.paths.work_dir.join("queued.txt");
    for (index, body) in ["A", "B", "C"].into_iter().enumerate() {
        fs::write(&path, body).unwrap();
        let collected = collect::run(
            &config,
            index == 0,
            Utc::now() - chrono::Duration::days(3 - index as i64),
        )
        .unwrap();
        assert_eq!(collected.parse_jobs, 0);
        assert_eq!(collected.versions, 1);
    }
    let plan = maintenance::prune_plan(
        &config,
        (Utc::now() + chrono::Duration::days(1)).date_naive(),
    )
    .unwrap();
    assert!(plan["versions"].as_array().unwrap().is_empty());
    assert_eq!(plan["protected"].as_array().unwrap().len(), 3);
}
