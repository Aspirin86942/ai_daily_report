#![cfg(windows)]

use ai_daily_report::{
    archive::{cli as archive_cli, model_input, schedule, store::ArchiveStore},
    cli::{Cli, Command as CliCommand},
    config::AppConfig,
    health,
    run::App,
    store::locking::ReportLock,
    types::{self, Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use chrono::{DateTime, Timelike, Utc};
use clap::Parser;
use rusqlite::params;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path, process::Command, time::Duration};

fn at(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

fn setup() -> (tempfile::TempDir, AppConfig, ArchiveStore) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("config")).unwrap();
    fs::create_dir_all(dir.path().join("work")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        json!({"paths":{"work_dir":dir.path().join("work"),"data_dir":dir.path().join("data"),"db_dir":dir.path().join("data/db"),"reports_dir":dir.path().join("data/reports"),"log_dir":dir.path().join("logs")},"archive":{"collection_interval_minutes":120,"collection_workday_window":{"start":"09:00","end":"18:00"}}}).to_string(),
    ).unwrap();
    let mut config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    let workers = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug");
    config.office_worker_path = workers.join("ai-daily-office-parser.exe");
    config.pdf_worker_path = workers.join("ai-daily-pdf-parser.exe");
    let store = ArchiveStore::open(&config, true).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO metadata VALUES('baseline',?)",
            [at("2026-10-16T00:00:00Z").to_rfc3339()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO metadata VALUES(?, '120')",
            [format!(
                "collection_interval:{}",
                at("2026-10-16T00:00:00Z").to_rfc3339()
            )],
        )
        .unwrap();
    (dir, config, store)
}

fn window(store: &ArchiveStore, since: &str, value: Value) {
    store
        .conn
        .execute(
            "INSERT INTO metadata VALUES(?,?)",
            params![
                format!("collection_window:{}", at(since).to_rfc3339()),
                value.to_string()
            ],
        )
        .unwrap();
}

fn complete(store: &ArchiveStore, when: &str) {
    let timestamp = at(when).to_rfc3339();
    store.conn.execute("INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES(?,?,?,'complete',0)", params![timestamp,timestamp,timestamp]).unwrap();
}

#[test]
fn friday_close_and_monday_open_bridge_planned_nights_and_weekend() {
    let (_dir, _config, store) = setup();
    window(
        &store,
        "2026-10-16T00:00:00Z",
        json!({"start":"09:00","end":"18:00"}),
    );
    for when in [
        "2026-10-16T09:00:00Z",
        "2026-10-16T10:00:00Z",
        "2026-10-19T01:00:00Z",
    ] {
        complete(&store, when);
    }
    let end = at("2026-10-19T01:02:00Z");
    let result = schedule::coverage(&store, at("2026-10-16T09:00:00Z"), end, end).unwrap();
    assert_eq!(result.gaps, 0, "{:?}", result.ranges);
    assert!(result.completed);
}

#[test]
fn closing_tick_and_monday_opening_tick_remain_required() {
    for (missing, expected) in [
        ("2026-10-16T10:00:00Z", "2026-10-16 18:00:00"),
        ("2026-10-19T01:00:00Z", "2026-10-19 09:00:00"),
    ] {
        let (_dir, _config, store) = setup();
        window(
            &store,
            "2026-10-16T00:00:00Z",
            json!({"start":"09:00","end":"18:00"}),
        );
        for when in [
            "2026-10-16T09:00:00Z",
            "2026-10-16T10:00:00Z",
            "2026-10-19T01:00:00Z",
        ] {
            if when != missing {
                complete(&store, when);
            }
        }
        let end = at("2026-10-19T01:02:00Z");
        let result = schedule::coverage(&store, at("2026-10-16T09:00:00Z"), end, end).unwrap();
        assert_eq!(result.gaps, 1, "{:?}", result.ranges);
        assert!(
            result.ranges[0].starts_with(expected),
            "{:?}",
            result.ranges
        );
    }
}

#[test]
fn old_all_day_gaps_survive_later_workday_activation() {
    let (_dir, _config, store) = setup();
    window(
        &store,
        "2026-10-19T01:00:00Z",
        json!({"start":"09:00","end":"18:00"}),
    );
    complete(&store, "2026-10-18T02:00:00Z");
    complete(&store, "2026-10-19T01:00:00Z");
    let end = at("2026-10-19T01:02:00Z");
    let result = schedule::coverage(&store, at("2026-10-18T02:00:00Z"), end, end).unwrap();
    assert_eq!(result.gaps, 1, "{:?}", result.ranges);
    assert!(result.ranges[0].starts_with("2026-10-18 12:00:00"));
}

