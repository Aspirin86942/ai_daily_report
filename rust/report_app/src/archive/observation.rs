//! Small, durable capture intent. Observing a path does not mean its body was saved.
use super::{identity, store::ArchiveStore};
use crate::ReportError;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct PendingCapture {
    fingerprint: String,
    changed_at: Option<String>,
}

pub(super) fn observe(
    conn: &Connection,
    document: &str,
    fingerprint: String,
    changed: bool,
    now: &str,
) -> Result<bool, ReportError> {
    let key = format!("capture:{document}");
    let previous: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key=?", [&key], |r| {
            r.get(0)
        })
        .optional()?;
    let previous = previous
        .as_deref()
        .map(serde_json::from_str::<PendingCapture>)
        .transpose()?;
    let changed_at = if previous
        .as_ref()
        .is_some_and(|p| p.fingerprint != fingerprint)
        || changed && previous.as_ref().is_none_or(|p| p.changed_at.is_none())
    {
        Some(now.to_owned())
    } else {
        previous.as_ref().and_then(|p| p.changed_at.clone())
    };
    let pending = PendingCapture {
        fingerprint,
        changed_at,
    };
    let value = serde_json::to_string(&pending)?;
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value<>excluded.value",
        params![key, value],
    )?;
    Ok(pending.changed_at.is_some())
}

pub(super) fn fingerprint(guard: Option<&str>, size: u64, modified: &str) -> String {
    identity(&[guard.unwrap_or(""), &size.to_string(), modified])
}

pub(super) fn captured(conn: &Connection, document: &str) -> Result<(), ReportError> {
    conn.execute(
        "DELETE FROM metadata WHERE key=?",
        [format!("capture:{document}")],
    )?;
    Ok(())
}

/// Record the gap before copying starts, so a deadline or interrupted run does
/// not erase the observation. A later capture resolves it with its actual time.
pub(super) fn record_gap(conn: &Connection, document: &str, run: &str) -> Result<(), ReportError> {
    conn.execute(
        "INSERT INTO gaps(collection_id,path,code,detail)
         SELECT ?1,d.path,'CAPTURE_CONTENT_UNAVAILABLE','发现文件变化，但尚未取得该版本正文。'
         FROM documents d JOIN metadata m ON m.key='capture:'||d.id
         WHERE d.id=?2 AND json_extract(m.value,'$.changed_at') IS NOT NULL
         AND NOT EXISTS(SELECT 1 FROM gaps g WHERE g.collection_id=?1 AND g.path=d.path
             AND g.code='CAPTURE_CONTENT_UNAVAILABLE' AND g.resolved_at IS NULL)",
        params![run, document],
    )?;
    Ok(())
}

pub(crate) fn counts(
    store: &ArchiveStore,
    before: Option<&str>,
) -> Result<(u64, u64), ReportError> {
    let start = before
        .map(|end| {
            chrono::DateTime::parse_from_rfc3339(end)
                .map(|end| (end - chrono::Duration::days(1)).to_rfc3339())
                .map_err(|_| super::error("ARCHIVE_TIME_INVALID"))
        })
        .transpose()?;
    let changes: i64 = store.conn.query_row(
        "SELECT count(*) FROM metadata m JOIN documents d ON m.key='capture:'||d.id
         WHERE json_extract(m.value,'$.changed_at') IS NOT NULL
         AND d.missing_count<2
         AND (?1 IS NULL OR (json_extract(m.value,'$.changed_at')<?1
             AND json_extract(m.value,'$.changed_at')>=?2))",
        params![before, start],
        |r| r.get(0),
    )?;
    let baselines: i64 = store.conn.query_row(
        "SELECT count(*) FROM documents d WHERE d.baseline=1 AND d.current_version IS NULL
         AND d.missing_count<2
         AND NOT EXISTS(SELECT 1 FROM metadata m WHERE m.key='capture:'||d.id
             AND json_extract(m.value,'$.changed_at') IS NOT NULL)",
        [],
        |r| r.get(0),
    )?;
    Ok((changes as u64, baselines as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmed_missing_capture_is_not_a_live_queue_item_and_keeps_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArchiveStore::at(dir.path(), true).unwrap();
        store.conn.execute("INSERT INTO documents(id,root,path,extension,baseline,missing_count) VALUES('d','root','gone.txt','.txt',0,1)",[]).unwrap();
        store.conn.execute("INSERT INTO metadata(key,value) VALUES('capture:d',?)",
            [serde_json::json!({"fingerprint":"saved","changed_at":"2026-10-23T01:00:00+00:00"}).to_string()]).unwrap();
        assert_eq!(counts(&store, None).unwrap().0, 1);
        store
            .conn
            .execute("UPDATE documents SET missing_count=2 WHERE id='d'", [])
            .unwrap();
        assert_eq!(counts(&store, None).unwrap().0, 0);
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM metadata WHERE key='capture:d'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }
}
