//! Historical collection cadence shared by capture, reports and health checks.
use super::{error, store::ArchiveStore};
use crate::ReportError;
use chrono::{DateTime, Duration, Utc};
use rusqlite::params;

const PREFIX: &str = "collection_interval:";
const DEFAULT_MINUTES: u32 = 30;

pub struct CollectionSchedule {
    changes: Vec<(DateTime<Utc>, u32)>,
}

pub struct CollectionCoverage {
    pub completed: bool,
    pub gaps: usize,
    pub ranges: Vec<String>,
}

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

impl CollectionSchedule {
    pub fn load(store: &ArchiveStore) -> Result<Self, ReportError> {
        let mut statement = store
            .conn
            .prepare("SELECT key,value FROM metadata WHERE key GLOB 'collection_interval:*'")?;
        let mut changes = Vec::new();
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (key, value) = row?;
            let at = DateTime::parse_from_rfc3339(&key[PREFIX.len()..])
                .map_err(|_| error("ARCHIVE_TIME_INVALID"))?
                .with_timezone(&Utc);
            let minutes: u32 = value.parse().map_err(|_| error("ARCHIVE_CONFIG_INVALID"))?;
            if !(30..=1440).contains(&minutes) || 1440 % minutes != 0 {
                return Err(error("ARCHIVE_CONFIG_INVALID"));
            }
            changes.push((at, minutes));
        }
        changes.sort_by_key(|(at, _)| *at);
        Ok(Self { changes })
    }

    fn interval_at(&self, at: DateTime<Utc>) -> u32 {
        self.changes
            .iter()
            .rev()
            .find(|(since, _)| *since <= at)
            .map_or(DEFAULT_MINUTES, |(_, minutes)| *minutes)
    }

    /// Triggers are aligned to Shanghai midnight, matching the task definition.
    /// Policy changes cancel old future ticks without rewriting historical gaps.
    pub fn missed_after(
        &self,
        previous: DateTime<Utc>,
        end: DateTime<Utc>,
        start: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let mut cursor = previous.max(start);
        while cursor < end {
            let seconds = i64::from(self.interval_at(cursor)) * 60;
            let local_seconds = cursor.timestamp() + 8 * 3600;
            let aligned = local_seconds.div_euclid(seconds) * seconds - 8 * 3600;
            let mut due = DateTime::from_timestamp(aligned, 0)?;
            if due < cursor || due <= previous {
                due += Duration::seconds(seconds);
            }
            let next_change = self
                .changes
                .iter()
                .find(|(at, _)| *at > cursor)
                .map_or(end, |(at, _)| (*at).min(end));
            if due < next_change && due + Duration::minutes(1) < end {
                return Some(due);
            }
            cursor = next_change;
        }
        None
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