#[test]
fn policy_boundary_uses_new_tick_and_null_window_restores_all_day() {
    let (_dir, _config, store) = setup();
    window(
        &store,
        "2026-10-16T03:00:00Z",
        json!({"start":"09:00","end":"18:00"}),
    );
    complete(&store, "2026-10-16T02:00:00Z");
    let end = at("2026-10-16T03:02:00Z");
    let result = schedule::coverage(&store, at("2026-10-16T02:00:00Z"), end, end).unwrap();
    assert_eq!(result.gaps, 1, "{:?}", result.ranges);
    assert!(result.ranges[0].starts_with("2026-10-16 11:00:00"));
    complete(&store, "2026-10-16T03:00:00Z");
    assert_eq!(
        schedule::coverage(&store, at("2026-10-16T02:00:00Z"), end, end)
            .unwrap()
            .gaps,
        0
    );
    window(&store, "2026-10-16T10:01:00Z", Value::Null);
    complete(&store, "2026-10-16T10:00:00Z");
    let end = at("2026-10-16T12:02:00Z");
    let result = schedule::coverage(&store, at("2026-10-16T10:00:00Z"), end, end).unwrap();
    assert_eq!(result.gaps, 1);
    assert!(result.ranges[0].starts_with("2026-10-16 20:00:00"));
}

#[test]
fn planned_idle_without_completed_collections_is_consistent_for_health_and_report() {
    let (_dir, config, store) = setup();
    window(
        &store,
        "2026-10-16T00:00:00Z",
        json!({"start":"09:00","end":"18:00"}),
    );
    let start = at("2026-10-16T16:00:00Z");
    let end = at("2026-10-17T16:00:00Z");
    let result = schedule::coverage(&store, start, end, end).unwrap();
    assert!(!result.completed);
    assert!(result.planned_idle);
    assert_eq!(result.gaps, 0, "{:?}", result.ranges);
    let target = start.with_timezone(&types::shanghai()).date_naive();
    let _app = App::open(config.clone(), end).unwrap();
    let health = health::check(&config, target, end).unwrap();
    assert!(
        health
            .checks
            .iter()
            .any(|c| c.code == "COLLECTION_GAP" && c.level == "ok"),
        "{:?}",
        health.checks
    );
    let request = ReportRequest {
        period: Period {
            mode: ReportMode::Daily,
            source: ReportSource::Archive,
            start: target,
            end: target,
            label: target.to_string(),
        },
        context: RunContext::new("a".repeat(32), end, target, String::new()).unwrap(),
        input: String::new(),
        save: false,
        dry_run: true,
        scheduled: false,
        regeneration_reason: None,
    };
    let prepared = model_input::prepare(&config, &request).unwrap();
    assert!(
        !prepared
            .gaps
            .iter()
            .any(|gap| gap.code == "ARCHIVE_COLLECTION_GAP"),
        "{:?}",
        prepared.gaps
    );
    // The same empty database must still report real missing ticks on a working day.
    let working = at("2026-10-19T16:00:00Z");
    let target = at("2026-10-18T16:00:00Z")
        .with_timezone(&types::shanghai())
        .date_naive();
    let health = health::check(&config, target, working).unwrap();
    assert!(health
        .checks
        .iter()
        .any(|c| c.code == "COLLECTION_GAP" && c.level == "error"));
}

fn activate(config: &AppConfig) -> Value {
    let parsed = Cli::try_parse_from(["ai-daily-report", "archive", "activate-schedule"]).unwrap();
    let Some(CliCommand::Archive { command, .. }) = parsed.command else {
        panic!("expected archive command")
    };
    archive_cli::execute(config, command).unwrap()
}

#[test]
fn activation_is_atomic_and_repeating_it_does_not_move_the_effective_time() {
    let (_dir, config, store) = setup();
    store
        .conn
        .execute(
            "DELETE FROM metadata WHERE key GLOB 'collection_interval:*'",
            [],
        )
        .unwrap();
    store.conn.execute_batch("CREATE TRIGGER refuse_window BEFORE INSERT ON metadata WHEN NEW.key GLOB 'collection_window:*' BEGIN SELECT RAISE(ABORT,'synthetic window write failure'); END;").unwrap();
    let parsed = Cli::try_parse_from(["ai-daily-report", "archive", "activate-schedule"]).unwrap();
    let Some(CliCommand::Archive { command, .. }) = parsed.command else {
        panic!()
    };
    assert!(archive_cli::execute(&config, command).is_err());
    let count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM metadata WHERE key GLOB 'collection_*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "interval write survived failed window write");
    store
        .conn
        .execute_batch("DROP TRIGGER refuse_window")
        .unwrap();
    let first = activate(&config);
    let second = activate(&config);
    assert_eq!(first["status"], "activated");
    assert_eq!(second["status"], "unchanged");
    assert_eq!(first["effective_at"], second["effective_at"]);
    assert_eq!(second["interval_minutes"], 120);
    assert_eq!(
        second["collection_workday_window"],
        json!({"start":"09:00","end":"18:00"})
    );
    let count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM metadata WHERE key GLOB 'collection_*'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
    let collections: i64 = store
        .conn
        .query_row("SELECT count(*) FROM collections", [], |r| r.get(0))
        .unwrap();
    assert_eq!(collections, 0, "activating policy performed a collection");
}

