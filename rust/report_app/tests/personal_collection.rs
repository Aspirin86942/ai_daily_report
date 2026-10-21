#![cfg(windows)]
use ai_daily_report::{
    archive::{collect, queue, selection, store::ArchiveStore},
    config::AppConfig,
    processing_cache::{ProcessingSession, CONTROL_BYTES, MAX_BYTES},
    types::{new_run_id, shanghai, Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

fn setup() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("config")).unwrap();
    fs::create_dir_all(dir.path().join("work")).unwrap();
    let workers = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug");
    fs::write(dir.path().join("config/settings.yaml"),json!({
        "paths":{"work_dir":"work"},"report":{"profile":"personal"},
        "archive":{"daily_source":"archive","parse_workers":1,"read_workers":1},
        "scanner":{"office_worker_path":workers.join("ai-daily-office-parser.exe"),"pdf_worker_path":workers.join("ai-daily-pdf-parser.exe")}
    }).to_string()).unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    (dir, config)
}

fn request(now: DateTime<Utc>, observed: DateTime<Utc>) -> ReportRequest {
    let day = observed.with_timezone(&shanghai()).date_naive();
    ReportRequest {
        context: RunContext::new(new_run_id(), now, day, String::new()).unwrap(),
        period: Period {
            mode: ReportMode::Daily,
            source: ReportSource::Archive,
            start: day,
            end: day,
            label: day.to_string(),
        },
        input: String::new(),
        save: false,
        dry_run: true,
        scheduled: false,
        regeneration_reason: None,
    }
}

fn table(amount: u64) -> String {
    let mut text = "编号,金额（元）,条件,状态\n".to_string();
    for i in 1..=200 {
        text.push_str(&format!(
            "{i},{},{},{}\n",
            amount + i,
            "预算内且复核通过",
            "仍待审批"
        ));
    }
    text
}

#[test]
fn bounds_200_row_changes_preserves_pairs_and_reuses_unchanged_sources() {
    let (_dir, config) = setup();
    let now = Utc::now();
    let before = now - chrono::Duration::days(2);
    let target = now - chrono::Duration::days(1);
    let source = config.paths.work_dir.join("amounts.csv");
    fs::write(&source, table(100)).unwrap();
    let baseline = collect::run(&config, true, before).unwrap();
    assert_eq!(
        (baseline.errors, baseline.versions, baseline.parse_jobs),
        (0, 1, 0),
        "{baseline:?}"
    );
    fs::write(&source, table(200)).unwrap();
    let changed = collect::run(&config, false, target).unwrap();
    assert_eq!(
        (changed.errors, changed.versions, changed.parse_jobs),
        (0, 1, 2),
        "{changed:?}"
    );
    let warm = collect::run(&config, false, target + chrono::Duration::seconds(1)).unwrap();
    assert_eq!(
        (
            warm.bytes_read,
            warm.versions,
            warm.parse_jobs,
            warm.worker_starts
        ),
        (0, 0, 0, 0)
    );
    let request = request(Utc::now() + chrono::Duration::seconds(1), target);
    let selected =
        selection::prepare(&config, &request, Instant::now() + Duration::from_secs(30)).unwrap();
    assert!(!selected.materials.is_empty(), "{selected:?}");
    assert!(selected.summary.selected_chars <= 20_000);
    assert_eq!(selected.summary.selected_files, 1);
    let mut tampered = selected.clone();
    tampered.materials[0].evidence[0].quote.push_str("tampered");
    assert!(tampered.validate().is_err());
    for material in &selected.materials {
        assert!(material.text.contains("金额（元）"));
        assert!(material.text.contains("预算内且复核通过"));
        assert!(material.evidence.iter().any(|e| e.side == "before"));
        assert!(material.evidence.iter().any(|e| e.side == "after"));
    }
    let again =
        selection::prepare(&config, &request, Instant::now() + Duration::from_secs(30)).unwrap();
    assert_eq!(
        serde_json::to_value(&selected).unwrap(),
        serde_json::to_value(again).unwrap()
    );
    let store = ArchiveStore::open_collection(&config, false).unwrap();
    let count: i64 = store
        .conn
        .query_row("SELECT count(*) FROM changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "personal collects versions and defers date-window comparison"
    );
    assert!(!config.paths.data_dir.join("archive").exists());
}

