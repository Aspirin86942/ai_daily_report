use super::{
    error, file_hash, identity, parse,
    store::{atomic_write, ArchiveStore},
};
use crate::{config::AppConfig, store::locking::ReportLock, ReportError};
use chrono::{NaiveDate, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrunePlan {
    version: u32,
    id: String,
    before: String,
    archive_root: String,
    versions: Vec<String>,
    objects: Vec<String>,
    extractions: Vec<String>,
    protected: Vec<Value>,
    bytes: u64,
}

fn plan(store: &ArchiveStore, before: NaiveDate) -> Result<PrunePlan, ReportError> {
    let mut statement=store.conn.prepare("SELECT v.id FROM versions v WHERE v.observed_at<? AND v.pruned_at IS NULL AND NOT EXISTS(SELECT 1 FROM documents d WHERE d.current_version=v.id) AND NOT EXISTS(SELECT 1 FROM report_versions rv WHERE rv.version_id=v.id) AND NOT EXISTS(SELECT 1 FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=v.id AND (e.status IN('queued','running') OR e.id IN(SELECT value FROM metadata WHERE key LIKE 'retry:%'))) AND NOT EXISTS(SELECT 1 FROM changes c WHERE (c.version_id=v.id OR c.before_version=v.id) AND NOT EXISTS(SELECT 1 FROM report_changes rc JOIN report_evidence r ON r.run_id=rc.run_id WHERE rc.change_id=c.id AND r.status='published')) AND NOT EXISTS(SELECT 1 FROM versions child JOIN documents d ON d.current_version=child.id WHERE child.parent_id=v.id) ORDER BY v.id")?;
    let cutoff = format!(
        "{}T16:00:00+00:00",
        before
            .pred_opt()
            .ok_or_else(|| error("ARCHIVE_DATE_INVALID"))?
    );
    let versions = statement
        .query_map([cutoff], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let candidates: BTreeSet<_> = versions.iter().cloned().collect();
    let mut objects = BTreeSet::new();
    let mut bytes = 0;
    for id in &versions {
        let version = store.version(id)?;
        let mut statement = store
            .conn
            .prepare("SELECT id FROM versions WHERE sha256=? AND pruned_at IS NULL")?;
        let references = statement
            .query_map([&version.sha256], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if references.iter().all(|id| candidates.contains(id))
            && objects.insert(version.sha256.clone())
        {
            bytes += version.size_bytes;
        }
    }
    let mut protected = vec![];
    for version in store.versions(None)? {
        if !candidates.contains(&version.id) {
            let references: i64 = store.conn.query_row(
                "SELECT count(*) FROM report_versions WHERE version_id=?",
                [&version.id],
                |r| r.get(0),
            )?;
            protected.push(json!({"version_id":version.id,"report_references":references,"reason":"current_predecessor_or_unreported_or_queued_or_pinned"}));
        }
    }
    let mut stmt = store
        .conn
        .prepare("SELECT id FROM extractions WHERE status<>'pruned'")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut extractions = vec![];
    for id in ids {
        let mut refs = store
            .conn
            .prepare("SELECT version_id FROM version_extractions WHERE extraction_id=?")?;
        let versions = refs
            .query_map([&id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !versions.is_empty() && versions.iter().all(|v| candidates.contains(v)) {
            for name in ["body.md", "units.jsonl", "units.idx"] {
                bytes += fs::metadata(store.extraction_dir(&id)?.join(name)).map_or(0, |m| m.len());
            }
            extractions.push(id);
        }
    }
    let objects: Vec<_> = objects.into_iter().collect();
    let root = fs::canonicalize(&store.root)?
        .to_string_lossy()
        .into_owned();
    let id = identity(&[
        "prune-v1",
        &root,
        &before.to_string(),
        &serde_json::to_string(&versions)?,
        &serde_json::to_string(&objects)?,
    ]);
    Ok(PrunePlan {
        version: 1,
        id,
        before: before.to_string(),
        archive_root: root,
        versions,
        objects,
        extractions,
        protected,
        bytes,
    })
}
pub fn prune_plan(config: &AppConfig, before: NaiveDate) -> Result<Value, ReportError> {
    let store = ArchiveStore::open(config, false)?;
    let plan = plan(&store, before)?;
    // The command prints the concrete plan; the caller chooses where to save it.
    Ok(serde_json::to_value(plan)?)
}
pub fn prune_execute(config: &AppConfig, path: &Path) -> Result<Value, ReportError> {
    let saved: PrunePlan = serde_json::from_slice(&fs::read(path)?)?;
    let mut store = ArchiveStore::open(config, true)?;
    let _lock = ReportLock::acquire(
        &store.root.join("collector"),
        "archive",
        Duration::from_secs(5),
    )?;
    let completed: Option<String> = store
        .conn
        .query_row(
            "SELECT plan_json FROM prune_audit WHERE plan_id=?",
            [&saved.id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(prior) = &completed {
        if serde_json::from_str::<Value>(prior)? != serde_json::to_value(&saved)? {
            return Err(error("ARCHIVE_PRUNE_PLAN_INVALID"));
        }
    } else {
        let current = plan(&store, crate::types::parse_date(&saved.before)?)?;
        if serde_json::to_value(&saved)? != serde_json::to_value(&current)? {
            return Err(error("ARCHIVE_PRUNE_PLAN_STALE"));
        }
    }
    let at = Utc::now().to_rfc3339();
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for id in &saved.versions {
        let protected: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM report_versions WHERE version_id=?)",
            [id],
            |r| r.get(0),
        )?;
        if protected {
            return Err(error("ARCHIVE_VERSION_PINNED"));
        }
        tx.execute(
            "UPDATE versions SET pruned_at=? WHERE id=?",
            params![at, id],
        )?;
    }
    // Commit tombstones before unlinking: a crash leaves recoverable extra bytes.
    for id in &saved.extractions {
        tx.execute("UPDATE extractions SET status='pruned' WHERE id=?", [id])?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO prune_audit VALUES(?,?,?)",
        params![saved.id, at, serde_json::to_string(&saved)?],
    )?;
    tx.commit()?;
    let mut released = 0;
    for hash in &saved.objects {
        let object = store.object(hash)?;
        crate::snapshots::reject_redirects(&object, "ARCHIVE_PATH_REDIRECT")?;
        if object.exists() {
            released += fs::metadata(&object)?.len();
            fs::remove_file(object)?;
        }
    }
    for id in &saved.extractions {
        let directory = store.extraction_dir(id)?;
        crate::snapshots::reject_redirects(&directory, "ARCHIVE_PATH_REDIRECT")?;
        for name in ["body.md", "units.jsonl", "units.idx"] {
            let path = directory.join(name);
            if path.exists() {
                released += fs::metadata(&path)?.len();
                fs::remove_file(path)?;
            }
        }
        if directory.exists() {
            fs::remove_dir(directory)?;
        }
    }
    Ok(
        json!({"plan_id":saved.id,"versions_pruned":saved.versions.len(),"released_bytes":released,"metadata_retained":true}),
    )
}
