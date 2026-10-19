use ai_daily_report::{
    archive::config::{ArchiveSettings, WorkdayWindow},
    config::AppConfig,
    operations, ReportError,
};
use chrono::{DateTime, NaiveTime, TimeZone, Timelike, Utc};
use serde_json::Value;
use std::{collections::BTreeMap, fs};

const SID: &str = "S-1-5-21-123";
const WEEKDAYS: &str =
    "<DaysOfWeek><Monday/><Tuesday/><Wednesday/><Thursday/><Friday/></DaysOfWeek>";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T03:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn time(value: &str) -> NaiveTime {
    NaiveTime::parse_from_str(value, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M:%S"))
        .unwrap()
}

fn window(start: &str, end: &str) -> WorkdayWindow {
    WorkdayWindow {
        start: start.into(),
        end: end.into(),
    }
}

fn shanghai_at(day: u32, clock: &str) -> DateTime<Utc> {
    let clock = time(clock);
    ai_daily_report::types::shanghai()
        .with_ymd_and_hms(2026, 10, day, clock.hour(), clock.minute(), clock.second())
        .unwrap()
        .with_timezone(&Utc)
}

/// Synthetic config directory; no worker, model or production path is touched.
fn load(archive_lines: &str) -> Result<(tempfile::TempDir, AppConfig), ReportError> {
    let dir = tempfile::Builder::new()
        .prefix("collection window ")
        .tempdir()
        .unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::create_dir(dir.path().join("work")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        format!(
            "paths:\n  work_dir: work\nllm:\n  max_request_utf8_bytes: 64000\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\narchive:\n{archive_lines}"
        ),
    )
    .unwrap();
    fs::write(dir.path().join("ai-daily-report.exe"), b"task-plan-fixture").unwrap();
    let config = AppConfig::load_with_env(
        dir.path(),
        None,
        &BTreeMap::from([("DEEPSEEK_API_KEY".into(), "synthetic-key".into())]),
    )?;
    Ok((dir, config))
}

fn fixture(archive_lines: &str) -> (tempfile::TempDir, AppConfig) {
    load(archive_lines).unwrap()
}

fn collect_plan(config: &AppConfig) -> Value {
    operations::task_plan_kind(config, SID, now(), "collect").unwrap()
}

#[test]
fn default_config_keeps_full_day_interval_trigger() {
    // No `collection_workday_window`: the target contract carries an explicit null
    // and the historical all-day repetition trigger verbatim.
    let (_dir, config) = fixture("  collection_interval_minutes: 120\n");
    assert_eq!(config.archive.collection_workday_window, None);
    let plan = collect_plan(&config);
    let xml = plan["xml"].as_str().unwrap();
    assert_eq!(plan.get("collection_workday_window"), Some(&Value::Null));
    assert_eq!(plan["interval_minutes"], 120);
    assert!(xml.contains(
        "<CalendarTrigger id=\"CollectInterval\"><StartBoundary>2026-10-15T00:00:00+08:00</StartBoundary><Enabled>true</Enabled><Repetition><Interval>PT2H</Interval><Duration>P1D</Duration><StopAtDurationEnd>false</StopAtDurationEnd></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>"
    ));
    assert!(xml.contains("<LogonTrigger id=\"LogonCatchup\">"));
}

#[test]
fn configured_workday_window_generates_six_weekday_triggers() {
    // 09:00, 11:00, 13:00, 15:00, 17:00, 18:00 for a 120 minute interval.
    let (_dir, config) = fixture(
        "  collection_interval_minutes: 120\n  collection_workday_window:\n    start: \"09:00\"\n    end: \"18:00\"\n",
    );
    let plan = collect_plan(&config);
    let xml = plan["xml"].as_str().unwrap();
    assert_eq!(plan["collection_workday_window"]["start"], "09:00");
    assert_eq!(plan["collection_workday_window"]["end"], "18:00");
    assert_eq!(xml.matches("<CalendarTrigger").count(), 6);
    assert!(!xml.contains("<Repetition>"));
    assert!(!xml.contains("<ScheduleByDay>"));
    for clock in ["09:00", "11:00", "13:00", "15:00", "17:00", "18:00"] {
        let expected = format!(
            "<StartBoundary>2026-10-15T{clock}:00+08:00</StartBoundary><Enabled>true</Enabled><ScheduleByWeek><WeeksInterval>1</WeeksInterval>{WEEKDAYS}</ScheduleByWeek>"
        );
        assert!(xml.contains(&expected), "missing trigger for {clock}");
    }
    // No weekend day is part of the weekly schedule.
    assert!(!xml.contains("<Saturday/>"));
    assert!(!xml.contains("<Sunday/>"));
    assert!(xml.contains("<LogonTrigger id=\"LogonCatchup\">"));
}

#[test]
fn ticks_follow_timestamp_grid_and_append_end_without_duplicates() {
    let day = window("09:00", "18:00");
    assert_eq!(
        day.ticks(120).unwrap(),
        ["09:00", "11:00", "13:00", "15:00", "17:00", "18:00"]
            .map(time)
            .to_vec()
    );
    assert_eq!(
        day.ticks(180).unwrap(),
        ["09:00", "12:00", "15:00", "18:00"].map(time).to_vec()
    );
    assert_eq!(day.ticks(30).unwrap().len(), 19);
    assert_eq!(
        window("09:10", "09:45").ticks(30).unwrap(),
        ["09:10", "09:40", "09:45"].map(time).to_vec()
    );
    assert_eq!(
        window("09:00", "09:30").ticks(30).unwrap(),
        [time("09:00"), time("09:30")].to_vec()
    );
    // Interval range follows the existing archive constraint.
    assert_eq!(day.ticks(0).unwrap_err().code, "ARCHIVE_CONFIG_INVALID");
    assert_eq!(day.ticks(100).unwrap_err().code, "ARCHIVE_CONFIG_INVALID");
    assert_eq!(
        window("18:00", "09:00").ticks(120).unwrap_err().code,
        "ARCHIVE_CONFIG_INVALID"
    );
}

#[test]
fn allows_uses_shanghai_weekdays_and_includes_the_end_minute() {
    let day = window("09:00", "18:00");
    // 2026-10-16 is Friday, 17 Saturday, 18 Sunday, 19 Monday.
    let cases = [
        (16, "08:59:59", false),
        (16, "09:00:00", true),
        (16, "18:00:00", true),
        (16, "18:00:59", true),
        (16, "18:01:00", false),
        (17, "10:00:00", false),
        (18, "10:00:00", false),
        (19, "08:59:59", false),
        (19, "09:00:00", true),
    ];
    for (day_of_month, clock, expected) in cases {
        assert_eq!(
            day.allows(shanghai_at(day_of_month, clock)),
            expected,
            "2026-10-{day_of_month} {clock}"
        );
    }
    // Malformed or inverted windows fail closed.
    assert!(!window("18:00", "09:00").allows(shanghai_at(16, "10:00:00")));
    assert!(!window("9:00", "18:00").allows(shanghai_at(16, "10:00:00")));
}

#[test]
fn malformed_or_unknown_window_configuration_is_rejected() {
    for archive_lines in [
        "  collection_workday_window:\n    start: \"9:00\"\n    end: \"18:00\"\n",
        "  collection_workday_window:\n    start: \"09:00\"\n    end: \"18:00:00\"\n",
        "  collection_workday_window:\n    start: \"18:00\"\n    end: \"09:00\"\n",
        "  collection_workday_window:\n    start: \"09:00\"\n",
        "  collection_workday_window:\n    start: \"09:00\"\n    end: \"18:00\"\n    weekdays: true\n",
    ] {
        assert!(load(archive_lines).is_err(), "accepted: {archive_lines}");
    }
    let mut settings = ArchiveSettings::default();
    assert!(settings.validate().is_ok());
    settings.collection_workday_window = Some(window("18:00", "09:00"));
    assert_eq!(
        settings.validate().unwrap_err().code,
        "ARCHIVE_CONFIG_INVALID"
    );
    assert_eq!(
        window("18:00", "09:00").validate().unwrap_err().code,
        "ARCHIVE_CONFIG_INVALID"
    );
}

#[test]
fn invalid_interval_with_window_fails_closed_at_plan_time() {
    let (_dir, mut config) = fixture(
        "  collection_interval_minutes: 120\n  collection_workday_window:\n    start: \"09:00\"\n    end: \"18:00\"\n",
    );
    config.archive.collection_interval_minutes = 100;
    assert_eq!(
        operations::task_plan_kind(&config, SID, now(), "collect")
            .unwrap_err()
            .code,
        "ARCHIVE_CONFIG_INVALID"
    );
    // Daily and health plans keep their original contract regardless of the window.
    let daily = operations::task_plan_kind(&config, SID, now(), "daily").unwrap();
    assert!(daily["xml"].as_str().unwrap().contains("09:00:00+08:00"));
    let health = operations::task_plan_kind(&config, SID, now(), "health").unwrap();
    assert!(!health["xml"].as_str().unwrap().contains("CollectWorkday"));
}
