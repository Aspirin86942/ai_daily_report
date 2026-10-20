//! Personal processing retention. This entry point requires the processing lock.
use super::{error, store::ArchiveStore};
use crate::{
    processing_cache::{check_deadline, MAX_INPUT_BYTES, MAX_OUTPUT_BYTES},
    ReportError,
};
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::{path::Path, time::Instant};

pub fn prune(
    store: &mut ArchiveStore,
    now: DateTime<Utc>,
    deadline: Instant,
) -> Result<(), ReportError> {
    let quota = store
        .quota
        .clone()
        .ok_or_else(|| error("PROCESSING_LOCK_REQUIRED"))?;
    let expired = quota.expired()?;
    for path in expired {
        check_deadline(deadline)?;
        release(store, &path, now)?;
        quota.remove(&path)?;
    }
    // Free completed old materials before admitting the next fixed-size writer.
    let headroom = MAX_INPUT_BYTES + MAX_OUTPUT_BYTES;
    if quota.available_bytes()? < headroom {
        for path in quota.reclaim_candidates()? {
            check_deadline(deadline)?;
            if quota.available_bytes()? >= headroom {
                break;
            }
            release(store, &path, now)?;
            quota.remove(&path)?;
        }
    }
    let cutoff = (now - chrono::Duration::days(7)).to_rfc3339();
    loop {
        check_deadline(deadline)?;
        let ids = {
            let mut statement = store.conn.prepare(
                "SELECT id FROM versions WHERE pruned_at IS NOT NULL
                 AND id NOT IN(SELECT current_version FROM documents WHERE current_version IS NOT NULL)
                 AND id NOT IN(SELECT version_id FROM report_versions) LIMIT 128")?;
            let rows = statement
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        if ids.is_empty() {
            break;
        }
        let tx = store.conn.transaction()?;
        for id in ids {
            tx.execute(
                "DELETE FROM changes WHERE version_id=?1 OR before_version=?1",
                [&id],
            )?;
            tx.execute(
                "UPDATE versions SET parent_id=NULL WHERE parent_id=?",
                [&id],
            )?;
            tx.execute("DELETE FROM version_extractions WHERE version_id=?", [&id])?;
            tx.execute("DELETE FROM versions WHERE id=?", [&id])?;
        }
        tx.commit()?;
    }
    // Current path/hash state survives body expiry; historical diagnostics do not.
    for sql in [
        "DELETE FROM file_metrics WHERE collection_id IN(SELECT id FROM collections WHERE started_at<?)",
        "DELETE FROM gaps WHERE collection_id IN(SELECT id FROM collections WHERE started_at<?)",
        "DELETE FROM paths WHERE observed_at<? AND path NOT IN(SELECT path FROM documents)",
        "DELETE FROM collections WHERE started_at<? AND id NOT IN(SELECT collection_id FROM versions)",
    ] {
        check_deadline(deadline)?;
        let (table, condition) = sql.strip_prefix("DELETE FROM ").unwrap().split_once(" WHERE ").unwrap();
        let bounded = format!("DELETE FROM {table} WHERE rowid IN(SELECT rowid FROM {table} WHERE {condition} LIMIT 128)");
        while store.conn.execute(&bounded, [&cutoff])? == 128 {
            check_deadline(deadline)?;
        }
    }
    for (table, condition) in [
        ("version_extractions", "extraction_id IN(SELECT id FROM extractions WHERE status='expired')"),
        ("extractions", "status='expired' AND id NOT IN(SELECT extraction_id FROM version_extractions)"),
        ("objects", "sha256 NOT IN(SELECT sha256 FROM versions) AND sha256 NOT IN(SELECT sha256 FROM extractions)"),
    ] {
        let sql = format!("DELETE FROM {table} WHERE rowid IN(SELECT rowid FROM {table} WHERE {condition} LIMIT 128)");
        loop {
            check_deadline(deadline)?;
            if store.conn.execute(&sql, [])? < 128 { break; }
        }
    }
    store.conn.pragma_update(None, "incremental_vacuum", 128)?;
    Ok(())
}

fn release(store: &mut ArchiveStore, path: &Path, now: DateTime<Utc>) -> Result<(), ReportError> {
    let path = crate::processing_cache::normalized(path);
    let root = crate::processing_cache::normalized(&store.root);
    let relative = path.strip_prefix(&root).ok();
    let Some(relative) = relative else {
        return Ok(());
    }; // snapshots have no archive references
    let parts: Vec<_> = relative.components().collect();
    let area = parts.first().map(|p| p.as_os_str().to_string_lossy());
    let id = path.file_name().unwrap_or_default().to_string_lossy();
    let at = now.to_rfc3339();
    let tx = store.conn.transaction()?;
    match area.as_deref() {
        Some("objects") => {
            tx.execute(
                "UPDATE versions SET pruned_at=? WHERE sha256=? AND pruned_at IS NULL",
                params![at, id],
            )?;
            tx.execute("DELETE FROM metadata WHERE key LIKE 'retry:%' AND value IN(SELECT id FROM extractions WHERE sha256=?)", [&*id])?;
            tx.execute("UPDATE extractions SET status='expired',error_code='PROCESSING_CONTENT_EXPIRED' WHERE sha256=? AND status IN('queued','running','failed')", [&*id])?;
        }
        Some("extractions") => {
            tx.execute(
                "DELETE FROM changes WHERE extraction_id=?1 OR before_extraction=?1",
                [&*id],
            )?;
            tx.execute("DELETE FROM metadata WHERE key=?", [format!("retry:{id}")])?;
            tx.execute("UPDATE extractions SET status='expired',manifest_json=NULL,error_code='PROCESSING_CONTENT_EXPIRED' WHERE id=?", [&*id])?;
        }
        Some("report_evidence" | "prune_plans") => {
            return Err(error("PROCESSING_UNEXPECTED_EVIDENCE"))
        }
        _ => {}
    }
    tx.commit()?;
    Ok(())
}
