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
        (0, 1, 1),
        "{baseline:?}"
    );
    fs::write(&source, table(200)).unwrap();
    let changed = collect::run(&config, false, target).unwrap();
    assert_eq!(
        (changed.errors, changed.versions, changed.parse_jobs),
        (0, 1, 1),
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
