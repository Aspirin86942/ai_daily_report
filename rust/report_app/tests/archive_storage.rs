#![cfg(windows)]

use ai_daily_report::{
    archive::{collect, maintenance, schedule, storage, store::ArchiveStore},
    config::AppConfig,
    health, operations,
};
use chrono::{DateTime, Duration, Utc};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

fn setup() -> (tempfile::TempDir, AppConfig) {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("work")).unwrap();
    fs::create_dir_all(directory.path().join("config")).unwrap();
    fs::write(
        directory.path().join("config/settings.yaml"),
        "paths:\n  work_dir: work\narchive:\n  parse_workers: 1\n",
    )
    .unwrap();
    let mut config = AppConfig::load_with_env(directory.path(), None, &BTreeMap::new()).unwrap();
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug");
    config.office_worker_path = target.join("ai-daily-office-parser.exe");
    config.pdf_worker_path = target.join("ai-daily-pdf-parser.exe");
    (directory, config)
}

fn time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn unchanged_rounds_and_daily_audits_keep_summaries_without_repeating_details() {
    let (_directory, config) = setup();
    let source = config.paths.work_dir.join("policy.txt");
    fs::write(&source, "approval threshold 50000\n").unwrap();
    let now = time("2026-10-17T02:00:00Z");
    collect::run(&config, true, now).unwrap();
    let warm = collect::run(&config, false, now + Duration::minutes(1)).unwrap();
    assert_eq!(
        (warm.files, warm.unchanged, warm.bytes_read, warm.parse_jobs),
        (1, 1, 0, 0)
    );
    let audit = collect::run(&config, false, now + Duration::days(1)).unwrap();
    assert_eq!(
        (audit.audit_files, audit.unchanged, audit.versions),
        (1, 1, 0)
    );
    let store = ArchiveStore::open(&config, false).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM collections", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM file_metrics", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(store);
    fs::write(&source, "approval threshold 100000\n").unwrap();
    collect::run(
        &config,
        false,
        now + Duration::days(1) + Duration::minutes(1),
    )
    .unwrap();
    let store = ArchiveStore::open(&config, false).unwrap();
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM file_metrics", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(maintenance::verify(&store).unwrap()["status"], "verified");
}

