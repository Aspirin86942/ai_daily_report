use super::{
    error, file_hash, parse,
    store::{atomic_write, ArchiveStore},
};
use crate::{config::AppConfig, store::locking::ReportLock, ReportError};
use chrono::Utc;
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, path::Path, time::Duration};

pub fn verify(store: &ArchiveStore) -> Result<Value, ReportError> {
    let mut objects = BTreeSet::new();
    let mut failures = vec![];
    let integrity: String = store
        .conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        failures.push(json!({"code":"ARCHIVE_INDEX_CORRUPT"}));
    }
    let broken: i64 =
        store
            .conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
    if broken > 0 {
        failures.push(json!({"code":"ARCHIVE_REFERENCE_BROKEN","count":broken}));
    }
    for version in store.versions(None)? {
        if objects.insert(version.sha256.clone()) && store.verify_object(&version.sha256).is_err() {
            failures.push(json!({"object":version.sha256,"code":"ARCHIVE_OBJECT_CORRUPT"}));
        }
    }
    let mut statement = store.conn.prepare(
        "SELECT id,manifest_json,sha256 FROM extractions WHERE status IN('complete','partial')",
    )?;
    let mut count = 0;
    for row in statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, manifest, raw_sha256) = row?;
        count += 1;
        let artifact: ai_daily_worker_contract::archive::ArchiveArtifact =
            serde_json::from_str(&manifest)?;
        if parse::verify_artifact(&store.extraction_dir(&id)?, &artifact, &raw_sha256).is_err() {
            failures.push(json!({"extraction":id,"code":"ARCHIVE_EXTRACTION_CORRUPT"}));
        }
    }
    let mut statement = store
        .conn
        .prepare("SELECT run_id,path,sha256,markdown_sha256 FROM report_evidence")?;
    for row in statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })? {
        let (run, path, hash, markdown) = row?;
        if !file_hash(&store.root.join(path)).is_ok_and(|v| v == hash) {
            failures.push(json!({"run_id":run,"code":"ARCHIVE_REPORT_INPUT_CORRUPT"}));
        }
        if let Some(hash) = markdown {
            if !file_hash(
                &store
                    .root
                    .join("report_evidence")
                    .join(&run)
                    .join("report.md"),
            )
            .is_ok_and(|v| v == hash)
            {
                failures.push(json!({"run_id":run,"code":"ARCHIVE_REPORT_BODY_CORRUPT"}));
            }
        }
    }
    Ok(
        json!({"status":if failures.is_empty(){"verified"}else{"failed"},"objects":objects.len(),"extractions":count,"failures":failures}),
    )
}

pub fn backup(config: &AppConfig, output: &Path) -> Result<Value, ReportError> {
    let store = ArchiveStore::open(config, false)?;
    let _lock = ReportLock::acquire(
        &store.root.join("collector"),
        "archive",
        Duration::from_secs(5),
    )?;
    if !output.is_absolute() || output.exists() || output.starts_with(&store.root) {
        return Err(error("ARCHIVE_BACKUP_TARGET_INVALID"));
    }
    fs::create_dir_all(output)?;
    crate::snapshots::reject_redirects(output, "ARCHIVE_PATH_REDIRECT")?;
    let target = output.join("index.sqlite3");
    store
        .conn
        .execute("VACUUM INTO ?", [target.to_string_lossy().as_ref()])?;
    let snapshot = ArchiveStore::at(output, false)?;
    let mut files = BTreeSet::new();
    for v in snapshot.versions(None)? {
        files.insert(Path::new("objects").join(&v.sha256[..2]).join(v.sha256));
    }
    let mut statement = snapshot
        .conn
        .prepare("SELECT id FROM extractions WHERE status IN('complete','partial')")?;
    for id in statement.query_map([], |r| r.get::<_, String>(0))? {
        let id = id?;
        for name in ["body.md", "units.jsonl", "units.idx"] {
            files.insert(Path::new("extractions").join(&id).join(name));
        }
    }
    let mut statement = snapshot
        .conn
        .prepare("SELECT run_id,path,markdown_sha256 FROM report_evidence")?;
    for row in statement.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (run, path, markdown) = row?;
        files.insert(path.into());
        if markdown.is_some() {
            files.insert(Path::new("report_evidence").join(run).join("report.md"));
        }
    }
    let mut manifest = vec![];
    for relative in files {
        let source = store.root.join(&relative);
        let destination = output.join(&relative);
        crate::snapshots::reject_redirects(&source, "ARCHIVE_PATH_REDIRECT")?;
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::copy(&source, &destination)?;
        let hash = file_hash(&source)?;
        if file_hash(&destination)? != hash {
            return Err(error("ARCHIVE_BACKUP_VERIFY_FAILED"));
        }
        manifest.push(json!({"path":relative,"sha256":hash}));
    }
    manifest.push(json!({"path":"index.sqlite3","sha256":file_hash(&target)?}));
    let validation = verify(&snapshot)?;
    if validation["status"] != "verified" {
        return Err(error("ARCHIVE_BACKUP_VERIFY_FAILED"));
    }
    atomic_write(
        &output.join("backup.json"),
        &serde_json::to_vec_pretty(
            &json!({"version":1,"created_at":Utc::now(),"files":manifest,"verification":validation}),
        )?,
    )?;
    Ok(json!({"status":"verified","output":output,"files":manifest.len()}))
}
