//! Versions needed by personal daily reports. No queue writes or source-body reads.
use super::{model_input::permitted, store::ArchiveStore};
use crate::{
    config::{AppConfig, ReportProfile},
    types, ReportError,
};
use chrono::{DateTime, NaiveDate};
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
/// Without a target, prepare each retained day's final version and its pre-day version.
pub(crate) fn scope(
    config: &AppConfig,
    store: &ArchiveStore,
    before: Option<&str>,
    deadline: Instant,
) -> Result<Option<String>, ReportError> {
    if config.report.profile != ReportProfile::Personal {
        return Ok(None);
    }
    let days = if let Some(end) = before {
        let end =
            DateTime::parse_from_rfc3339(end).map_err(|_| super::error("ARCHIVE_TIME_INVALID"))?;
        vec![(end.with_timezone(&types::shanghai()) - chrono::Duration::days(1)).date_naive()]
    } else {
        let mut statement = store.conn.prepare(
            "SELECT DISTINCT date(observed_at,'+8 hours') FROM versions
             WHERE baseline=0 AND pruned_at IS NULL ORDER BY 1 DESC",
        )?;
        let dates = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        dates
            .into_iter()
            .map(|day| {
                NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                    .map_err(|_| super::error("ARCHIVE_TIME_INVALID"))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut versions = BTreeSet::new();
    for day in days {
        let (start, end) = types::utc_window(day)?;
        let mut cursor = String::new();
        let mut inspected = 0;
        while inspected < MAX_FILES {
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
    }
    Ok(Some(serde_json::to_string(&versions)?))
}
