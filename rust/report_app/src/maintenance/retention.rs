use crate::{
    run::App,
    snapshots::{digest, reject_redirects, valid_hash},
    store::{locking::ReportLock, RunRecord, RunStatus},
    types::RunPhase,
    ReportError,
};
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::PathBuf, time::Duration};
#[derive(Debug, Default, Serialize)]
pub struct RetentionResult {
    pub deleted_files: usize,
    pub reclaimed_bytes: u64,
    pub deleted_runs: usize,
    pub preserved_runs: usize,
    pub planned_files: usize,
    pub dry_run: bool,
}
struct Candidate {
    run_id: String,
    files: Vec<(PathBuf, u64, String)>,
    delete_run: bool,
}
fn ids(value: &Value, key: &str, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    if let Some(id) = v.as_str() {
                        into.insert(id.into());
                    }
                }
                ids(v, key, into);
            }
        }
        Value::Array(items) => {
            for item in items {
                ids(item, key, into)
            }
        }
        _ => {}
    }
}
/// Operates only on known, expired snapshot files. All path/hash checks precede deletion.
pub fn cleanup(
    app: &App,
    now: DateTime<Utc>,
    execute: bool,
) -> Result<RetentionResult, ReportError> {
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs(5),
    )?;
    reject_redirects(&app.snapshots.root, "RETENTION_PATH_REDIRECTED")?;
    let conn = app.store.connection()?;
    let run_ids = conn
        .prepare("SELECT run_id FROM report_runs ORDER BY run_id")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let runs = run_ids
        .iter()
        .map(|id| app.store.get(id))
        .collect::<Result<Vec<RunRecord>, _>>()?;
    let mut current=conn.prepare("SELECT current_run_id FROM daily_reports WHERE current_run_id IS NOT NULL UNION SELECT value FROM report_metadata WHERE key LIKE 'current_period:%'")?.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<BTreeSet<_>,_>>()?;
    for run in &runs {
        if let Some(id) = &run.previous_run_id {
            current.insert(id.clone());
        }
    }
    let mut protected: BTreeSet<_> = runs
        .iter()
        .filter(|r| {
            matches!(r.status, RunStatus::Running)
                || r.regeneration_reason
                    .as_deref()
                    .is_some_and(|s| s.starts_with("coverage_candidate:"))
                || matches!(
                    r.phase,
                    RunPhase::Prepared | RunPhase::Generating | RunPhase::DbCommitted
                ) && !matches!(r.status, RunStatus::Succeeded)
                    && r.error_code.as_deref() != Some("RUN_SUPERSEDED")
        })
        .map(|r| r.run_id.clone())
        .collect();
    let metadata = conn
        .prepare("SELECT key,value FROM report_metadata")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (key, text) in &metadata {
        if key.starts_with("job_budget_revision:") || key.starts_with("job_calls_revision:") {
            let value: Value = serde_json::from_str(text)?;
            for id in value["revision"]["retained_call_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64)
            {
                let run: Option<String> = conn
                    .query_row(
                        "SELECT run_id FROM report_model_calls WHERE call_id=?",
                        [id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(run) = run {
                    protected.insert(run);
                }
            }
        }
        if let Some(job) = key.strip_prefix("backfill_budget:") {
            if metadata
                .iter()
                .any(|(k, _)| k == &format!("backfill_complete:{job}"))
            {
                continue;
            }
            let value: Value = serde_json::from_str(text)?;
            let baseline = value["baseline_call_id"]
                .as_i64()
                .ok_or_else(|| ReportError::new("RETENTION_BUDGET_INVALID", "补跑消耗起点无效"))?;
            let referenced = conn
                .prepare("SELECT DISTINCT run_id FROM report_model_calls WHERE call_id>?")?
                .query_map([baseline], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            protected.extend(referenced);
        }
    }
    // Cross-generation references remain readable while their owner still has an input snapshot.
    for run in &runs {
        if let (Some(path), Some(sha)) = (&run.snapshot_path, &run.snapshot_sha256) {
            let reference = crate::snapshots::SnapshotRef {
                path: path.clone(),
                sha256: sha.clone(),
            };
            match app.snapshots.read(&reference) {
                Ok(value) => {
                    for field in ["recovery_inputs", "reused_calls", "prior_calls"] {
                        if let Some(value) = value.get(field) {
                            ids(value, "run_id", &mut protected);
                        }
                    }
                }
                Err(_) => {
                    protected.insert(run.run_id.clone());
                }
            }
        }
    }
    let generations: BTreeSet<_> = runs
        .iter()
        .filter(|r| protected.contains(&r.run_id))
        .map(|r| r.generation_id.clone())
        .collect();
    protected.extend(
        runs.iter()
            .filter(|r| generations.contains(&r.generation_id))
            .map(|r| r.run_id.clone()),
    );
    let file_cutoff =
        now - chrono::Duration::days(app.config.retention.input_snapshot_days.min(365000) as i64);
    let run_cutoff =
        now - chrono::Duration::days(app.config.retention.terminal_run_days.min(365000) as i64);
    let mut candidates = Vec::new();
    let mut result = RetentionResult {
        dry_run: !execute,
        ..Default::default()
    };
    for run in runs {
        let Some(finished) = run
            .finished_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        else {
            continue;
        };
        if protected.contains(&run.run_id) || finished >= file_cutoff {
            result.preserved_runs += 1;
            continue;
        }
        if run.run_id.len() != 32
            || !run
                .run_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(ReportError::new(
                "RETENTION_RUN_PATH_INVALID",
                "运行目录身份无效",
            ));
        }
        let directory = app.snapshots.root.join(&run.run_id);
        reject_redirects(&directory, "RETENTION_PATH_REDIRECTED")?;
        let mut files = Vec::new();
        let mut preserve = false;
        if directory.exists() {
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let path = entry.path();
                reject_redirects(&path, "RETENTION_PATH_REDIRECTED")?;
                if !entry.file_type()?.is_file() {
                    preserve = true;
                    break;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let expected = name
                    .strip_suffix(".json")
                    .and_then(|s| s.rsplit_once('-'))
                    .filter(|(kind, hash)| {
                        !kind.is_empty()
                            && kind.bytes().all(|b| {
                                b.is_ascii_lowercase()
                                    || b.is_ascii_digit()
                                    || b == b'_'
                                    || b == b'-'
                            })
                            && valid_hash(hash)
                    })
                    .map(|(_, hash)| hash.to_owned());
                let Some(expected) = expected else {
                    preserve = true;
                    break;
                };
                let bytes = super::files::read_bytes(&path, 8 * 1024 * 1024)?;
                if digest(&bytes) != expected {
                    preserve = true;
                    break;
                }
                files.push((path, bytes.len() as u64, expected));
            }
        }
        if preserve {
            result.preserved_runs += 1;
            continue;
        }
        result.planned_files += files.len();
        candidates.push(Candidate {
            run_id: run.run_id.clone(),
            files,
            delete_run: finished < run_cutoff && !current.contains(&run.run_id),
        });
    }
    if !execute {
        return Ok(result);
    }
    use rusqlite::TransactionBehavior;
    for candidate in candidates {
        for (path, size, hash) in &candidate.files {
            reject_redirects(path, "RETENTION_PATH_REDIRECTED")?;
            if crate::store::file_sha(path)? != *hash {
                return Err(ReportError::new(
                    "RETENTION_FILE_CHANGED",
                    "清理前文件已被修改",
                ));
            }
            fs::remove_file(path)?;
            result.deleted_files += 1;
            result.reclaimed_bytes += size;
        }
        let directory = app.snapshots.root.join(&candidate.run_id);
        if directory.exists() && fs::read_dir(&directory)?.next().is_none() {
            fs::remove_dir(&directory)?;
        }
        let mut conn = app.store.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE report_runs SET snapshot_path=NULL,snapshot_sha256=NULL WHERE run_id=?",
            [&candidate.run_id],
        )?;
        tx.execute("UPDATE report_model_calls SET request_path=NULL,request_sha256=NULL,response_path=NULL,response_sha256=NULL WHERE run_id=?",[&candidate.run_id])?;
        if candidate.delete_run {
            tx.execute(
                "DELETE FROM report_runs WHERE run_id=?",
                params![candidate.run_id],
            )?;
            result.deleted_runs += 1;
        }
        tx.commit()?;
    }
    Ok(result)
}
use rusqlite::OptionalExtension;
