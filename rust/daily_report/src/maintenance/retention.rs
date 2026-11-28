use crate::{
    infra::{
        fs::{read_bounded_bytes, reject_redirects},
        json::{digest, valid_hash, valid_run_id},
        locking::ReportLock,
    },
    run::App,
    snapshots::SnapshotRef,
    store::{
        queries::{
            all_metadata, all_run_records, clear_run_snapshot_and_calls,
            current_referenced_run_ids, is_personal_run_meta, open_job_call_run_ids,
        },
        RunStatus,
    },
    types::RunPhase,
    ReportError,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    time::Duration,
};
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
fn collect_protected_runs(
    app: &App,
    conn: &rusqlite::Connection,
    runs: &[crate::store::RunRecord],
    metadata: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>, ReportError> {
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
    let mut retained_calls = BTreeSet::new();
    let mut oldest_open_job = i64::MAX;
    for (key, text) in metadata {
        if key.starts_with("job_budget_revision:") || key.starts_with("job_calls_revision:") {
            let value: Value = serde_json::from_str(text)?;
            retained_calls.extend(
                value["revision"]["retained_call_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_i64),
            );
        } else if let Some(job) = key.strip_prefix("backfill_budget:") {
            if !metadata.contains_key(&format!("backfill_complete:{job}")) {
                let value: Value = serde_json::from_str(text)?;
                let baseline = value["baseline_call_id"].as_i64().ok_or_else(|| {
                    ReportError::new("RETENTION_BUDGET_INVALID", "补跑消耗起点无效")
                })?;
                oldest_open_job = oldest_open_job.min(baseline);
            }
        }
    }
    protected.extend(open_job_call_run_ids(
        conn,
        oldest_open_job,
        &serde_json::to_string(&retained_calls)?,
    )?);
    // Cross-generation references remain readable while their owner still has an input snapshot.
    for run in runs {
        if is_personal_run_meta(metadata, &run.run_id) {
            continue;
        }
        if let (Some(path), Some(sha)) = (&run.snapshot_path, &run.snapshot_sha256) {
            let reference = SnapshotRef {
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
    Ok(protected)
}

fn collect_candidates(
    app: &App,
    runs: Vec<crate::store::RunRecord>,
    current: &BTreeSet<String>,
    protected: &BTreeSet<String>,
    metadata: &BTreeMap<String, String>,
    now: DateTime<Utc>,
    result: &mut RetentionResult,
) -> Result<Vec<Candidate>, ReportError> {
    let file_cutoff =
        now - chrono::Duration::days(app.config.retention.input_snapshot_days.min(365000) as i64);
    let run_cutoff =
        now - chrono::Duration::days(app.config.retention.terminal_run_days.min(365000) as i64);
    let mut candidates = Vec::new();
    for run in runs {
        if is_personal_run_meta(metadata, &run.run_id) {
            result.preserved_runs += 1;
            continue;
        }
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
        if !valid_run_id(&run.run_id) {
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
                let bytes = read_bounded_bytes(
                    &path,
                    8 * 1024 * 1024,
                    "MAINTENANCE_PATH_REDIRECTED",
                    "MAINTENANCE_FILE_LIMIT",
                )?;
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
    Ok(candidates)
}

fn execute_candidates(
    app: &App,
    candidates: Vec<Candidate>,
    now: DateTime<Utc>,
    result: &mut RetentionResult,
) -> Result<(), ReportError> {
    if let Some(session) = &app.processing {
        session.prune_snapshots()?;
        app.store
            .compact_personal(now, &session.quota.root.join("snapshots"))?;
    }
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
        if clear_run_snapshot_and_calls(&mut conn, &candidate.run_id, candidate.delete_run)? {
            result.deleted_runs += 1;
        }
    }
    Ok(())
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
    let runs = all_run_records(&conn)?;
    let current = current_referenced_run_ids(&conn, &runs)?;
    let metadata = all_metadata(&conn)?;
    let protected = collect_protected_runs(app, &conn, &runs, &metadata)?;
    drop(conn);
    let mut result = RetentionResult {
        dry_run: !execute,
        ..Default::default()
    };
    let candidates =
        collect_candidates(app, runs, &current, &protected, &metadata, now, &mut result)?;
    if execute {
        execute_candidates(app, candidates, now, &mut result)?;
    }
    Ok(result)
}
