//! Historical collection policies shared by capture, reports and health checks.
use super::{config::ArchiveSettings, config::WorkdayWindow, error, store::ArchiveStore};
use crate::ReportError;
use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use rusqlite::params;
use serde_json::{json, Value};

const PREFIX: &str = "collection_interval:";
const WINDOW_PREFIX: &str = "collection_window:";
const DEFAULT_MINUTES: u32 = 30;

pub struct CollectionSchedule {
    changes: Vec<(DateTime<Utc>, u32)>,
    windows: Vec<(DateTime<Utc>, Option<WorkdayWindow>)>,
}

pub struct CollectionCoverage {
    pub completed: bool,
    pub planned_idle: bool,
    pub gaps: usize,
    pub ranges: Vec<String>,
}

/// Legacy interval-only activation preserves the recorded window history.
pub fn activate(store: &ArchiveStore, minutes: u32, now: DateTime<Utc>) -> Result<(), ReportError> {
    if !(30..=1440).contains(&minutes) || 1440 % minutes != 0 {
        return Err(error("ARCHIVE_CONFIG_INVALID"));
    }
    if CollectionSchedule::load(store)?.interval_at(now) != minutes {
        store.conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?,?)",
            params![format!("{PREFIX}{}", now.to_rfc3339()), minutes.to_string()],
        )?;
    }
    Ok(())
}

/// Record both parts of the policy atomically, without collecting or parsing.
pub fn activate_config(
    store: &ArchiveStore,
    settings: &ArchiveSettings,
    now: DateTime<Utc>,
) -> Result<Value, ReportError> {
    let minutes = settings.collection_interval_minutes;
    if !(30..=1440).contains(&minutes) || 1440 % minutes != 0 {
        return Err(error("ARCHIVE_CONFIG_INVALID"));
    }
    if let Some(window) = &settings.collection_workday_window {
        window.validate()?;
    }
    let tx = store.conn.unchecked_transaction()?;
    let schedule = CollectionSchedule::load(store)?;
    let interval_changed = schedule.interval_at(now) != settings.collection_interval_minutes;
    let window_changed = schedule.window_at(now) != settings.collection_workday_window.as_ref();
    if interval_changed {
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES(?,?)",
            params![
                format!("{PREFIX}{}", now.to_rfc3339()),
                settings.collection_interval_minutes.to_string()
            ],
        )?;
    }
    if window_changed {
        tx.execute(
            "INSERT INTO metadata(key,value) VALUES(?,?)",
            params![
                format!("{WINDOW_PREFIX}{}", now.to_rfc3339()),
                serde_json::to_string(&settings.collection_workday_window)?
            ],
        )?;
    }
    let changed = interval_changed || window_changed;
    let effective_at = if changed {
        Some(now)
    } else {
        schedule
            .changes
            .iter()
            .map(|(at, _)| *at)
            .chain(schedule.windows.iter().map(|(at, _)| *at))
            .filter(|at| *at <= now)
            .max()
    };
    tx.commit()?;
    Ok(json!({
        "status": if changed { "activated" } else { "unchanged" },
        "interval_minutes": settings.collection_interval_minutes,
        "collection_workday_window": settings.collection_workday_window,
        "effective_at": effective_at,
        "model_calls": 0,
    }))
}