#[test]
fn expired_bodies_keep_current_fingerprint_and_leave_legacy_archive_untouched() {
    let (_dir, config) = setup();
    let now = Utc::now();
    let source = config.paths.work_dir.join("policy.txt");
    fs::write(&source, "金额 100 元，仍待审批。\n").unwrap();
    collect::run(&config, true, now).unwrap();
    let legacy = config.paths.data_dir.join("archive");
    fs::create_dir(&legacy).unwrap();
    fs::write(legacy.join("preserve.txt"), "legacy").unwrap();
    let session = ProcessingSession::at(
        &config.paths.data_dir.join("daily_cache"),
        now + chrono::Duration::days(8),
        Instant::now() + Duration::from_secs(30),
        MAX_BYTES,
        CONTROL_BYTES,
        Duration::ZERO,
    )
    .unwrap();
    let store = session.open_archive(&config).unwrap();
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM documents WHERE current_version IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM versions WHERE pruned_at IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    assert_eq!(
        fs::read_to_string(legacy.join("preserve.txt")).unwrap(),
        "legacy"
    );
    drop(store);
    drop(session);
    let repeated = collect::run(&config, false, now + chrono::Duration::days(8)).unwrap();
    assert_eq!(
        repeated.versions, 0,
        "expiry is not a newly observed business change"
    );
    let changed_at = now + chrono::Duration::days(8) + chrono::Duration::hours(1);
    fs::write(&source, "金额 120 元，仍待审批。\n").unwrap();
    let changed = collect::run(&config, false, changed_at).unwrap();
    assert_eq!(changed.parse_jobs, 1, "{changed:?}");
    let selected = selection::prepare(
        &config,
        &request(changed_at, changed_at),
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    assert!(selected
        .gaps
        .iter()
        .any(|gap| gap.code == "PREVIOUS_CONTENT_UNAVAILABLE"));
    let end = request(changed_at, changed_at)
        .context
        .window_end_utc
        .to_rfc3339();
    assert_eq!(
        queue::pending(&config, Some(&end)).unwrap()["unavailable"],
        1
    );
}

#[test]
fn outside_collection_window_creates_no_processing_directory_and_busy_parse_skips() {
    let (_dir, mut config) = setup();
    config.archive.collection_workday_window =
        Some(ai_daily_report::archive::config::WorkdayWindow {
            start: "09:00".into(),
            end: "18:00".into(),
        });
    let outside = DateTime::parse_from_rfc3339("2026-10-19T16:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        collect::execute(&config, false, true, outside).unwrap()["action"],
        "skipped"
    );
    assert_eq!(
        queue::execute(&config, true, None, 120, outside).unwrap()["action"],
        "skipped"
    );
    assert!(!config.paths.data_dir.join("daily_cache").exists());
    fs::write(config.paths.work_dir.join("file.txt"), "text\n").unwrap();
    collect::run(&config, true, Utc::now()).unwrap();
    let _session = ProcessingSession::acquire(
        &config,
        Utc::now(),
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    let inside = DateTime::parse_from_rfc3339("2026-10-20T02:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        queue::execute(&config, true, None, 120, inside).unwrap()["reason"],
        "processing_busy"
    );
}

#[test]
fn target_day_uses_final_version_excludes_other_days_and_marks_missing_previous() {
    let (_dir, config) = setup();
    let now = Utc::now();
    let day = now - chrono::Duration::days(2);
    let before = day - chrono::Duration::days(1);
    let path = config.paths.work_dir.join("approval.txt");
    let original = "报价金额 100 元，预算内且复核通过后生效，仍待审批。\n";
    fs::write(&path, original).unwrap();
    collect::run(&config, true, before).unwrap();
    fs::write(
        &path,
        "报价金额 120 元，预算内且复核通过后生效，仍待审批。\n",
    )
    .unwrap();
    collect::run(&config, false, day).unwrap();
    fs::write(&path, original).unwrap();
    collect::run(&config, false, day + chrono::Duration::seconds(1)).unwrap();
    fs::write(&path, "次日报价金额 150 元，仍待审批。\n").unwrap();
    collect::run(&config, false, day + chrono::Duration::days(1)).unwrap();
    let input = request(now + chrono::Duration::seconds(10), day);
    let selected =
        selection::prepare(&config, &input, Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(
        selected.materials.is_empty(),
        "reverted intraday changes and next-day changes must not enter the target day"
    );
    let first = config.paths.work_dir.join("first.txt");
    fs::write(first, "首次观察到材料，截止日期待确认。\n").unwrap();
    collect::run(&config, false, day + chrono::Duration::seconds(2)).unwrap();
    let selected =
        selection::prepare(&config, &input, Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(selected
        .gaps
        .iter()
        .any(|g| g.code == "PREVIOUS_CONTENT_UNAVAILABLE"));
    assert!(selected.materials.iter().all(|m| m.source.unverified()));
}

#[test]
fn exhausted_parse_retry_is_not_reset_and_sqlite_control_limits_fail_closed() {
    use ai_daily_report::archive::cli::{execute, ArchiveCommand};
    let (_dir, config) = setup();
    fs::write(config.paths.work_dir.join("a.txt"), "材料\n").unwrap();
    collect::run(&config, true, Utc::now()).unwrap();
    let store = ArchiveStore::open_collection(&config, true).unwrap();
    let id: String = store
        .conn
        .query_row("SELECT id FROM versions LIMIT 1", [], |r| r.get(0))
        .unwrap();
    drop(store);
    execute(
        &config,
        ArchiveCommand::Retry {
            version: id.clone(),
        },
    )
    .unwrap();
    let store = ArchiveStore::open_collection(&config, true).unwrap();
    store
        .conn
        .execute("UPDATE extractions SET attempts=3", [])
        .unwrap();
    drop(store);
    assert_eq!(
        execute(&config, ArchiveCommand::Retry { version: id })
            .unwrap_err()
            .code,
        "PARSE_RETRY_EXHAUSTED"
    );
    let store = ArchiveStore::open_collection(&config, true).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("PRAGMA max_page_count", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        8192
    );
    store
        .conn
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    drop(store);
    assert!(ArchiveStore::open_collection(&config, true).is_err());
}

fn capture_only(
    config: &AppConfig,
    baseline: bool,
    at: DateTime<Utc>,
) -> collect::CollectionMetrics {
    let mut bounded = config.clone();
    bounded.archive.system_reserve_mib = u64::MAX;
    collect::run(&bounded, baseline, at).unwrap()
}

#[test]
fn older_reports_require_explicit_preparation() {
    let (_dir, config) = setup();
    let target = Utc::now() - chrono::Duration::days(3);
    let source = config.paths.work_dir.join("older.txt");
    fs::write(&source, "amount 100\n").unwrap();
    capture_only(&config, true, target - chrono::Duration::days(1));
    fs::write(&source, "amount 120\n").unwrap();
    capture_only(&config, false, target);
    let default = queue::execute(&config, false, None, 120, Utc::now()).unwrap();
    assert_eq!(default["parse_jobs"], 0);
    let explicit = queue::execute(
        &config,
        false,
        Some(target.with_timezone(&shanghai()).date_naive()),
        120,
        Utc::now(),
    )
    .unwrap();
    assert_eq!(explicit["parse_jobs"], 2, "{explicit}");
}

#[test]
fn uncaptured_change_is_visible_with_empty_parse_queue_and_late_capture_keeps_day_gap() {
    let (_dir, config) = setup();
    let now = Utc::now();
    let source = config.paths.work_dir.join("oversized.txt");
    fs::write(&source, "before\n").unwrap();
    capture_only(&config, true, now - chrono::Duration::days(1));
    fs::OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_len(51 * 1024 * 1024)
        .unwrap();
    let result = collect::run(&config, false, now).unwrap();
    assert_eq!(
        (result.versions, result.queued, result.capture_pending),
        (0, 0, 1)
    );
    let day = now.with_timezone(&shanghai()).date_naive();
    let health = ai_daily_report::health::check(&config, day, now).unwrap();
    let check = health
        .checks
        .iter()
        .find(|c| c.code == "PARSE_QUEUE")
        .unwrap();
    assert_eq!(check.level, "warning");
    assert_eq!(check.evidence["capture_pending"], 1);
    let selected = selection::prepare(
        &config,
        &request(now, now),
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    assert!(selected
        .gaps
        .iter()
        .any(|g| g.code == "CAPTURE_CONTENT_UNAVAILABLE"));
    fs::write(&source, "after\n").unwrap();
    let later = now + chrono::Duration::days(1);
    let result = capture_only(&config, false, later);
    assert_eq!((result.versions, result.capture_pending), (1, 0));
    let selected = selection::prepare(
        &config,
        &request(later, now),
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    assert!(selected
        .gaps
        .iter()
        .any(|g| g.code == "CAPTURE_CONTENT_UNAVAILABLE"));
}

#[test]
fn reverted_unparsed_versions_share_one_job_and_emit_no_change() {
    let (_dir, config) = setup();
    let day = Utc::now()
        .with_timezone(&shanghai())
        .date_naive()
        .pred_opt()
        .unwrap();
    let (start, end) = ai_daily_report::types::utc_window(day).unwrap();
    let source = config.paths.work_dir.join("reverted.txt");
    fs::write(&source, "original\n").unwrap();
    capture_only(&config, true, start - chrono::Duration::hours(1));
    fs::write(&source, "intermediate\n").unwrap();
    capture_only(&config, false, start + chrono::Duration::hours(1));
    fs::write(&source, "original\n").unwrap();
    capture_only(&config, false, start + chrono::Duration::hours(2));
    assert_eq!(
        queue::pending(&config, Some(&end.to_rfc3339())).unwrap()["queued"],
        1
    );
    let drained = queue::execute(&config, false, Some(day), 120, Utc::now()).unwrap();
    assert_eq!(drained["parse_jobs"], 1, "{drained}");
    let selection = selection::prepare(
        &config,
        &request(Utc::now(), start),
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    assert!(selection.materials.is_empty());
}

#[test]
fn retryable_failure_is_attempted_once_per_drain() {
    let (_dir, mut config) = setup();
    let day = Utc::now()
        .with_timezone(&shanghai())
        .date_naive()
        .pred_opt()
        .unwrap();
    let (start, _) = ai_daily_report::types::utc_window(day).unwrap();
    capture_only(&config, true, start - chrono::Duration::hours(1));
    fs::write(config.paths.work_dir.join("retry.txt"), "retry\n").unwrap();
    capture_only(&config, false, start + chrono::Duration::hours(1));
    // A zero per-file budget deterministically exercises the retryable timeout path.
    config.archive.parse_seconds = 0;
    for attempts in 1..=3 {
        let drained = queue::execute(&config, false, Some(day), 120, Utc::now()).unwrap();
        assert_eq!(drained["parse_jobs"], 1, "{drained}");
        let store = ArchiveStore::open_collection(&config, false).unwrap();
        assert_eq!(
            store
                .conn
                .query_row("SELECT max(attempts) FROM extractions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            attempts
        );
    }
    let final_pass = queue::execute(&config, false, Some(day), 120, Utc::now()).unwrap();
    assert_eq!(final_pass["parse_jobs"], 0);
    assert_eq!(final_pass["unavailable"], 1);
}

#[test]
fn baseline_stays_idle_and_existing_backlog_drains_only_required_pairs_across_pages() {
    use ai_daily_report::{
        archive::{identity, parse},
        health,
        store::Store,
    };
    let (_dir, config) = setup();
    let today = Utc::now().with_timezone(&shanghai()).date_naive();
    let day = today.pred_opt().unwrap();
    let (start, _) = ai_daily_report::types::utc_window(day).unwrap();
    let baseline_at = start - chrono::Duration::hours(1);
    for index in 0..140 {
        fs::write(
            config.paths.work_dir.join(format!("file-{index:03}.txt")),
            format!("before {index}\n"),
        )
        .unwrap();
    }
    let baseline = collect::run(&config, true, baseline_at).unwrap();
    assert_eq!(
        (
            baseline.versions,
            baseline.parse_jobs,
            baseline.worker_starts,
            baseline.queued
        ),
        (140, 0, 0, 0)
    );
    assert_eq!(
        queue::pending(&config, None).unwrap()["standby_baselines"],
        140
    );
    let store = ArchiveStore::open_collection(&config, true).unwrap();
    let profile = parse::profile(&config).unwrap();
    // Existing installs already have baseline jobs. Preserve these rows unchanged.
    for version in store.versions(None).unwrap() {
        let id = identity(&["extraction", &version.sha256, &version.extension, &profile]);
        store.conn.execute("INSERT INTO extractions(id,sha256,profile,extension,status) VALUES(?,?,?,?,'queued')", rusqlite::params![id,version.sha256,profile,version.extension]).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO version_extractions VALUES(?,?)",
                rusqlite::params![version.id, id],
            )
            .unwrap();
    }
    drop(store);
    Store::open(&config.paths.db_dir.join("reports.sqlite3"), Utc::now()).unwrap();
    let report = health::check(&config, day, Utc::now()).unwrap();
    let check = report
        .checks
        .iter()
        .find(|check| check.code == "PARSE_QUEUE")
        .unwrap();
    assert_eq!(check.level, "ok");
    assert_eq!(check.evidence["queued"], 0);
    assert_eq!(check.evidence["standby_baselines"], 140);
    for index in 0..66 {
        fs::write(
            config.paths.work_dir.join(format!("file-{index:03}.txt")),
            format!("after {index}\n"),
        )
        .unwrap();
    }
    let captured = capture_only(&config, false, start + chrono::Duration::hours(1));
    assert_eq!(captured.parse_jobs, 0);
    assert_eq!(captured.queued, 132);
    let short = queue::execute(&config, false, Some(day), 1, Utc::now()).unwrap();
    assert_eq!(short["parse_jobs"], 0);
    assert_eq!(short["queued"], 132);
    let store = ArchiveStore::open_collection(&config, false).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("SELECT sum(attempts) FROM extractions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(store);
    let drained = queue::execute(&config, false, Some(day), 120, Utc::now()).unwrap();
    assert_eq!(drained["parse_jobs"], 132, "{drained}");
    assert_eq!(drained["queued"], 0);
    assert_eq!(drained["model_calls"], 0);
    assert_eq!(drained["standby_baselines"], 74);
    let store = ArchiveStore::open_collection(&config, false).unwrap();
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM extractions WHERE status='queued' AND attempts=0",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        74
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM versions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        206
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT max(attempts) FROM extractions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(store);
    let empty = queue::execute(&config, false, Some(day), 120, Utc::now()).unwrap();
    assert_eq!(
        (
            empty["parse_jobs"].as_u64(),
            empty["worker_starts"].as_u64()
        ),
        (Some(0), Some(0))
    );
}

#[test]
fn target_preparation_skips_intraday_and_other_day_versions_and_keeps_failures_visible() {
    use ai_daily_report::{archive::parse, health};
    let (_dir, config) = setup();
    let day = Utc::now()
        .with_timezone(&shanghai())
        .date_naive()
        .pred_opt()
        .unwrap();
    let (start, _) = ai_daily_report::types::utc_window(day).unwrap();
    let path = config.paths.work_dir.join("policy.txt");
    fs::write(&path, "amount 100\n").unwrap();
    capture_only(&config, true, start - chrono::Duration::hours(1));
    for (hour, text) in [
        (1, "amount 110\n"),
        (2, "amount 120\n"),
        (25, "amount 130\n"),
    ] {
        fs::write(&path, text).unwrap();
        capture_only(&config, false, start + chrono::Duration::hours(hour));
    }
    let pending = queue::pending(
        &config,
        Some(&(start + chrono::Duration::days(1)).to_rfc3339()),
    )
    .unwrap();
    assert_eq!(pending["queued"], 2);
    let drained = queue::execute(&config, false, Some(day), 120, Utc::now()).unwrap();
    assert_eq!(drained["parse_jobs"], 2, "{drained}");
    let selected = selection::prepare(
        &config,
        &request(Utc::now(), start),
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    let text = selected
        .materials
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("100") && text.contains("120"), "{selected:?}");
    assert!(!text.contains("110") && !text.contains("130"));
    let store = ArchiveStore::open_collection(&config, true).unwrap();
    let profile = parse::profile(&config).unwrap();
    store
        .conn
        .execute(
            "UPDATE extractions SET status='failed',attempts=3 WHERE profile=? AND status='queued'",
            [&profile],
        )
        .unwrap();
    drop(store);
    let health = health::check(&config, day, Utc::now()).unwrap();
    let check = health
        .checks
        .iter()
        .find(|c| c.code == "PARSE_QUEUE")
        .unwrap();
    assert_eq!(check.level, "warning");
    assert!(check.evidence["unavailable"].as_u64().unwrap() > 0);
}
