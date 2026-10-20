#![cfg(windows)]

use ai_daily_report::{
    archive::{collect, config::WorkdayWindow, queue, schedule, store::ArchiveStore},
    config::AppConfig,
    store::locking::ReportLock,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

fn setup() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::create_dir(dir.path().join("work")).unwrap();
    let workers = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug");
    let settings = json!({
        "paths": {"work_dir": "work"},
        "scanner": {
            "office_worker_path": workers.join("ai-daily-office-parser.exe"),
            "pdf_worker_path": workers.join("ai-daily-pdf-parser.exe")
        },
        "archive": {"parse_workers": 1},
        "llm": {"max_request_utf8_bytes":64000,"context_window_tokens":128000,"verified_max_output_tokens":32768}
    });
    fs::write(
        dir.path().join("config/settings.yaml"),
        settings.to_string(),
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    (dir, config)
}

fn capture_without_parsing(config: &AppConfig, baseline: bool, observed: DateTime<Utc>) {
    let mut low_memory = config.clone();
    // Exercise the real resource gate deterministically, without changing global memory state.
    low_memory.archive.system_reserve_mib = u64::MAX;
    let captured = collect::run(&low_memory, baseline, observed).unwrap();
    assert_eq!(captured.parse_jobs, 0);
    assert!(captured
        .warnings
        .iter()
        .any(|code| code == "MEMORY_RESERVE"));
}

#[test]
fn drain_processes_durable_copy_without_source_access_or_new_capture() {
    let (_dir, config) = setup();
    let source = config.paths.work_dir.join("policy.txt");
    fs::write(&source, "审批门槛十万元，方案仍待确认。\n").unwrap();
    capture_without_parsing(&config, true, at("2026-10-19T07:00:00Z"));
    fs::remove_file(&source).unwrap();
    fs::remove_dir(&config.paths.work_dir).unwrap();
    let result = queue::execute(&config, false, None, 30, Utc::now()).unwrap();
    assert_eq!(result["parse_jobs"], 1);
    assert_eq!(result["queued"], 0);
    assert_eq!(result["versions"], 0);
    assert_eq!(result["bytes_read"], 0);
    assert_eq!(result["material_status"], "complete");
    assert_eq!(result["model_calls"], 0);
    let store = ArchiveStore::open(&config, false).unwrap();
    assert_eq!(store.versions(None).unwrap().len(), 1);
    let capture_count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM collections WHERE status IN ('complete','partial')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(capture_count, 1);
    let parse_count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM collections WHERE status='parse_complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(parse_count, 1);
    let view =
        ai_daily_report::archive::stats::query(&config, None, None, result["run_id"].as_str())
            .unwrap();
    assert_eq!(
        view["summed_parallel_work_ms"]["parse_ms"],
        result["parse_work_ms"]
    );
}

#[test]
fn target_date_drains_only_eligible_observed_material() {
    let (_dir, config) = setup();
    fs::write(
        config.paths.work_dir.join("yesterday.txt"),
        "昨天完成的记录\n",
    )
    .unwrap();
    capture_without_parsing(&config, true, at("2026-10-18T07:00:00Z"));
    fs::write(config.paths.work_dir.join("today.txt"), "今天新增的材料\n").unwrap();
    capture_without_parsing(&config, false, at("2026-10-19T07:00:00Z"));
    let result = queue::execute(
        &config,
        false,
        Some(NaiveDate::from_ymd_opt(2026, 10, 18).unwrap()),
        30,
        Utc::now(),
    )
    .unwrap();
    assert_eq!(result["parse_jobs"], 1);
    assert_eq!(result["queued"], 0);
    let store = ArchiveStore::open(&config, false).unwrap();
    let pending: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM extractions WHERE status='queued'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 1, "today's material must remain pending");
}

#[test]
fn scheduled_drain_outside_window_has_no_archive_side_effects() {
    let (_dir, mut config) = setup();
    config.archive.collection_workday_window = Some(WorkdayWindow {
        start: "09:00".into(),
        end: "18:00".into(),
    });
    for time in ["2026-10-19T10:01:00Z", "2026-10-17T02:00:00Z"] {
        let result = queue::execute(&config, true, None, 30, at(time)).unwrap();
        assert_eq!(result["action"], "skipped");
        assert_eq!(result["model_calls"], 0);
        assert!(!config.paths.data_dir.join("archive").exists());
    }
}