impl CollectionSchedule {
    pub fn load(store: &ArchiveStore) -> Result<Self, ReportError> {
        let mut statement = store.conn.prepare(
            "SELECT key,value FROM metadata WHERE key GLOB 'collection_interval:*' OR key GLOB 'collection_window:*'",
        )?;
        let mut changes = Vec::new();
        let mut windows = Vec::new();
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (key, value) = row?;
            let prefix = if key.starts_with(PREFIX) {
                PREFIX
            } else {
                WINDOW_PREFIX
            };
            let at = DateTime::parse_from_rfc3339(&key[prefix.len()..])
                .map_err(|_| error("ARCHIVE_TIME_INVALID"))?
                .with_timezone(&Utc);
            if prefix == PREFIX {
                let minutes: u32 = value.parse().map_err(|_| error("ARCHIVE_CONFIG_INVALID"))?;
                if !(30..=1440).contains(&minutes) || 1440 % minutes != 0 {
                    return Err(error("ARCHIVE_CONFIG_INVALID"));
                }
                changes.push((at, minutes));
            } else {
                let window: Option<WorkdayWindow> = serde_json::from_str(&value)?;
                if let Some(window) = &window {
                    window.validate()?;
                }
                windows.push((at, window));
            }
        }
        changes.sort_by_key(|(at, _)| *at);
        windows.sort_by_key(|(at, _)| *at);
        Ok(Self { changes, windows })
    }

    fn interval_at(&self, at: DateTime<Utc>) -> u32 {
        self.changes
            .iter()
            .rev()
            .find(|(since, _)| *since <= at)
            .map_or(DEFAULT_MINUTES, |(_, minutes)| *minutes)
    }

    fn window_at(&self, at: DateTime<Utc>) -> Option<&WorkdayWindow> {
        self.windows
            .iter()
            .rev()
            .find(|(since, _)| *since <= at)
            .and_then(|(_, window)| window.as_ref())
    }

    fn next_change(&self, at: DateTime<Utc>, end: DateTime<Utc>) -> DateTime<Utc> {
        self.changes
            .iter()
            .map(|(at, _)| *at)
            .chain(self.windows.iter().map(|(at, _)| *at))
            .filter(|change| *change > at)
            .min()
            .unwrap_or(end)
            .min(end)
    }

    fn next_tick(&self, cursor: DateTime<Utc>, previous: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let minutes = self.interval_at(cursor);
        let Some(window) = self.window_at(cursor) else {
            let seconds = i64::from(minutes) * 60;
            let local_seconds = cursor.timestamp() + 8 * 3600;
            let aligned = local_seconds.div_euclid(seconds) * seconds - 8 * 3600;
            let mut due = DateTime::from_timestamp(aligned, 0)?;
            if due < cursor || due <= previous {
                due += Duration::seconds(seconds);
            }
            return Some(due);
        };
        let ticks = window.ticks(minutes).expect("validated collection policy");
        let zone = crate::types::shanghai();
        let date = cursor.with_timezone(&zone).date_naive();
        for offset in 0..=7 {
            let day = date.checked_add_signed(Duration::days(offset))?;
            if day.weekday().number_from_monday() > 5 {
                continue;
            }
            for tick in &ticks {
                let due = zone
                    .from_local_datetime(&day.and_time(*tick))
                    .single()?
                    .with_timezone(&Utc);
                if due >= cursor && due > previous {
                    return Some(due);
                }
            }
        }
        None
    }

    /// Apply each policy only in its half-open historical interval.
    fn next_expected(
        &self,
        previous: DateTime<Utc>,
        end: DateTime<Utc>,
        start: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let mut cursor = previous.max(start);
        while cursor < end {
            let change = self.next_change(cursor, end);
            if let Some(due) = self.next_tick(cursor, previous) {
                if due < change {
                    return Some(due);
                }
            }
            cursor = change;
        }
        None
    }

    pub fn missed_after(
        &self,
        previous: DateTime<Utc>,
        end: DateTime<Utc>,
        start: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        self.next_expected(previous, end, start)
            .filter(|due| *due + Duration::minutes(1) < end)
    }

    fn planned_idle(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
        if start >= end
            || self
                .next_expected(start - Duration::nanoseconds(1), end, start)
                .is_some()
        {
            return false;
        }
        let mut cursor = start;
        while cursor < end {
            if self.window_at(cursor).is_none() {
                return false;
            }
            cursor = self.next_change(cursor, end);
        }
        true
    }
}

pub fn coverage(
    store: &ArchiveStore,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<CollectionCoverage, ReportError> {
    let schedule = CollectionSchedule::load(store)?;
    let mut result = CollectionCoverage {
        completed: false,
        planned_idle: schedule.planned_idle(start, end),
        gaps: 0,
        ranges: Vec::new(),
    };
    let mut statement = store.conn.prepare(
        "WITH completed AS (
            SELECT started_at FROM collections
            WHERE status IN ('complete','partial') AND started_at<?2 AND finished_at<=?3
        ) SELECT started_at FROM completed
          WHERE started_at>=?1 OR started_at=(SELECT max(started_at) FROM completed WHERE started_at<?1)
          ORDER BY started_at",
    )?;
    let rows = statement.query_map(
        params![start.to_rfc3339(), end.to_rfc3339(), now.to_rfc3339()],
        |row| row.get::<_, String>(0),
    )?;
    let mut previous = start - Duration::minutes(i64::from(schedule.interval_at(start)));
    let mut record = |from, to| {
        if let Some(due) = schedule.missed_after(from, to, start) {
            result.gaps += 1;
            if result.ranges.len() < 5 {
                result.ranges.push(format!(
                    "{} 至 {}",
                    due.with_timezone(&crate::types::shanghai())
                        .format("%Y-%m-%d %H:%M:%S"),
                    to.with_timezone(&crate::types::shanghai())
                        .format("%Y-%m-%d %H:%M:%S")
                ));
            }
        }
    };
    for row in rows {
        let at = DateTime::parse_from_rfc3339(&row?)
            .map_err(|_| error("ARCHIVE_TIME_INVALID"))?
            .with_timezone(&Utc);
        if at >= start {
            record(previous, at);
        }
        previous = at;
        result.completed = true;
    }
    record(previous, end);
    Ok(result)
}