#[test]
fn transparent_compression_preserves_hashes_and_random_reads_and_is_idempotent() {
    let (_directory, mut config) = setup();
    config.archive.compress_extractions = false;
    let text =
        "approval rule: preserve every source range and every original byte.\n".repeat(20000);
    fs::write(config.paths.work_dir.join("policy.txt"), &text).unwrap();
    collect::run(&config, true, Utc::now()).unwrap();
    let store = ArchiveStore::open(&config, false).unwrap();
    let id: String = store
        .conn
        .query_row(
            "SELECT id FROM extractions WHERE status='complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let body = store.extraction_dir(&id).unwrap().join("body.md");
    let original = fs::read(&body).unwrap();
    drop(store);
    config.archive.compress_extractions = true;
    let result = storage::optimize(&config).unwrap();
    assert_eq!(result["status"], "verified");
    assert!(result["reclaimed_bytes"].as_u64().unwrap() > 0);
    let mut file = fs::File::open(&body).unwrap();
    file.seek(SeekFrom::Start(100000)).unwrap();
    let mut sample = [0; 4000];
    file.read_exact(&mut sample).unwrap();
    assert_eq!(sample, original[100000..104000]);
    assert_eq!(fs::read(&body).unwrap(), original);
    assert_eq!(storage::optimize(&config).unwrap()["reclaimed_bytes"], 0);
    assert_eq!(
        maintenance::verify(&ArchiveStore::open(&config, false).unwrap()).unwrap()["status"],
        "verified"
    );
}

#[test]
fn resumed_parse_keeps_its_detail_when_the_source_has_not_changed() {
    let (_directory, mut config) = setup();
    fs::write(
        config.paths.work_dir.join("policy.txt"),
        "preserve queued work\n",
    )
    .unwrap();
    config.archive.system_reserve_mib = u64::MAX;
    let first = collect::run(&config, true, Utc::now()).unwrap();
    assert_eq!((first.versions, first.parse_jobs), (1, 0));
    config.archive.system_reserve_mib = 2048;
    let resumed = collect::run(&config, false, Utc::now()).unwrap();
    assert_eq!((resumed.unchanged, resumed.parse_jobs), (1, 1));
    let store = ArchiveStore::open(&config, false).unwrap();
    let parsed: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM file_metrics WHERE collection_id=? AND parse_ms IS NOT NULL",
            [&resumed.run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parsed, 1);
}

#[test]
fn cadence_changes_preserve_old_gaps_and_do_not_report_planned_two_hour_intervals() {
    let (_directory, mut config) = setup();
    let start = time("2026-10-16T16:00:00Z");
    collect::run(&config, true, start).unwrap();
    let store = ArchiveStore::open(&config, true).unwrap();
    let switch = time("2026-10-17T02:45:00Z");
    schedule::activate(&store, 120, switch).unwrap();
    let insert = |at: DateTime<Utc>| {
        store.conn.execute(
            "INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES(?,?,?,'complete',0)",
            rusqlite::params![at.to_rfc3339(), at.to_rfc3339(), at.to_rfc3339()],
        ).unwrap();
    };
    // One missed half-hour before the change must remain visible afterwards.
    for half_hour in 1..22 {
        if half_hour != 4 {
            insert(start + Duration::minutes(30 * half_hour));
        }
    }
    for hour in (12..48).step_by(2) {
        insert(start + Duration::hours(hour));
    }
    let old = schedule::coverage(
        &store,
        start,
        start + Duration::hours(10),
        start + Duration::days(2),
    )
    .unwrap();
    assert_eq!(old.gaps, 1, "{:?}", old.ranges);
    let current_start = start + Duration::days(1);
    let current_end = current_start + Duration::days(1);
    let current = schedule::coverage(&store, current_start, current_end, current_end).unwrap();
    assert_eq!(current.gaps, 0, "{:?}", current.ranges);
    // Config changes alone cannot rewrite the recorded historical cadence.
    config.archive.collection_interval_minutes = 120;
    let report = health::check(
        &config,
        current_start
            .with_timezone(&ai_daily_report::types::shanghai())
            .date_naive(),
        current_end,
    )
    .unwrap();
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.code == "COLLECTION_GAP" && check.level == "ok"),
        "{:?}",
        report.checks
    );
    store
        .conn
        .execute(
            "DELETE FROM collections WHERE started_at=?",
            [(current_start + Duration::hours(4)).to_rfc3339()],
        )
        .unwrap();
    assert_eq!(
        schedule::coverage(&store, current_start, current_end, current_end)
            .unwrap()
            .gaps,
        1
    );
    let previous = time("2026-10-17T02:30:00Z");
    let new_tick = time("2026-10-17T04:00:00Z");
    assert!(schedule::CollectionSchedule::load(&store)
        .unwrap()
        .missed_after(previous, new_tick, previous)
        .is_none());
}

#[test]
fn collection_task_and_configuration_validate_the_selected_cadence() {
    let (directory, mut config) = setup();
    config.archive.collection_interval_minutes = 0;
    assert!(config.archive.validate().is_err());
    config.archive.collection_interval_minutes = 100;
    assert!(config.archive.validate().is_err());
    config.archive.collection_interval_minutes = 120;
    config.archive.validate().unwrap();
    config.program_dir = directory.path().to_path_buf();
    fs::write(config.program_dir.join("ai-daily-report.exe"), []).unwrap();
    let plan =
        operations::task_plan_kind(&config, "S-1-5-21-123-456-789-1001", Utc::now(), "collect")
            .unwrap();
    assert_eq!(plan["interval_minutes"], 120);
    assert!(plan["xml"]
        .as_str()
        .unwrap()
        .contains("<Interval>PT2H</Interval>"));
}