#[test]
fn manual_weekend_collection_and_in_window_logon_collection_remain_available() {
    for (scheduled, when) in [
        (false, "2026-10-17T01:00:00Z"),
        (true, "2026-10-19T02:15:00Z"),
    ] {
        let (_dir, config, store) = setup();
        let value = ai_daily_report::archive::collect::execute(&config, false, scheduled, at(when))
            .unwrap();
        assert!(value["run_id"].as_str().is_some());
        assert_ne!(value["action"], "skipped");
        assert_eq!(value["model_calls"], 0);
        assert_eq!(
            store
                .conn
                .query_row("SELECT count(*) FROM collections", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn activation_boundary_keeps_the_one_minute_scheduling_grace() {
    let (_dir, _config, store) = setup();
    window(
        &store,
        "2026-10-16T03:00:00Z",
        json!({"start":"09:00","end":"18:00"}),
    );
    let schedule = schedule::CollectionSchedule::load(&store).unwrap();
    let previous = at("2026-10-16T02:00:00Z");
    assert!(schedule
        .missed_after(previous, at("2026-10-16T03:01:00Z"), previous)
        .is_none());
    assert_eq!(
        schedule.missed_after(previous, at("2026-10-16T03:01:00.001Z"), previous),
        Some(at("2026-10-16T03:00:00Z"))
    );
}

#[test]
fn scheduled_cli_skips_before_archive_lock_or_source_access_and_logs_the_reason() {
    let (_dir, mut config, store) = setup();
    let hour = Utc::now().with_timezone(&types::shanghai()).hour();
    let (start, end) = if hour < 12 {
        ("18:00", "18:01")
    } else {
        ("09:00", "09:01")
    };
    let path = config.config_dir.join("settings.yaml");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    settings["archive"]["collection_workday_window"] = json!({"start":start,"end":end});
    settings["paths"]["work_dir"] = json!(config.paths.work_dir.join("unavailable"));
    fs::write(path, settings.to_string()).unwrap();
    let hash = "a".repeat(64);
    store
        .conn
        .execute(
            "INSERT INTO objects VALUES(?,0,?)",
            params![hash, Utc::now().to_rfc3339()],
        )
        .unwrap();
    store.conn.execute("INSERT INTO extractions(id,sha256,profile,extension,status) VALUES(?,?,'old-pending','.txt','queued')",params![hash,hash]).unwrap();
    let _lock = ReportLock::acquire(
        &config.paths.data_dir.join("archive/collector"),
        "archive",
        Duration::ZERO,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ai-daily-report"))
        .args(["--config-dir"])
        .arg(&config.config_dir)
        .args(["collect", "--scheduled"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["action"], "skipped");
    assert_eq!(value["status"], "skipped");
    assert_eq!(value["reason"], "outside_collection_workday_window");
    assert_eq!(value["model_calls"], 0);
    assert_eq!(
        store
            .conn
            .query_row("SELECT count(*) FROM collections", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conn
            .query_row("SELECT status FROM extractions", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "queued"
    );
    let log = config.paths.log_dir.join(format!(
        "scheduled-{}.jsonl",
        Utc::now()
            .with_timezone(&types::shanghai())
            .format("%Y-%m-%d")
    ));
    let event: Value =
        serde_json::from_str(fs::read_to_string(log).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(event["status"], "skipped");
    assert_eq!(event["reason"], value["reason"]);
    // A skipped first run also leaves an absent archive uninitialized.
    config.paths.data_dir = config.paths.data_dir.join("not-created");
    settings["paths"]["data_dir"] = json!(config.paths.data_dir);
    fs::write(
        config.config_dir.join("settings.yaml"),
        settings.to_string(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ai-daily-report"))
        .arg("--config-dir")
        .arg(&config.config_dir)
        .args(["collect", "--scheduled"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !config.paths.data_dir.exists(),
        "scheduled skip initialized the archive"
    );
}