#[test]
fn scheduled_drain_skips_busy_archive_without_mutating_queue() {
    let (_dir, config) = setup();
    fs::write(config.paths.work_dir.join("queued.txt"), "等待解析的原文\n").unwrap();
    capture_without_parsing(&config, true, at("2026-10-19T07:00:00Z"));
    let _lock = ReportLock::acquire(
        &config.paths.data_dir.join("archive/collector"),
        "archive",
        Duration::ZERO,
    )
    .unwrap();
    let result = queue::execute(&config, true, None, 30, at("2026-10-19T09:30:00Z")).unwrap();
    assert_eq!(result["action"], "skipped");
    assert_eq!(result["reason"], "archive_busy");
    assert_eq!(result["worker_starts"], 0);
}

#[test]
fn morning_waits_for_materials_then_allows_explicit_partial_at_cutoff() {
    let (_dir, mut config) = setup();
    fs::write(
        config.paths.work_dir.join("queued.txt"),
        "昨天观察到的待解析原文\n",
    )
    .unwrap();
    capture_without_parsing(&config, true, at("2026-10-18T07:00:00Z"));
    config.archive.system_reserve_mib = u64::MAX;
    let target = NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
    let waiting = queue::prepare_daily(&config, target, at("2026-10-19T01:00:00Z")).unwrap();
    assert_eq!(waiting["action"], "deferred");
    assert_eq!(waiting["queued"], 1);
    assert_eq!(waiting["model_calls"], 0);
    let cutoff = queue::prepare_daily(&config, target, at("2026-10-19T01:30:00Z")).unwrap();
    assert_eq!(cutoff["action"], "ready");
    assert_eq!(cutoff["material_status"], "partial");
    assert_eq!(cutoff["queued"], 1);
    assert_eq!(cutoff["model_calls"], 0);
}

#[test]
fn scheduled_preparation_refreshes_the_freeze_time_after_new_parses() {
    use ai_daily_report::{
        cli::Cli,
        run::{self, App},
        types,
    };
    use clap::Parser;
    let (_dir, config) = setup();
    let now = Utc::now();
    let target = types::target_date(now, None, true, config.schedule.due_time().unwrap()).unwrap();
    // Before the scheduled report time the due report covers two calendar days ago.
    let observed = types::utc_window(target).unwrap().0 + chrono::Duration::hours(12);
    let source = config.paths.work_dir.join("progress.txt");
    fs::write(&source, "原有审批条件。\n").unwrap();
    capture_without_parsing(&config, true, observed - chrono::Duration::days(1));
    fs::write(&source, "原有审批条件。\n新增十万元复核条件。\n").unwrap();
    capture_without_parsing(&config, false, observed);
    let app = App::open(config, Utc::now()).unwrap();
    let mut request = Cli::parse_from(["app", "daily", "--scheduled", "--source", "archive"])
        .command
        .unwrap()
        .report_request(&app.config, now, types::new_run_id(), String::new())
        .unwrap();
    let initial = request.context.started_at;
    let prepared = run::prepare_scheduled_archive(&app, &mut request)
        .unwrap()
        .unwrap();
    assert_eq!(prepared["action"], "ready");
    assert!(request.context.started_at >= initial);
    let frozen = run::prepare_archive_input(&app, &request).unwrap();
    assert!(
        !frozen.sources.is_empty(),
        "newly parsed evidence must enter this freeze"
    );
    assert!(
        app.store.list_runs(None, 100).unwrap().is_empty(),
        "preparation must not create a report or call a model"
    );
}

