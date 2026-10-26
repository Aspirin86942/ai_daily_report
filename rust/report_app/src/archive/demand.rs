//! Versions needed by personal daily reports. No queue writes or source-body reads.
use super::{model_input::permitted, store::ArchiveStore};
use crate::{
    config::{AppConfig, ReportProfile},
    types, ReportError,
};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::params;
use std::{collections::BTreeSet, path::Path, time::Instant};

pub(crate) const MAX_FILES: usize = 1_024;
pub(crate) const PAGE_SIZE: usize = 128;

pub(crate) struct VersionPair {
    pub document: String,
    pub current: String,
    pub previous: Option<String>,
    pub path: String,
}

pub(crate) fn eligible(config: &AppConfig, path: &str) -> bool {
    let path = Path::new(path);
    permitted(config, path)
        && !path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".tmp-"))
}

pub(crate) fn pairs(
    store: &ArchiveStore,
    start: &str,
    end: &str,
    cursor: &str,
) -> Result<Vec<VersionPair>, ReportError> {
    let mut statement = store.conn.prepare(
        "SELECT v.document_id,v.id,
         (SELECT p.id FROM versions p WHERE p.document_id=v.document_id AND p.observed_at<?1
          ORDER BY p.observed_at DESC,p.rowid DESC LIMIT 1),v.path
         FROM versions v WHERE v.observed_at>=?1 AND v.observed_at<?2 AND v.baseline=0
         AND v.document_id>?3 AND NOT EXISTS(
           SELECT 1 FROM versions n WHERE n.document_id=v.document_id AND n.observed_at<?2
           AND (n.observed_at>v.observed_at OR (n.observed_at=v.observed_at AND n.rowid>v.rowid)))
         ORDER BY v.document_id LIMIT ?4",
    )?;
    let rows = statement
        .query_map(params![start, end, cursor, PAGE_SIZE as i64], |row| {
            Ok(VersionPair {
                document: row.get(0)?,
                current: row.get(1)?,
                previous: row.get(2)?,
                path: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// JSON is a bounded SQL membership parameter, not a second persistent queue.
/// Default preparation also includes unresolved report dates within retention.
pub(crate) fn scope(
    config: &AppConfig,
    store: &ArchiveStore,
    before: Option<&str>,
    deadline: Instant,
) -> Result<Option<String>, ReportError> {
    if config.report.profile != ReportProfile::Personal {
        return Ok(None);
    }
    let now = store
        .quota
        .as_ref()
        .map_or_else(Utc::now, |quota| quota.now());
    let mut days = days(before, now)?;
    if before.is_none() {
        days.extend(crate::personal_recovery::material_dates(config, now)?);
        days.sort();
        days.dedup();
    }
    let mut versions = BTreeSet::new();
    for day in days {
        collect_pairs(config, store, day, deadline, Some(MAX_FILES), &mut versions)?;
    }
    Ok(Some(serde_json::to_string(&versions)?))
}

fn days(before: Option<&str>, now: DateTime<Utc>) -> Result<Vec<NaiveDate>, ReportError> {
    Ok(if let Some(end) = before {
        let end =
            DateTime::parse_from_rfc3339(end).map_err(|_| super::error("ARCHIVE_TIME_INVALID"))?;
        vec![(end.with_timezone(&types::shanghai()) - chrono::Duration::days(1)).date_naive()]
    } else {
        let today = now.with_timezone(&types::shanghai()).date_naive();
        vec![today - chrono::Duration::days(1), today]
    })
}

/// Reclamation must finish the complete query. A selection limit is not proof
/// that the unvisited versions are unused; errors leave all candidates intact.
pub(crate) fn protected(
    config: &AppConfig,
    store: &ArchiveStore,
    before: Option<&str>,
    now: DateTime<Utc>,
    deadline: Instant,
) -> Result<BTreeSet<String>, ReportError> {
    let mut versions = BTreeSet::new();
    let mut dates = days(None, now)?;
    dates.extend(crate::personal_recovery::material_dates(config, now)?);
    if before.is_some() {
        dates.extend(days(before, now)?);
    }
    dates.sort();
    dates.dedup();
    for day in dates {
        collect_pairs(config, store, day, deadline, None, &mut versions)?;
        // A changed file can be waiting for its first successful copy. Its
        // existing comparison body must survive reclamation by another job.
        let (start, end) = types::utc_window(day)?;
        let mut statement = store.conn.prepare(
            "SELECT d.path,d.current_version,
             (SELECT v.id FROM versions v WHERE v.document_id=d.id AND v.observed_at<?1
              ORDER BY v.observed_at DESC,v.rowid DESC LIMIT 1)
             FROM metadata m JOIN documents d ON m.key='capture:'||d.id
             WHERE json_extract(m.value,'$.changed_at')>=?1 AND json_extract(m.value,'$.changed_at')<?2",
        )?;
        let mut rows = statement.query(params![start.to_rfc3339(), end.to_rfc3339()])?;
        while let Some(row) = rows.next()? {
            crate::processing_cache::check_deadline(deadline)?;
            if eligible(config, &row.get::<_, String>(0)?) {
                versions.extend(row.get::<_, Option<String>>(1)?);
                versions.extend(row.get::<_, Option<String>>(2)?);
            }
        }
    }
    Ok(versions)
}

fn collect_pairs(
    config: &AppConfig,
    store: &ArchiveStore,
    day: NaiveDate,
    deadline: Instant,
    limit: Option<usize>,
    versions: &mut BTreeSet<String>,
) -> Result<(), ReportError> {
    let (start, end) = types::utc_window(day)?;
    let mut cursor = String::new();
    let mut inspected = 0;
    while limit.is_none_or(|limit| inspected < limit) {
        crate::processing_cache::check_deadline(deadline)?;
        let page = pairs(store, &start.to_rfc3339(), &end.to_rfc3339(), &cursor)?;
        if page.is_empty() {
            break;
        }
        for pair in page {
            crate::processing_cache::check_deadline(deadline)?;
            inspected += 1;
            cursor = pair.document;
            if eligible(config, &pair.path) {
                versions.insert(pair.current);
                versions.extend(pair.previous);
            }
        }
    }
    Ok(())
}