#[test]
fn existing_frozen_run_skips_reprocessing_its_archive_input() {
    use ai_daily_report::{
        cli::Cli,
        run::{self, App},
        types,
    };
    use clap::Parser;
    let (_dir, config) = setup();
    fs::write(config.paths.work_dir.join("queued.txt"), "已保存的原文。\n").unwrap();
    capture_without_parsing(&config, true, Utc::now() - chrono::Duration::days(1));
    let app = App::open(config, Utc::now()).unwrap();
    let mut request = Cli::parse_from(["app", "daily", "--scheduled", "--source", "archive"])
        .command
        .unwrap()
        .report_request(&app.config, Utc::now(), types::new_run_id(), String::new())
        .unwrap();
    app.store
        .create(
            &request.context,
            "report",
            &request.context.run_id,
            None,
            Utc::now(),
        )
        .unwrap();
    let snapshot = app
        .snapshots
        .put(&request.context.run_id, "test-frozen", &json!({}))
        .unwrap();
    app.store
        .prepare(
            &request.context.run_id,
            &snapshot,
            &"a".repeat(64),
            &[],
            Utc::now(),
        )
        .unwrap();
    assert!(run::prepare_scheduled_archive(&app, &mut request)
        .unwrap()
        .is_none());
    assert_eq!(queue::pending(&app.config, None).unwrap()["queued"], 1);
}

#[test]
fn extracted_material_without_its_change_is_not_report_ready() {
    use ai_daily_report::{cli::Cli, run::App, types};
    use clap::Parser;
    let (_dir, config) = setup();
    let source = config.paths.work_dir.join("progress.txt");
    fs::write(&source, "基线条文。\n").unwrap();
    collect::run(&config, true, at("2026-10-17T02:00:00Z")).unwrap();
    fs::write(&source, "基线条文。\n新增审批条件。\n").unwrap();
    collect::run(&config, false, at("2026-10-18T02:00:00Z")).unwrap();
    fs::write(&source, "基线条文。\n新增审批条件。\n次日新增条件。\n").unwrap();
    collect::run(&config, false, at("2026-10-19T02:00:00Z")).unwrap();
    let store = ArchiveStore::open(&config, true).unwrap();
    store.conn.execute("DELETE FROM changes", []).unwrap(); // emulate a stop between extraction and diff
    assert_eq!(queue::pending(&config, None).unwrap()["queued"], 0);
    assert_eq!(queue::pending(&config, None).unwrap()["pending_changes"], 2);
    let lock = ReportLock::acquire(
        &config.paths.data_dir.join("archive/collector"),
        "archive",
        Duration::ZERO,
    )
    .unwrap();
    let target = chrono::NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
    let waiting = queue::prepare_daily(&config, target, at("2026-10-19T01:00:00Z")).unwrap();
    assert_eq!(waiting["action"], "deferred");
    let cutoff = queue::prepare_daily(&config, target, at("2026-10-19T01:30:00Z")).unwrap();
    assert_eq!(cutoff["material_status"], "partial");
    let app = App::open(config.clone(), Utc::now()).unwrap();
    let request = Cli::parse_from([
        "app",
        "daily",
        "--source",
        "archive",
        "--target-date",
        "2026-10-18",
    ])
    .command
    .unwrap()
    .report_request(&app.config, Utc::now(), types::new_run_id(), String::new())
    .unwrap();
    let material = ai_daily_report::archive::model_input::prepare(&app.config, &request).unwrap();
    assert!(material
        .gaps
        .iter()
        .any(|gap| gap.code == "ARCHIVE_PARSE_PENDING"));
    drop(lock);
    let drained = queue::execute(&config, false, Some(target), 120, Utc::now()).unwrap();
    assert_eq!(drained["parse_jobs"], 0);
    assert_eq!(drained["pending_changes"], 0);
    assert_eq!(drained["material_status"], "complete");
    assert_eq!(
        queue::pending(&config, None).unwrap()["pending_changes"],
        1,
        "future-day changes must stay untouched"
    );
}

#[test]
fn parse_only_run_cannot_fill_a_missing_capture_window() {
    let (_dir, config) = setup();
    fs::write(
        config.paths.work_dir.join("queued.txt"),
        "保存原件后延期解析\n",
    )
    .unwrap();
    capture_without_parsing(&config, true, at("2026-10-18T07:00:00Z"));
    queue::execute(&config, false, None, 30, at("2026-10-19T07:00:00Z")).unwrap();
    let store = ArchiveStore::open(&config, false).unwrap();
    let coverage = schedule::coverage(
        &store,
        at("2026-10-19T00:00:00Z"),
        at("2026-10-19T08:00:00Z"),
        Utc::now(),
    )
    .unwrap();
    assert!(
        coverage.gaps > 0,
        "parsing stored bytes is not a new source inventory"
    );
}
