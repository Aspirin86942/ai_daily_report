use super::{error, identity, local_day, parse, store::ArchiveStore};
use crate::{config::AppConfig, store::locking::ReportLock, types::new_run_id, ReportError};
use ai_daily_discovery::{discover_files_with_diagnostics, DiscoveredFileOut, DiscoveryRequest};
use ai_daily_scanner_core::source_guard::archive_identity;
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CollectionMetrics {
    pub run_id: String,
    pub files: u64,
    pub unchanged: u64,
    pub versions: u64,
    pub errors: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub discovery_ms: u64,
    pub capture_ms: u64,
    pub capture_work_ms: u64,
    pub parse_ms: u64,
    pub parse_work_ms: u64,
    pub queue_work_ms: u64,
    pub diff_ms: u64,
    pub wall_ms: u64,
    pub worker_peak_bytes: u64,
    pub parse_read_bytes: u64,
    pub audit_files: u64,
    pub parse_jobs: u64,
    pub parse_reused: u64,
    pub worker_starts: u64,
    pub model_calls: u64,
    pub memory_budget_mib: u64,
    pub queued: u64,
    #[serde(default)]
    pub capture_pending: u64,
    #[serde(default)]
    pub baseline_deferred: u64,
    #[serde(default)]
    pub reclaimed_bytes: u64,
    pub warnings: Vec<String>,
}

struct CaptureJob {
    file: DiscoveredFileOut,
    document: String,
    parent: Option<String>,
    known_hash: Option<String>,
    fs_id: Option<String>,
    guard: Option<String>,
    baseline: bool,
    audit: bool,
    audit_only: bool,
    priority: u8,
}
struct Capture {
    job: CaptureJob,
    result: Result<Captured, ReportError>,
    elapsed: u64,
}
struct Captured {
    hash: String,
    temp: Option<tempfile::NamedTempFile>,
    bytes: u64,
    modified: String,
    _reservation: Option<DiskReservation>,
    cache_reservation: Option<crate::processing_cache::CacheReservation>,
}

// Receipts include paths and fingerprints. Their atomic writer reserves both
// the temporary and final JSON bytes; enforce the bound used for admission.
const MAX_CAPTURE_RECEIPT_BYTES: u64 = 256 * 1024;

struct DiskReservation {
    bytes: u64,
    pending: Arc<Mutex<u64>>,
}
impl DiskReservation {
    fn acquire(
        pending: &Arc<Mutex<u64>>,
        path: &Path,
        bytes: u64,
        floor: u64,
    ) -> Result<Self, ReportError> {
        let mut reserved = pending.lock().unwrap();
        if free_disk(path)?
            .saturating_sub(*reserved)
            .saturating_sub(bytes)
            < floor
        {
            return Err(error("ARCHIVE_DISK_RESERVE"));
        }
        *reserved = reserved.saturating_add(bytes);
        Ok(Self {
            bytes,
            pending: pending.clone(),
        })
    }
}
impl Drop for DiskReservation {
    fn drop(&mut self) {
        let mut reserved = self.pending.lock().unwrap();
        *reserved = reserved.saturating_sub(self.bytes);
    }
}

#[derive(Serialize, Deserialize)]
struct CaptureReceipt {
    version: super::store::Version,
    run: String,
    day: String,
    fs_id: Option<String>,
    guard: Option<String>,
    temp: Option<PathBuf>,
}

pub(crate) fn recover_captures(store: &mut ArchiveStore) -> Result<(), ReportError> {
    for entry in fs::read_dir(store.root.join("staging"))? {
        if let Some(quota) = &store.quota {
            crate::processing_cache::check_deadline(quota.deadline())?;
        }
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("capture-")
            || entry.path().extension().is_none_or(|ext| ext != "json")
        {
            continue;
        }
        crate::snapshots::reject_redirects(&entry.path(), "ARCHIVE_PATH_REDIRECT")?;
        let receipt: CaptureReceipt = serde_json::from_slice(&fs::read(entry.path())?)?;
        let object = store.object(&receipt.version.sha256)?;
        if !object.exists() {
            let temp = receipt
                .temp
                .as_ref()
                .filter(|p| p.parent() == Some(store.root.join("staging").as_path()))
                .ok_or_else(|| error("ARCHIVE_CAPTURE_INCOMPLETE"))?;
            let hash = if let Some(quota) = &store.quota {
                super::file_hash_bounded(temp, quota.deadline())?
            } else {
                super::file_hash(temp)?
            };
            if hash != receipt.version.sha256 {
                return Err(error("ARCHIVE_CAPTURE_CORRUPT"));
            }
            fs::create_dir_all(object.parent().unwrap())?;
            if let Some(quota) = &store.quota {
                quota.relocate(temp, &object)?;
            } else {
                fs::rename(temp, &object)?;
            }
        }
        store.verify_object(&receipt.version.sha256)?;
        register_receipt(store, &receipt)?;
        if let Some(quota) = &store.quota {
            quota.remove(&entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn register_receipt(store: &mut ArchiveStore, receipt: &CaptureReceipt) -> Result<(), ReportError> {
    let v = &receipt.version;
    let tx = store.conn.transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO objects VALUES(?,?,?)",
        params![v.sha256, v.size_bytes as i64, v.observed_at],
    )?;
    tx.execute("INSERT OR IGNORE INTO versions(id,document_id,parent_id,sha256,path,extension,size_bytes,modified_at,observed_at,collection_id,baseline) VALUES(?,?,?,?,?,?,?,?,?,?,?)",params![v.id,v.document_id,v.parent_id,v.sha256,v.path,v.extension,v.size_bytes as i64,v.modified_at,v.observed_at,receipt.run,v.baseline])?;
    let updated = tx.execute("UPDATE documents SET fs_id=?,guard=?,current_version=?,audit_day=? WHERE id=? AND (current_version IS ? OR current_version=?)",params![receipt.fs_id,receipt.guard,v.id,receipt.day,v.document_id,v.parent_id,v.id])?;
    if store.quota.is_some() && updated > 0 {
        super::observation::captured(&tx, &v.document_id)?;
        tx.execute(
            "UPDATE gaps SET resolved_at=? WHERE path=? AND resolved_at IS NULL AND code='CAPTURE_CONTENT_UNAVAILABLE'",
            params![v.observed_at, v.path],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// CLI collection path, including the gate before resources or archive writes.
pub fn execute(
    config: &AppConfig,
    baseline: bool,
    scheduled: bool,
    now: DateTime<Utc>,
) -> Result<serde_json::Value, ReportError> {
    if scheduled
        && config
            .archive
            .collection_workday_window
            .as_ref()
            .is_some_and(|window| !window.allows(now))
    {
        return Ok(serde_json::json!({
            "status": "skipped",
            "action": "skipped",
            "reason": "outside_collection_workday_window",
            "model_calls": 0,
            "bytes_read": 0,
            "parse_jobs": 0,
            "worker_starts": 0,
        }));
    }
    let budget = super::resources::Budget::new(config.archive.memory_mib)?;
    let metrics = match run(config, baseline, now) {
        Err(e)
            if config.report.profile == crate::config::ReportProfile::Personal
                && e.code == "REPORT_ALREADY_RUNNING" =>
        {
            return Ok(
                serde_json::json!({"status":"skipped","action":"skipped","reason":"processing_busy","model_calls":0}),
            );
        }
        result => result?,
    };
    let mut value = result_value(&metrics, "collect")?;
    value["resources"] = budget.sample()?;
    if config.report.profile == crate::config::ReportProfile::Personal {
        return Ok(value);
    }
    let store = ArchiveStore::open(config, true)?;
    store.conn.execute(
        "UPDATE collections SET metrics_json=? WHERE id=?",
        params![serde_json::to_string(&value)?, metrics.run_id],
    )?;
    Ok(value)
}

pub(super) fn recover_parses(store: &mut ArchiveStore, run_id: &str) -> Result<(), ReportError> {
    // Work interrupted after a durable raw copy is requeued; originals are immutable.
    store.conn.execute(
        "UPDATE extractions SET status='queued' WHERE status='running' AND attempts<3",
        [],
    )?;
    store.conn.execute("UPDATE extractions SET status='failed',error_code='INTERRUPTED_RETRY_EXHAUSTED' WHERE status='running'",[])?;
    store.conn.execute("INSERT INTO gaps(collection_id,path,code,detail) SELECT ?, 'extraction:'||e.id,'INTERRUPTED_RETRY_EXHAUSTED','retry interrupted after maximum attempts' FROM extractions e JOIN metadata m ON m.value=e.id AND m.key='retry:'||e.id WHERE e.attempts>=3",[run_id])?;
    store.conn.execute("DELETE FROM metadata WHERE key LIKE 'retry:%' AND value IN(SELECT id FROM extractions WHERE attempts>=3)",[])?;
    Ok(())
}

pub(super) fn result_value(
    metrics: &CollectionMetrics,
    operation: &str,
) -> Result<serde_json::Value, ReportError> {
    let mut value = serde_json::to_value(metrics)?;
    let complete = metrics.errors == 0
        && metrics.queued == 0
        && metrics.capture_pending == 0
        && metrics.baseline_deferred == 0
        && !metrics.warnings.iter().any(|code| {
            matches!(
                code.as_str(),
                "PERSONAL_INPUT_UNAVAILABLE" | "COLLECTION_DEADLINE" | "RUN_BUDGET_DEFERRED"
            )
        });
    value["operation"] = serde_json::json!(operation);
    value["status"] = serde_json::json!(if complete { "succeeded" } else { "partial" });
    value["execution_status"] = serde_json::json!("succeeded");
    value["material_status"] = serde_json::json!(if complete { "complete" } else { "partial" });
    value["publication_status"] = serde_json::json!("not_applicable");
    let mut reasons = metrics.warnings.clone();
    if metrics.capture_pending > 0 {
        reasons.push("CAPTURE_CONTENT_UNAVAILABLE".into());
    }
    if metrics.baseline_deferred > 0 {
        reasons.push("BASELINE_CAPTURE_DEFERRED".into());
    }
    value["reason_codes"] = serde_json::json!(reasons);
    Ok(value)
}

pub fn run(
    config: &AppConfig,
    baseline: bool,
    now: DateTime<Utc>,
) -> Result<CollectionMetrics, ReportError> {
    let started = Instant::now();
    let deadline = started
        + Duration::from_secs(
            if config.report.profile == crate::config::ReportProfile::Personal {
                config.archive.run_seconds.min(120)
            } else {
                config.archive.run_seconds
            },
        );
    let personal = config.report.profile == crate::config::ReportProfile::Personal;
    let processing = if personal {
        Some(crate::processing_cache::ProcessingSession::at(
            &config.paths.data_dir.join("daily_cache"),
            now,
            deadline,
            crate::processing_cache::MAX_BYTES,
            crate::processing_cache::CONTROL_BYTES,
            Duration::ZERO,
        )?)
    } else {
        None
    };
    let _lock = if !personal {
        Some(ReportLock::acquire(
            &config.paths.data_dir.join("archive/collector"),
            "archive",
            Duration::ZERO,
        )?)
    } else {
        None
    };
    let mut store = if let Some(session) = &processing {
        session.open_archive(config)?
    } else {
        ArchiveStore::open(config, true)?
    };
    recover_captures(&mut store)?;
    store.conn.execute(
        "UPDATE collections SET status='interrupted',finished_at=? WHERE status='running'",
        [now.to_rfc3339()],
    )?;
    let run_id = new_run_id();
    let initialized: bool = store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='baseline')",
        [],
        |r| r.get(0),
    )?;
    if !initialized && !baseline {
        return Err(error("ARCHIVE_BASELINE_REQUIRED"));
    }
    if initialized && baseline {
        return Err(error("ARCHIVE_BASELINE_ALREADY_CREATED"));
    }
    let building_baseline = personal
        && (!initialized
            || store.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='baseline_building')",
                [],
                |r| r.get::<_, bool>(0),
            )?);
    let baseline = baseline || building_baseline;
    if building_baseline {
        store.conn.execute(
            "INSERT OR IGNORE INTO metadata VALUES('baseline_building',?)",
            [now.to_rfc3339()],
        )?;
    }
    let timestamp = now.to_rfc3339();
    super::schedule::activate_config(&store, &config.archive, now)?;
    store.conn.execute(
        "INSERT INTO collections(id,started_at,status,baseline) VALUES(?,?,'running',?)",
        params![run_id, timestamp, baseline],
    )?;
    recover_parses(&mut store, &run_id)?;
    let mut metrics = CollectionMetrics {
        run_id: run_id.clone(),
        ..Default::default()
    };
    let previous: Option<String> = store.conn.query_row(
        "SELECT max(started_at) FROM collections WHERE id<>? AND started_at<? AND status NOT LIKE 'parse_%'",
        params![run_id, timestamp],
        |r| r.get(0),
    )?;
    if let Some(previous) = previous.and_then(|v| DateTime::parse_from_rfc3339(&v).ok()) {
        let previous = previous.with_timezone(&Utc);
        let schedule = super::schedule::CollectionSchedule::load(&store)?;
        if let Some(due) = schedule.missed_after(previous, now, previous) {
            let gap = serde_json::json!({"from":due.to_rfc3339(),"to":timestamp,"interval_seconds":now.signed_duration_since(previous).num_seconds()});
            store.gap(&run_id, None, "COLLECTION_INTERVAL_GAP", &gap.to_string())?;
            store.conn.execute("UPDATE gaps SET resolved_at=? WHERE collection_id=? AND code='COLLECTION_INTERVAL_GAP'",params![timestamp,run_id])?;
            metrics.warnings.push("COLLECTION_INTERVAL_GAP".into());
        }
    }
    let profile = ai_daily_scanner_core::config::normalize_scanner_settings(
        &config.scanner,
        ai_daily_scanner_contract::ReportMode::Daily,
    )
    .map_err(|e| ReportError::new("ARCHIVE_CONFIG_INVALID", e.to_string()))?;
    let mut roots = vec![config.paths.work_dir.clone()];
    roots.extend(config.archive.roots.iter().cloned());
    roots.sort();
    roots.dedup();
    let mut files = BTreeMap::new();
    let mut complete_roots = BTreeSet::new();
    let mut cursor_updates = Vec::new();
    let mut remaining_entries = 10_000usize;
    let mut continued_roots = 0;
    for root in &roots {
        if building_baseline
            && store.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?)",
                [format!("baseline_root:{}", root.to_string_lossy())],
                |r| r.get::<_, bool>(0),
            )?
        {
            continue;
        }
        let mut excluded: Vec<PathBuf> = profile
            .discovery
            .excluded_dirs
            .iter()
            .map(PathBuf::from)
            .collect();
        excluded.extend([
            config.paths.data_dir.clone(),
            config.paths.reports_dir.clone(),
            config.paths.log_dir.clone(),
            store.root.clone(),
        ]);
        let request = DiscoveryRequest {
            work_dir: root.clone(),
            start_date: NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
            end_date: NaiveDate::from_ymd_opt(9999, 12, 31).unwrap(),
            window_start_utc: None,
            window_end_utc: None,
            follow_symlinks: false,
            allowed_extensions: profile.discovery.allowed_extensions.clone(),
            ignored_patterns: profile.discovery.ignored_patterns.clone(),
            excluded_dirs: excluded,
        };
        let found = if personal {
            let cursor_key = format!("discovery_cursor:{}", root.to_string_lossy());
            let cursor: Option<String> = store
                .conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?",
                    [&cursor_key],
                    |r| r.get(0),
                )
                .optional()?;
            let cursor = cursor
                .as_deref()
                .map(serde_json::from_str::<ai_daily_discovery::DiscoveryCursor>)
                .transpose()?;
            let result = ai_daily_discovery::discover_files_bounded_from(
                &request,
                deadline,
                remaining_entries.min((10_000 / roots.len()).max(1)),
                cursor,
            );
            match result {
                Ok((found, next, visited)) => {
                    remaining_entries = remaining_entries.saturating_sub(visited);
                    if let Some(next) = next {
                        continued_roots += 1;
                        cursor_updates.push((
                            cursor_key,
                            Some(serde_json::to_string(&next)?),
                            None,
                        ));
                    } else {
                        let baseline_key = (building_baseline
                            && found
                                .issues
                                .iter()
                                .all(|i| i.kind == ai_daily_discovery::DiscoveryIssueKind::Budget))
                        .then(|| format!("baseline_root:{}", root.to_string_lossy()));
                        cursor_updates.push((cursor_key, None, baseline_key));
                    }
                    Ok(found)
                }
                Err(e) => Err(e),
            }
        } else {
            discover_files_with_diagnostics(&request)
        };
        match found {
            Ok(found) => {
                if found.issues.is_empty() {
                    complete_roots.insert(root.to_string_lossy().into_owned());
                }
                for issue in found.issues {
                    store.gap(
                        &run_id,
                        issue.path.as_deref(),
                        "DISCOVERY_INCOMPLETE",
                        &format!("{:?}", issue.kind),
                    )?;
                    metrics.errors += 1;
                }
                for file in found.files {
                    files
                        .entry(file.path.clone())
                        .or_insert((root.to_string_lossy().into_owned(), file));
                }
            }
            Err(_) => {
                store.gap(
                    &run_id,
                    Some(&root.to_string_lossy()),
                    "ROOT_UNAVAILABLE",
                    "root inventory unavailable",
                )?;
                metrics.errors += 1;
            }
        }
    }
    metrics.discovery_ms = started.elapsed().as_millis() as u64;
    metrics.files = files.len() as u64;
    let present: BTreeSet<_> = files.keys().cloned().collect();
    let day = local_day(now);
    let mut jobs = VecDeque::new();
    let mut tx = store.conn.transaction()?;
    let mut files: Vec<_> = files.into_values().collect();
    if personal {
        files.sort_by(|a, b| {
            b.1.modified_at
                .cmp(&a.1.modified_at)
                .then(a.1.path.cmp(&b.1.path))
        });
    }
    for (index, (root, file)) in files.into_iter().enumerate() {
        if personal && index > 0 && index % 128 == 0 {
            tx.commit()?;
            tx = store.conn.transaction()?;
        }
        if personal && Instant::now() >= deadline {
            metrics.warnings.push("COLLECTION_DEADLINE".into());
            complete_roots.clear();
            break;
        }
        let metadata_identity = archive_identity(Path::new(&file.path)).ok().flatten();
        let (fs_id, guard) = metadata_identity
            .map(|(id, g)| (Some(id), Some(g)))
            .unwrap_or_default();
        type Existing = (String, Option<String>, Option<String>, bool, Option<String>);
        let mut existing: Option<Existing> = tx
            .query_row(
                "SELECT id,current_version,guard,baseline,audit_day FROM documents WHERE path=?",
                [&file.path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        if existing.is_none() {
            if let Some(id) = &fs_id {
                let matches = {
                    let mut stmt = tx.prepare("SELECT id,path,current_version,guard,baseline,audit_day,root FROM documents WHERE fs_id=?")?;
                    let values = stmt
                        .query_map([id], |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, Option<String>>(2)?,
                                r.get::<_, Option<String>>(3)?,
                                r.get::<_, bool>(4)?,
                                r.get::<_, Option<String>>(5)?,
                                r.get::<_, String>(6)?,
                            ))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    values
                };
                if matches.len() == 1
                    && complete_roots.contains(&matches[0].6)
                    && !present.contains(&matches[0].1)
                    && !Path::new(&matches[0].1).exists()
                {
                    let (doc, _, parent, old_guard, base, audit, _) =
                        matches.into_iter().next().unwrap();
                    tx.execute(
                        "UPDATE documents SET path=?,root=?,extension=? WHERE id=?",
                        params![file.path, root, file.extension, doc],
                    )?;
                    tx.execute(
                        "INSERT OR IGNORE INTO paths VALUES(?,?,?)",
                        params![doc, file.path, timestamp],
                    )?;
                    existing = Some((doc, parent, old_guard, base, audit));
                }
            }
        }
        let (doc, parent, old_guard, base, audit_day) = if let Some(row) = existing {
            row
        } else {
            let id = identity(&["document", &root, &file.path, &run_id]);
            tx.execute(
                "INSERT INTO documents(id,root,path,extension,fs_id,baseline) VALUES(?,?,?,?,?,?)",
                params![id, root, file.path, file.extension, fs_id, baseline],
            )?;
            tx.execute(
                "INSERT INTO paths VALUES(?,?,?)",
                params![id, file.path, timestamp],
            )?;
            (id, None, None, baseline, None)
        };
        tx.execute(
            "UPDATE documents SET seen_run=?,missing_count=0 WHERE id=?",
            params![run_id, doc],
        )?;
        tx.execute("UPDATE gaps SET resolved_at=? WHERE code='FILE_MISSING' AND path=? AND resolved_at IS NULL",params![timestamp,file.path])?;
        let audit = audit_day.as_deref() != Some(day.as_str());
        let pending_capture = personal
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?)",
                [format!("capture:{doc}")],
                |r| r.get::<_, bool>(0),
            )?;
        if parent.is_some() && guard.is_some() && old_guard == guard && !audit && !pending_capture {
            metrics.unchanged += 1;
            continue;
        }
        let known_hash = parent
            .as_ref()
            .map(|id| tx.query_row("SELECT sha256 FROM versions WHERE id=?", [id], |r| r.get(0)))
            .transpose()?;
        let audit_only = old_guard.is_some() && old_guard == guard;
        let changed = if personal {
            super::observation::observe(
                &tx,
                &doc,
                super::observation::fingerprint(
                    guard.as_deref(),
                    file.size_bytes,
                    &file.modified_at,
                ),
                parent.is_some() && !audit_only || parent.is_none() && !base,
                &timestamp,
            )?
        } else {
            false
        };
        let priority = if changed {
            0
        } else if parent.is_some() {
            1
        } else {
            2
        };
        if personal && changed {
            super::observation::record_gap(&tx, &doc, &run_id)?;
        }
        jobs.push_back(CaptureJob {
            file,
            document: doc,
            parent,
            known_hash,
            fs_id,
            guard,
            baseline: base && !changed,
            audit,
            audit_only,
            priority,
        });
    }
    for root in complete_roots {
        tx.execute("UPDATE documents SET missing_count=missing_count+1 WHERE root=? AND (seen_run IS NULL OR seen_run<>?)",params![root,run_id])?;
        tx.execute("INSERT INTO gaps(collection_id,path,code,detail) SELECT ?1,path,'FILE_MISSING','not observed in two complete inventories' FROM documents WHERE root=?2 AND missing_count=2 AND (seen_run IS NULL OR seen_run<>?1)",params![run_id,root])?;
        tx.execute("UPDATE gaps SET resolved_at=?1 WHERE resolved_at IS NULL AND code IN('ROOT_UNAVAILABLE','DISCOVERY_INCOMPLETE') AND (path=?2 OR substr(path,1,length(?2)+1)=?2||char(92) OR substr(path,1,length(?2)+1)=?2||'/')",params![timestamp,root])?;
    }
    if !initialized {
        tx.execute("INSERT INTO metadata VALUES('baseline',?)", [&timestamp])?;
    }
    // Advance only after every discovered path has durable metadata. A crash or
    // deadline during this page repeats it, preserving first-observation semantics.
    if !metrics.warnings.iter().any(|w| w == "COLLECTION_DEADLINE") {
        for (key, next, baseline_key) in cursor_updates {
            if let Some(next) = next {
                tx.execute("INSERT INTO metadata(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,next])?;
            } else {
                tx.execute("DELETE FROM metadata WHERE key=?", [&key])?;
            }
            if let Some(key) = baseline_key {
                tx.execute(
                    "INSERT OR IGNORE INTO metadata VALUES(?,?)",
                    params![key, timestamp],
                )?;
            }
        }
    }
    tx.commit()?;
    if building_baseline
        && continued_roots == 0
        && !metrics.warnings.iter().any(|w| w == "COLLECTION_DEADLINE")
    {
        let complete: bool = roots.iter().all(|root| {
            store
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM metadata WHERE key=?)",
                    [format!("baseline_root:{}", root.to_string_lossy())],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false)
        });
        if complete {
            store.conn.execute(
                "DELETE FROM metadata WHERE key='baseline_building' OR key LIKE 'baseline_root:%'",
                [],
            )?;
        }
    }
    let free = free_disk(&store.root)?;
    if free < config.archive.warn_free_mib * 1024 * 1024 {
        metrics.warnings.push("LOW_DISK_SPACE".into());
    }
    let capture_started = Instant::now();
    if personal {
        match capture_personal(config, &mut store, jobs, now, deadline, &mut metrics) {
            Err(e) if e.code == "PROCESSING_DEADLINE" => {
                metrics.errors += 1;
                metrics.warnings.push("COLLECTION_DEADLINE".into());
                store.gap(
                    &run_id,
                    None,
                    "COLLECTION_DEADLINE",
                    "采集时间预算已用完，保留未完成的采集记录。",
                )?;
            }
            result => result?,
        }
    } else {
        let job_queue = Mutex::new(jobs);
        let disk_pending = Arc::new(Mutex::new(0));
        let cache = store.quota.clone();
        // Capturing originals must leave room for at least one bounded parse.
        let capture_headroom = cache
            .as_ref()
            .map(|quota| quota.reserve(crate::processing_cache::MAX_OUTPUT_BYTES))
            .transpose()?;
        let staging = store.root.join("staging");
        let (sender, receiver) = mpsc::sync_channel(config.archive.read_workers * 2);
        std::thread::scope(|scope| -> Result<(), ReportError> {
            for _ in 0..config.archive.read_workers {
                let sender = sender.clone();
                let queue = &job_queue;
                let staging = &staging;
                let disk_pending = &disk_pending;
                let cache = &cache;
                scope.spawn(move || loop {
                    let Some(mut job) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let time = Instant::now();
                    let result = if time >= deadline {
                        Err(error("COLLECTION_DEADLINE"))
                    } else {
                        capture(
                            &job,
                            staging,
                            config,
                            deadline,
                            disk_pending,
                            cache.as_ref(),
                        )
                        .or_else(|e| {
                            if e.code == "SOURCE_CHANGED" || e.code == "SOURCE_BUSY" {
                                let fresh = fs::metadata(&job.file.path)?;
                                job.file.size_bytes = fresh.len();
                                let identity =
                                    archive_identity(Path::new(&job.file.path)).ok().flatten();
                                (job.fs_id, job.guard) = identity
                                    .map(|(id, guard)| (Some(id), Some(guard)))
                                    .unwrap_or_default();
                                job.audit_only = false;
                                capture(
                                    &job,
                                    staging,
                                    config,
                                    deadline,
                                    disk_pending,
                                    cache.as_ref(),
                                )
                            } else {
                                Err(e)
                            }
                        })
                    };
                    if sender
                        .send(Capture {
                            job,
                            result,
                            elapsed: time.elapsed().as_millis() as u64,
                        })
                        .is_err()
                    {
                        break;
                    }
                });
            }
            drop(sender);
            for capture in receiver {
                let at = now + chrono::Duration::from_std(started.elapsed()).unwrap_or_default();
                persist_capture(&mut store, &run_id, &day, at, capture, &mut metrics)?;
            }
            Ok(())
        })?;
        drop(capture_headroom);
    }
    metrics.capture_ms = capture_started.elapsed().as_millis() as u64;
    parse::drain(config, &mut store, &mut metrics, deadline)?;
    let pending = super::queue::pending_in(&store, config, None)?;
    metrics.queued = pending["queued"].as_u64().unwrap_or(0);
    if personal {
        (metrics.capture_pending, metrics.baseline_deferred) =
            super::observation::counts(&store, None)?;
    }
    if pending["unavailable"].as_u64().unwrap_or(0) > 0 {
        metrics.warnings.push("PERSONAL_INPUT_UNAVAILABLE".into());
    }
    metrics.wall_ms = started.elapsed().as_millis() as u64;
    let status = if result_value(&metrics, "collect")?["material_status"] == "complete" {
        "complete"
    } else {
        "partial"
    };
    store.conn.execute(
        "UPDATE collections SET finished_at=?,status=?,metrics_json=? WHERE id=?",
        params![
            Utc::now().to_rfc3339(),
            status,
            serde_json::to_string(&metrics)?,
            run_id
        ],
    )?;
    Ok(metrics)
}

fn capture_personal(
    config: &AppConfig,
    store: &mut ArchiveStore,
    jobs: VecDeque<CaptureJob>,
    now: DateTime<Utc>,
    deadline: Instant,
    metrics: &mut CollectionMetrics,
) -> Result<(), ReportError> {
    let started = Instant::now();
    let deadline = deadline - Duration::from_secs(2);
    let quota = store
        .quota
        .clone()
        .ok_or_else(|| error("PROCESSING_LOCK_REQUIRED"))?;
    let mut protected = match super::demand::protected(config, store, None, now, deadline) {
        Err(e) if e.code == "PROCESSING_DEADLINE" => {
            metrics.errors += 1;
            metrics.warnings.push("COLLECTION_DEADLINE".into());
            store.gap(
                &metrics.run_id,
                None,
                "COLLECTION_DEADLINE",
                "需求查询未完成，保留现有材料。",
            )?;
            return Ok(());
        }
        other => other?,
    };
    let mut jobs: Vec<_> = jobs.into_iter().collect();
    jobs.sort_by_key(|job| job.priority);
    for job in jobs.iter().filter(|job| job.priority == 0) {
        crate::processing_cache::check_deadline(deadline)?;
        protect_capture(store, job, now, &mut protected)?;
    }
    let staging = store.root.join("staging");
    let disk_pending = Arc::new(Mutex::new(0));
    for mut job in jobs {
        if Instant::now() >= deadline {
            metrics.errors += 1;
            metrics.warnings.push("COLLECTION_DEADLINE".into());
            store.gap(
                &metrics.run_id,
                None,
                "COLLECTION_DEADLINE",
                "采集时间预算已用完，未完成的正文仍待采集。",
            )?;
            break;
        }
        let at = Instant::now();
        let mut headroom = None;
        let mut audited_bytes = 0;
        let mut retried = false;
        let result = loop {
            if Instant::now() >= deadline {
                break Err(error("COLLECTION_DEADLINE"));
            }
            if !job.audit_only || job.known_hash.is_none() {
                if job.file.size_bytes > crate::processing_cache::MAX_INPUT_BYTES {
                    break Err(error("FILE_TOO_LARGE"));
                }
                let required = job.file.size_bytes
                    + crate::processing_cache::MAX_OUTPUT_BYTES
                    + 2 * MAX_CAPTURE_RECEIPT_BYTES;
                if job.priority == 0 {
                    match super::retention::reclaim(store, required, &protected, now, deadline) {
                        Ok(bytes) => metrics.reclaimed_bytes += bytes,
                        Err(e) => break Err(e),
                    }
                }
                if quota.available_bytes()? < required {
                    break Err(error("PROCESSING_CACHE_FULL"));
                }
                headroom = Some(quota.reserve(crate::processing_cache::MAX_OUTPUT_BYTES)?);
            }
            let result = capture(
                &job,
                &staging,
                config,
                deadline,
                &disk_pending,
                Some(&quota),
            );
            match result {
                Ok(captured)
                    if job.audit_only && job.known_hash.as_ref() != Some(&captured.hash) =>
                {
                    // Audits read without allocating a copy. Only a confirmed
                    // mismatch promotes the job and admits a bounded copy.
                    audited_bytes += captured.bytes;
                    job.audit_only = false;
                    job.priority = 0;
                    job.baseline = false;
                    super::observation::observe(
                        &store.conn,
                        &job.document,
                        super::observation::fingerprint(
                            job.guard.as_deref(),
                            job.file.size_bytes,
                            &job.file.modified_at,
                        ),
                        true,
                        &now.to_rfc3339(),
                    )?;
                    super::observation::record_gap(&store.conn, &job.document, &metrics.run_id)?;
                    protect_capture(store, &job, now, &mut protected)?;
                }
                Err(e) if !retried && matches!(e.code, "SOURCE_CHANGED" | "SOURCE_BUSY") => {
                    headroom = None;
                    retried = true;
                    let fresh = match fs::metadata(&job.file.path) {
                        Ok(value) => value,
                        Err(_) => break Err(e),
                    };
                    job.file.size_bytes = fresh.len();
                    job.file.modified_at = DateTime::<Utc>::from(fresh.modified()?).to_rfc3339();
                    let old_guard = job.guard.clone();
                    let identity = archive_identity(Path::new(&job.file.path)).ok().flatten();
                    (job.fs_id, job.guard) = identity
                        .map(|(id, guard)| (Some(id), Some(guard)))
                        .unwrap_or_default();
                    if old_guard != job.guard || e.code == "SOURCE_CHANGED" {
                        job.audit_only = false;
                        job.priority = 0;
                        job.baseline = false;
                        super::observation::observe(
                            &store.conn,
                            &job.document,
                            super::observation::fingerprint(
                                job.guard.as_deref(),
                                job.file.size_bytes,
                                &job.file.modified_at,
                            ),
                            true,
                            &now.to_rfc3339(),
                        )?;
                        super::observation::record_gap(
                            &store.conn,
                            &job.document,
                            &metrics.run_id,
                        )?;
                        protect_capture(store, &job, now, &mut protected)?;
                    }
                }
                other => break other,
            }
        };
        metrics.bytes_read += audited_bytes;
        if job.priority == 2
            && result
                .as_ref()
                .is_err_and(|e| e.code == "PROCESSING_CACHE_FULL")
        {
            // The durable inventory and baseline_deferred count already describe
            // this optional work. Repeated full-cache polls need no per-file rows.
            metrics.capture_work_ms += at.elapsed().as_millis() as u64;
            continue;
        }
        let document = job.document.clone();
        let required = job.priority == 0;
        let run = metrics.run_id.clone();
        persist_capture(
            store,
            &run,
            &local_day(now),
            now + chrono::Duration::from_std(started.elapsed()).unwrap_or_default(),
            Capture {
                job,
                result,
                elapsed: at.elapsed().as_millis() as u64,
            },
            metrics,
        )?;
        drop(headroom);
        if required {
            let current: Option<String> = store.conn.query_row(
                "SELECT current_version FROM documents WHERE id=?",
                [&document],
                |r| r.get(0),
            )?;
            protected.extend(current);
        }
    }
    Ok(())
}

fn protect_capture(
    store: &ArchiveStore,
    job: &CaptureJob,
    now: DateTime<Utc>,
    protected: &mut BTreeSet<String>,
) -> Result<(), ReportError> {
    protected.extend(job.parent.clone());
    let (start, _) =
        crate::types::utc_window(now.with_timezone(&crate::types::shanghai()).date_naive())?;
    let previous: Option<String> = store.conn.query_row(
        "SELECT id FROM versions WHERE document_id=? AND observed_at<? ORDER BY observed_at DESC,rowid DESC LIMIT 1",
        params![job.document, start.to_rfc3339()], |r| r.get(0),
    ).optional()?;
    protected.extend(previous);
    Ok(())
}

fn capture(
    job: &CaptureJob,
    staging: &Path,
    config: &AppConfig,
    run_deadline: Instant,
    disk_pending: &Arc<Mutex<u64>>,
    quota: Option<&Arc<crate::processing_cache::CacheQuota>>,
) -> Result<Captured, ReportError> {
    let path = Path::new(&job.file.path);
    let deadline =
        run_deadline.min(Instant::now() + Duration::from_secs(config.archive.copy_seconds));
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
    }
    let mut file = options.open(path).map_err(|_| error("SOURCE_BUSY"))?;
    let before = file.metadata()?;
    if quota.is_some() && before.len() > crate::processing_cache::MAX_INPUT_BYTES {
        return Err(error("FILE_TOO_LARGE"));
    }
    let stable = archive_identity(path).ok().flatten();
    if before.len() != job.file.size_bytes
        || (job.guard.is_some() && stable.as_ref().map(|(_, g)| g) != job.guard.as_ref())
    {
        return Err(error("SOURCE_CHANGED"));
    }
    let audit_only = job.audit_only && job.known_hash.is_some();
    let mut reservation = None;
    let mut cache_reservation = if audit_only {
        None
    } else {
        quota.map(|q| q.reserve(before.len())).transpose()?
    };
    let mut temp = if audit_only {
        None
    } else {
        reservation = Some(DiskReservation::acquire(
            disk_pending,
            staging,
            before.len(),
            config.archive.reserve_free_mib * 1024 * 1024,
        )?);
        Some(tempfile::NamedTempFile::new_in(staging)?)
    };
    if let (Some(reservation), Some(temp)) = (&mut cache_reservation, &temp) {
        reservation.track_existing_empty(temp.path())?;
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut bytes = 0;
    loop {
        if Instant::now() >= deadline {
            return Err(error("COPY_DEADLINE"));
        }
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        bytes += n as u64;
        if quota.is_some() && bytes > before.len() {
            return Err(error("SOURCE_CHANGED"));
        }
        if let Some(temp) = &mut temp {
            temp.write_all(&buffer[..n])?;
        }
    }
    let after = file.metadata()?;
    if before.len() != bytes
        || before.len() != after.len()
        || before.modified()? != after.modified()?
        || stable != archive_identity(path).ok().flatten()
    {
        return Err(error("SOURCE_CHANGED"));
    }
    let hash = ai_daily_worker_contract::archive::hex(&hash.finalize());
    if audit_only && Some(&hash) != job.known_hash.as_ref() && quota.is_none() {
        // An identity-preserving change discovered by the daily audit needs one copy.
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        reservation = Some(DiskReservation::acquire(
            disk_pending,
            staging,
            before.len(),
            config.archive.reserve_free_mib * 1024 * 1024,
        )?);
        let mut output = tempfile::NamedTempFile::new_in(staging)?;
        cache_reservation = quota.map(|q| q.reserve(before.len())).transpose()?;
        if let Some(reservation) = &mut cache_reservation {
            reservation.track_existing_empty(output.path())?;
        }
        let mut second = Sha256::new();
        let mut copied = 0u64;
        loop {
            if Instant::now() >= deadline {
                return Err(error("COPY_DEADLINE"));
            }
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            copied += n as u64;
            if quota.is_some() && copied > before.len() {
                return Err(error("SOURCE_CHANGED"));
            }
            output.write_all(&buffer[..n])?;
            second.update(&buffer[..n]);
            bytes += n as u64;
        }
        if ai_daily_worker_contract::archive::hex(&second.finalize()) != hash {
            return Err(error("SOURCE_CHANGED"));
        }
        temp = Some(output);
    }
    if let Some(temp) = &temp {
        temp.as_file().sync_all()?;
    }
    Ok(Captured {
        hash,
        temp,
        bytes,
        modified: DateTime::<Utc>::from(before.modified()?).to_rfc3339(),
        _reservation: reservation,
        cache_reservation,
    })
}

fn persist_capture(
    store: &mut ArchiveStore,
    run: &str,
    day: &str,
    at: DateTime<Utc>,
    capture: Capture,
    metrics: &mut CollectionMetrics,
) -> Result<(), ReportError> {
    let Capture {
        job,
        result,
        elapsed,
    } = capture;
    metrics.capture_work_ms += elapsed;
    let mut captured = match result {
        Ok(value) => value,
        Err(e) => {
            metrics.errors += 1;
            store.gap(run, Some(&job.file.path), e.code, &e.message)?;
            if store.quota.is_some() && job.priority == 0 {
                store.gap(
                    run,
                    Some(&job.file.path),
                    "CAPTURE_CONTENT_UNAVAILABLE",
                    "发现文件变化，但尚未取得该版本正文。",
                )?;
            }
            store.conn.execute(
                "INSERT INTO file_metrics(collection_id,path,capture_ms,status) VALUES(?,?,?,?)",
                params![run, job.file.path, elapsed as i64, e.code],
            )?;
            return Ok(());
        }
    };
    metrics.bytes_read += captured.bytes;
    if job.audit {
        metrics.audit_files += 1;
    }
    let unchanged = job.known_hash.as_ref() == Some(&captured.hash);
    let object = store.object(&captured.hash)?;
    let temp_path = captured
        .temp
        .map(|temp| {
            temp.keep()
                .map(|(file, path)| {
                    drop(file);
                    path
                })
                .map_err(|e| ReportError::from(e.error))
        })
        .transpose()?;
    let observed = at.to_rfc3339();
    let version_id = identity(&[
        "version",
        &job.document,
        job.parent.as_deref().unwrap_or(""),
        &captured.hash,
        run,
    ]);
    let receipt_path = store
        .root
        .join("staging")
        .join(format!("capture-{version_id}.json"));
    if !unchanged {
        // Commit the observation before moving its bytes. A crash on either side
        // of the rename can recover the same observed version, even if the source changes.
        let receipt = CaptureReceipt {
            version: super::store::Version {
                id: version_id.clone(),
                document_id: job.document.clone(),
                parent_id: job.parent.clone(),
                sha256: captured.hash.clone(),
                path: job.file.path.clone(),
                extension: job.file.extension.clone(),
                size_bytes: job.file.size_bytes,
                modified_at: captured.modified.clone(),
                observed_at: observed.clone(),
                baseline: job.baseline && job.parent.is_none(),
            },
            run: run.into(),
            day: day.into(),
            fs_id: job.fs_id.clone(),
            guard: job.guard.clone(),
            temp: temp_path.clone(),
        };
        let receipt = if store.quota.is_some()
            && serde_json::to_vec(&receipt)?.len() as u64 > MAX_CAPTURE_RECEIPT_BYTES
        {
            Err(error("PROCESSING_CACHE_FULL"))
        } else {
            store.put_json(&receipt_path, &receipt)
        };
        if let Err(e) = receipt {
            if store.quota.is_some() && e.code == "PROCESSING_CACHE_FULL" {
                // The original's reservation is still owned by `captured`.
                // Dropping it removes this uncommitted copy; the source remains
                // eligible for a later collection and no version is published.
                metrics.errors += 1;
                store.gap(run, Some(&job.file.path), e.code, &e.message)?;
                if job.priority == 0 {
                    store.gap(
                        run,
                        Some(&job.file.path),
                        "CAPTURE_CONTENT_UNAVAILABLE",
                        "发现文件变化，但尚未取得该版本正文。",
                    )?;
                }
                store.conn.execute(
                    "INSERT INTO file_metrics(collection_id,path,capture_ms,status) VALUES(?,?,?,?)",
                    params![run, job.file.path, elapsed as i64, e.code],
                )?;
                return Ok(());
            }
            return Err(e);
        }
    }
    if !(object.exists() || unchanged && store.quota.is_some()) {
        if let Some(reservation) = &mut captured.cache_reservation {
            reservation.track(&object)?;
        }
        fs::create_dir_all(object.parent().unwrap())?;
        let temp = temp_path
            .as_ref()
            .ok_or_else(|| error("ARCHIVE_OBJECT_MISSING"))?;
        fs::rename(temp, &object)?;
        metrics.bytes_written += job.file.size_bytes;
        if let Some(reservation) = captured.cache_reservation.take() {
            reservation.commit(std::slice::from_ref(&object))?;
        }
    }
    let tx = store.conn.transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO objects VALUES(?,?,?)",
        params![captured.hash, job.file.size_bytes as i64, observed],
    )?;
    let version = if unchanged {
        metrics.unchanged += 1;
        job.parent.unwrap()
    } else {
        let id = identity(&[
            "version",
            &job.document,
            job.parent.as_deref().unwrap_or(""),
            &captured.hash,
            run,
        ]);
        tx.execute("INSERT INTO versions(id,document_id,parent_id,sha256,path,extension,size_bytes,modified_at,observed_at,collection_id,baseline) VALUES(?,?,?,?,?,?,?,?,?,?,?)",params![id,job.document,job.parent,captured.hash,job.file.path,job.file.extension,job.file.size_bytes as i64,captured.modified,observed,run,job.baseline && job.parent.is_none()])?;
        metrics.versions += 1;
        id
    };
    tx.execute(
        "UPDATE documents SET fs_id=?,guard=?,current_version=?,audit_day=? WHERE id=?",
        params![job.fs_id, job.guard, version, day, job.document],
    )?;
    tx.execute(
        "UPDATE gaps SET resolved_at=? WHERE path=? AND resolved_at IS NULL",
        params![observed, job.file.path],
    )?;
    if store.quota.is_some() {
        super::observation::captured(&tx, &job.document)?;
    }
    if !unchanged {
        tx.execute("INSERT INTO file_metrics(collection_id,path,version_id,capture_ms,bytes_read,status) VALUES(?,?,?,?,?,?)",params![run,job.file.path,version,elapsed as i64,captured.bytes as i64,"captured"])?;
    }
    tx.commit()?;
    if !unchanged {
        if let Some(quota) = &store.quota {
            quota.remove(&receipt_path)?;
        } else {
            fs::remove_file(receipt_path)?;
        }
    }
    if let Some(temp) = temp_path.filter(|p| p.exists()) {
        fs::remove_file(temp)?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn free_disk(path: &Path) -> Result<u64, ReportError> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0;
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(free)
}
#[cfg(not(windows))]
pub fn free_disk(_path: &Path) -> Result<u64, ReportError> {
    Err(error("ARCHIVE_DISK_QUERY_UNSUPPORTED"))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::processing_cache::ProcessingSession;

    const MIB: usize = 1024 * 1024;

    fn capacity_setup() -> (tempfile::TempDir, AppConfig) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        fs::write(dir.path().join("config/settings.yaml"),
            r#"{"paths":{"work_dir":"work"},"report":{"profile":"personal"},"archive":{"daily_source":"archive"}}"#).unwrap();
        let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        (dir, config)
    }

    fn capacity_store(config: &AppConfig, now: DateTime<Utc>) -> (ProcessingSession, ArchiveStore) {
        let session = ProcessingSession::at(
            &config.paths.data_dir.join("daily_cache"),
            now,
            Instant::now() + Duration::from_secs(30),
            24 * MIB as u64,
            MIB as u64,
            Duration::ZERO,
        )
        .unwrap();
        let store = session.open_archive(config).unwrap();
        (session, store)
    }

    fn seed_body(
        store: &mut ArchiveStore,
        config: &AppConfig,
        name: &str,
        byte: u8,
        now: DateTime<Utc>,
    ) -> (String, String) {
        let source = config.paths.work_dir.join(name);
        let body = vec![byte; 4 * MIB];
        fs::write(&source, &body).unwrap();
        let hash = crate::snapshots::digest(&body);
        let object = store.object(&hash).unwrap();
        if !object.exists() {
            store.quota.as_ref().unwrap().write(&object, &body).unwrap();
        }
        let run = new_run_id();
        let document = identity(&[name]);
        let version = identity(&[name, &hash]);
        store
            .conn
            .execute(
                "INSERT INTO collections(id,started_at,status,baseline) VALUES(?,?,'complete',1)",
                params![run, now.to_rfc3339()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO documents(id,root,path,extension,baseline) VALUES(?,?,?,'.txt',1)",
                params![
                    document,
                    config.paths.work_dir.to_str().unwrap(),
                    source.to_str().unwrap()
                ],
            )
            .unwrap();
        register_receipt(
            store,
            &CaptureReceipt {
                version: super::super::store::Version {
                    id: version.clone(),
                    document_id: document.clone(),
                    parent_id: None,
                    sha256: hash,
                    path: source.to_str().unwrap().into(),
                    extension: ".txt".into(),
                    size_bytes: body.len() as u64,
                    modified_at: now.to_rfc3339(),
                    observed_at: now.to_rfc3339(),
                    baseline: true,
                },
                run,
                day: local_day(now),
                fs_id: None,
                guard: None,
                temp: None,
            },
        )
        .unwrap();
        (document, version)
    }

    fn capacity_job(
        store: &ArchiveStore,
        config: &AppConfig,
        name: &str,
        parent: Option<(String, String)>,
        now: DateTime<Utc>,
    ) -> CaptureJob {
        let source = config.paths.work_dir.join(name);
        let baseline = parent.is_none();
        let (document, version) = parent
            .map(|(d, v)| (d, Some(v)))
            .unwrap_or_else(|| (identity(&[name]), None));
        if baseline {
            store
                .conn
                .execute(
                    "INSERT OR IGNORE INTO documents(id,root,path,extension,baseline) VALUES(?,?,?,'.txt',1)",
                    params![
                        document,
                        config.paths.work_dir.to_str().unwrap(),
                        source.to_str().unwrap()
                    ],
                )
                .unwrap();
        }
        let known_hash = version.as_ref().map(|id| store.version(id).unwrap().sha256);
        let file = DiscoveredFileOut {
            file_identity: document.clone(),
            path: source.to_str().unwrap().into(),
            extension: ".txt".into(),
            modified_at: now.to_rfc3339(),
            size_bytes: fs::metadata(source).unwrap().len(),
            source_version: "test".into(),
            source_guard_kind: None,
            source_guard_sha256: None,
        };
        super::super::observation::observe(
            &store.conn,
            &document,
            super::super::observation::fingerprint(None, file.size_bytes, &file.modified_at),
            !baseline,
            &now.to_rfc3339(),
        )
        .unwrap();
        CaptureJob {
            file,
            document,
            parent: version,
            known_hash,
            fs_id: None,
            guard: None,
            baseline,
            audit: false,
            audit_only: false,
            priority: if baseline { 2 } else { 0 },
        }
    }

    fn capture_run(
        store: &mut ArchiveStore,
        config: &AppConfig,
        now: DateTime<Utc>,
        jobs: Vec<CaptureJob>,
    ) -> CollectionMetrics {
        let run_id = new_run_id();
        store
            .conn
            .execute(
                "INSERT INTO collections(id,started_at,status,baseline) VALUES(?,?,'running',0)",
                params![run_id, now.to_rfc3339()],
            )
            .unwrap();
        let mut metrics = CollectionMetrics {
            run_id,
            ..Default::default()
        };
        for job in &jobs {
            super::super::observation::record_gap(&store.conn, &job.document, &metrics.run_id)
                .unwrap();
        }
        capture_personal(
            config,
            store,
            jobs.into(),
            now,
            Instant::now() + Duration::from_secs(20),
            &mut metrics,
        )
        .unwrap();
        metrics
    }

    #[test]
    fn changed_body_precedes_baseline_and_shared_previous_bytes_survive_pressure() {
        let (_dir, config) = capacity_setup();
        let now = Utc::now();
        let old = now - chrono::Duration::days(1);
        let (session, mut store) = capacity_store(&config, old);
        let changed = seed_body(&mut store, &config, "changed.txt", b'a', old);
        seed_body(&mut store, &config, "alias.txt", b'a', old);
        seed_body(&mut store, &config, "standby-1.txt", b'b', old);
        seed_body(&mut store, &config, "standby-2.txt", b'c', old);
        let previous = store.version(&changed.1).unwrap();
        drop(store);
        drop(session);
        let (session, mut store) = capacity_store(&config, now);
        fs::write(config.paths.work_dir.join("changed.txt"), vec![b'd'; MIB]).unwrap();
        fs::write(
            config.paths.work_dir.join("late-baseline.txt"),
            vec![b'e'; 2 * MIB],
        )
        .unwrap();
        let baseline = capacity_job(&store, &config, "late-baseline.txt", None, now);
        let change = capacity_job(&store, &config, "changed.txt", Some(changed), now);
        let metrics = capture_run(&mut store, &config, now, vec![baseline, change]);
        assert_eq!((metrics.versions, metrics.errors), (1, 0));
        assert_eq!(metrics.reclaimed_bytes, 8 * MIB as u64);
        assert!(store.object(&previous.sha256).unwrap().is_file());
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM versions WHERE sha256=? AND pruned_at IS NULL",
                    [previous.sha256],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        assert_eq!(
            super::super::observation::counts(&store, None).unwrap(),
            (0, 1)
        );
        assert!(session.quota.used_bytes().unwrap() <= 24 * MIB as u64);
        let used = session.quota.used_bytes().unwrap();
        let retry = capacity_job(&store, &config, "late-baseline.txt", None, now);
        let retry = capture_run(&mut store, &config, now, vec![retry]);
        assert_eq!((retry.versions, retry.reclaimed_bytes), (0, 0));
        assert_eq!(session.quota.used_bytes().unwrap(), used);
        assert_eq!(
            store
                .conn
                .query_row("SELECT count(*) FROM file_metrics", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn unchanged_integrity_audit_reads_without_eviction_or_copy() {
        let (_dir, config) = capacity_setup();
        let now = Utc::now();
        let old = now - chrono::Duration::days(1);
        let (session, mut store) = capacity_store(&config, old);
        let source = seed_body(&mut store, &config, "audit.txt", b'a', old);
        seed_body(&mut store, &config, "standby.txt", b'b', old);
        drop(store);
        drop(session);
        let (session, mut store) = capacity_store(&config, now);
        let mut job = capacity_job(&store, &config, "audit.txt", Some(source), now);
        job.audit_only = true;
        job.audit = true;
        job.priority = 1;
        let used = session.quota.used_bytes().unwrap();
        let metrics = capture_run(&mut store, &config, now, vec![job]);
        assert_eq!(metrics.bytes_read, 4 * MIB as u64);
        assert_eq!(
            (
                metrics.versions,
                metrics.bytes_written,
                metrics.reclaimed_bytes
            ),
            (0, 0, 0)
        );
        assert_eq!(used, session.quota.used_bytes().unwrap());
        assert_eq!(
            super::super::observation::counts(&store, None).unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn deferred_capture_intent_survives_an_identical_observation() {
        let (_dir, config) = capacity_setup();
        let now = Utc::now();
        let (_session, store) = capacity_store(&config, now);
        fs::write(config.paths.work_dir.join("pending.txt"), b"first").unwrap();
        let job = capacity_job(&store, &config, "pending.txt", None, now);
        let observe = |fingerprint: &str, changed| {
            super::super::observation::observe(
                &store.conn,
                &job.document,
                fingerprint.to_owned(),
                changed,
                &now.to_rfc3339(),
            )
            .unwrap()
        };
        assert!(observe("second", false));
        assert!(observe("second", false));
        assert_eq!(
            super::super::observation::counts(&store, None).unwrap(),
            (1, 0)
        );
        super::super::observation::captured(&store.conn, &job.document).unwrap();
        assert_eq!(
            super::super::observation::counts(&store, None).unwrap(),
            (0, 1)
        );
    }

    #[test]
    fn protection_reads_beyond_selection_limit_and_keeps_pending_preimages() {
        let (_dir, config) = capacity_setup();
        let now = Utc::now();
        let old = now - chrono::Duration::days(2);
        let (session, mut store) = capacity_store(&config, old);
        let (_, seed) = seed_body(&mut store, &config, "seed.txt", b'a', old);
        let pending = seed_body(&mut store, &config, "pending.txt", b'b', old);
        let (_, last) = seed_body(&mut store, &config, "last.txt", b'c', old);
        store
            .conn
            .execute(
                "UPDATE versions SET baseline=0,observed_at=? WHERE id=?",
                params![now.to_rfc3339(), last],
            )
            .unwrap();
        let tx = store.conn.transaction().unwrap();
        for index in 0..1_030 {
            let id = format!("document-{index:04}");
            let path = config.paths.work_dir.join(format!("copy-{index}.txt"));
            tx.execute("INSERT INTO documents(id,root,path,extension,baseline,current_version) VALUES(?,?,?,'.txt',0,?)",
                params![id,config.paths.work_dir.to_str().unwrap(),path.to_str().unwrap(),id]).unwrap();
            tx.execute("INSERT INTO versions(id,document_id,sha256,path,extension,size_bytes,modified_at,observed_at,collection_id,baseline)
                SELECT ?1,?1,sha256,?2,extension,size_bytes,modified_at,?3,collection_id,0 FROM versions WHERE id=?4",
                params![id,path.to_str().unwrap(),now.to_rfc3339(),seed]).unwrap();
        }
        tx.commit().unwrap();
        super::super::observation::observe(
            &store.conn,
            &pending.0,
            "changed".into(),
            true,
            &now.to_rfc3339(),
        )
        .unwrap();
        let protected = super::super::demand::protected(
            &config,
            &store,
            None,
            now,
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(protected.len(), 1_032);
        assert!(protected.contains(&last));
        assert!(protected.contains(&pending.1));
        let used = session.quota.used_bytes().unwrap();
        let failed = super::super::demand::protected(&config, &store, None, now, Instant::now());
        assert_eq!(failed.unwrap_err().code, "PROCESSING_DEADLINE");
        assert_eq!(session.quota.used_bytes().unwrap(), used);
    }

    #[test]
    fn expired_capture_receipt_recovers_observation_before_pruning_bytes() {
        let (_dir, config) = capacity_setup();
        let old = Utc::now() - chrono::Duration::days(8);
        let (session, store) = capacity_store(&config, old);
        fs::write(config.paths.work_dir.join("recover.txt"), b"observed").unwrap();
        let job = capacity_job(&store, &config, "recover.txt", None, old);
        let run = new_run_id();
        store
            .conn
            .execute(
                "INSERT INTO collections(id,started_at,status,baseline) VALUES(?,?,'running',1)",
                params![run, old.to_rfc3339()],
            )
            .unwrap();
        let hash = crate::snapshots::digest(b"observed");
        let temp = store.root.join("staging/recover.tmp");
        session.quota.write(&temp, b"observed").unwrap();
        let receipt = CaptureReceipt {
            version: super::super::store::Version {
                id: identity(&["recovered-version"]),
                document_id: job.document.clone(),
                parent_id: None,
                sha256: hash.clone(),
                path: job.file.path,
                extension: ".txt".into(),
                size_bytes: 8,
                modified_at: old.to_rfc3339(),
                observed_at: old.to_rfc3339(),
                baseline: true,
            },
            run,
            day: local_day(old),
            fs_id: None,
            guard: None,
            temp: Some(temp),
        };
        store
            .put_json(&store.root.join("staging/capture-recover.json"), &receipt)
            .unwrap();
        drop(store);
        drop(session);
        let (_session, store) = capacity_store(&config, Utc::now());
        let current: String = store
            .conn
            .query_row(
                "SELECT current_version FROM documents WHERE id=?",
                [job.document],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(current, receipt.version.id);
        assert!(!store.object(&hash).unwrap().exists());
        assert!(store
            .conn
            .query_row(
                "SELECT pruned_at IS NOT NULL FROM versions WHERE id=?",
                [current],
                |r| r.get::<_, bool>(0)
            )
            .unwrap());
    }

    #[test]
    fn exhausted_capture_budget_keeps_durable_gap_and_comparison_body() {
        let (_dir, config) = capacity_setup();
        let now = Utc::now();
        let (_session, mut store) = capacity_store(&config, now);
        let previous = seed_body(
            &mut store,
            &config,
            "deadline.txt",
            b'a',
            now - chrono::Duration::days(1),
        );
        let before = store.version(&previous.1).unwrap();
        fs::write(config.paths.work_dir.join("deadline.txt"), b"changed").unwrap();
        let job = capacity_job(&store, &config, "deadline.txt", Some(previous), now);
        let run_id = new_run_id();
        store
            .conn
            .execute(
                "INSERT INTO collections(id,started_at,status,baseline) VALUES(?,?,'running',0)",
                params![run_id, now.to_rfc3339()],
            )
            .unwrap();
        super::super::observation::record_gap(&store.conn, &job.document, &run_id).unwrap();
        let mut metrics = CollectionMetrics {
            run_id,
            ..Default::default()
        };
        capture_personal(
            &config,
            &mut store,
            vec![job].into(),
            now,
            Instant::now(),
            &mut metrics,
        )
        .unwrap();
        assert_eq!((metrics.versions, metrics.bytes_read), (0, 0));
        assert!(store.object(&before.sha256).unwrap().is_file());
        assert_eq!(
            super::super::observation::counts(&store, None).unwrap().0,
            1
        );
        assert_eq!(store.conn.query_row("SELECT count(*) FROM gaps WHERE code='CAPTURE_CONTENT_UNAVAILABLE' AND resolved_at IS NULL",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    }

    #[test]
    fn shared_extraction_is_protected_across_versions() {
        let (_dir, config) = capacity_setup();
        let old = Utc::now() - chrono::Duration::days(1);
        let (session, mut store) = capacity_store(&config, old);
        let (_, original) = seed_body(&mut store, &config, "original.txt", b'a', old);
        let (_, alias) = seed_body(&mut store, &config, "alias.txt", b'a', old);
        let hash = store.version(&original).unwrap().sha256;
        let extraction = identity(&["extraction", &hash]);
        store.conn.execute("INSERT INTO extractions(id,sha256,profile,extension,status) VALUES(?,?,'test','.txt','complete')",params![extraction,hash]).unwrap();
        for id in [&original, &alias] {
            store
                .conn
                .execute(
                    "INSERT INTO version_extractions VALUES(?,?)",
                    params![id, extraction],
                )
                .unwrap();
        }
        let directory = store.extraction_dir(&extraction).unwrap();
        let path = directory.join("content.txt");
        let mut reservation = session.quota.reserve(100).unwrap();
        reservation.track(&directory).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::write(&path, b"shared parsed body").unwrap();
        reservation.commit(&[directory]).unwrap();
        drop(store);
        drop(session);
        let (_session, mut store) = capacity_store(&config, Utc::now());
        let protected = BTreeSet::from([alias]);
        assert_eq!(
            super::super::retention::reclaim(
                &mut store,
                23 * MIB as u64,
                &protected,
                Utc::now(),
                Instant::now() + Duration::from_secs(10)
            )
            .unwrap(),
            0
        );
        assert!(path.is_file());
        assert!(store.version(&original).is_ok());
        assert!(
            super::super::retention::reclaim(
                &mut store,
                23 * MIB as u64,
                &BTreeSet::new(),
                Utc::now(),
                Instant::now() + Duration::from_secs(10)
            )
            .unwrap()
                > 0
        );
        assert!(!path.exists());
    }

    #[test]
    fn failed_deletion_keeps_bytes_accounted_and_marks_content_unavailable() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_dir, config) = capacity_setup();
        let old = Utc::now() - chrono::Duration::days(1);
        let (session, mut store) = capacity_store(&config, old);
        let (_, id) = seed_body(&mut store, &config, "locked.txt", b'a', old);
        let object = store.object(&store.version(&id).unwrap().sha256).unwrap();
        drop(store);
        drop(session);
        let (session, mut store) = capacity_store(&config, Utc::now());
        let held = OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(&object)
            .unwrap();
        let used = session.quota.used_bytes().unwrap();
        assert!(super::super::retention::reclaim(
            &mut store,
            22 * MIB as u64,
            &BTreeSet::new(),
            Utc::now(),
            Instant::now() + Duration::from_secs(10)
        )
        .is_err());
        assert!(object.exists());
        assert_eq!(session.quota.used_bytes().unwrap(), used);
        assert!(store.version(&id).is_err());
        drop(held);
    }

    #[test]
    fn material_status_distinguishes_missing_input_from_storage_warnings() {
        let mut metrics = CollectionMetrics {
            warnings: vec!["ARCHIVE_COMPRESSION_UNAVAILABLE".into()],
            ..Default::default()
        };
        assert_eq!(
            result_value(&metrics, "collect").unwrap()["material_status"],
            "complete"
        );
        metrics.warnings.push("PERSONAL_INPUT_UNAVAILABLE".into());
        assert_eq!(
            result_value(&metrics, "collect").unwrap()["material_status"],
            "partial"
        );
    }

    #[test]
    fn full_receipt_capacity_defers_only_that_file_and_can_resume() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        fs::write(
            dir.path().join("config/settings.yaml"),
            r#"{"paths":{"work_dir":"work"},"report":{"profile":"personal"},"archive":{"daily_source":"archive"}}"#,
        )
        .unwrap();
        let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        let now = Utc::now();
        let deadline = Instant::now() + Duration::from_secs(30);
        let session = ProcessingSession::at(
            &config.paths.data_dir.join("daily_cache"),
            now,
            deadline,
            2 * 1024 * 1024,
            1024 * 1024,
            Duration::ZERO,
        )
        .unwrap();
        let mut store = session.open_archive(&config).unwrap();
        let source = config.paths.work_dir.join("amount.txt");
        let body = vec![b'a'; 1024];
        fs::write(&source, &body).unwrap();
        let run = new_run_id();
        let document = identity(&["document", source.to_str().unwrap()]);
        store
            .conn
            .execute(
                "INSERT INTO collections(id,started_at,status,baseline) VALUES(?,?,'running',1)",
                params![run, now.to_rfc3339()],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO documents(id,root,path,extension,baseline) VALUES(?,?,?,'.txt',1)",
                params![
                    document,
                    config.paths.work_dir.to_str().unwrap(),
                    source.to_str().unwrap()
                ],
            )
            .unwrap();
        let job = || CaptureJob {
            file: DiscoveredFileOut {
                file_identity: document.clone(),
                path: source.to_str().unwrap().into(),
                extension: ".txt".into(),
                modified_at: now.to_rfc3339(),
                size_bytes: body.len() as u64,
                source_version: "test".into(),
                source_guard_kind: None,
                source_guard_sha256: None,
            },
            document: document.clone(),
            parent: None,
            known_hash: None,
            fs_id: None,
            guard: None,
            baseline: true,
            audit: false,
            audit_only: false,
            priority: 0,
        };
        let quota = &session.quota;
        let occupied = quota
            .reserve(quota.available_bytes().unwrap() - body.len() as u64)
            .unwrap();
        let staging = store.root.join("staging");
        let pending = Arc::new(Mutex::new(0));
        let mut metrics = CollectionMetrics::default();
        {
            let job = job();
            let result = capture(&job, &staging, &config, deadline, &pending, Some(quota));
            assert!(result.is_ok());
            persist_capture(
                &mut store,
                &run,
                &local_day(now),
                now,
                Capture {
                    job,
                    result,
                    elapsed: 1,
                },
                &mut metrics,
            )
            .unwrap();
            assert_eq!(metrics.versions, 0);
            assert_eq!(fs::read(&source).unwrap(), body);
            assert!(fs::read_dir(&staging).unwrap().next().is_none());
            assert_eq!(metrics.errors, 1);
            assert_eq!(quota.used_bytes().unwrap(), 1024 * 1024);
        }
        drop(occupied);
        let run = new_run_id();
        store
            .conn
            .execute(
                "INSERT INTO collections(id,started_at,status,baseline) VALUES(?,?,'running',0)",
                params![run, now.to_rfc3339()],
            )
            .unwrap();
        let job = job();
        let result = capture(&job, &staging, &config, deadline, &pending, Some(quota));
        persist_capture(
            &mut store,
            &run,
            &local_day(now),
            now,
            Capture {
                job,
                result,
                elapsed: 1,
            },
            &mut metrics,
        )
        .unwrap();
        assert_eq!((metrics.errors, metrics.versions), (1, 1));
        assert_eq!(store.versions(None).unwrap().len(), 1);
        assert_eq!(fs::read(&source).unwrap(), body);
        assert!(fs::read_dir(staging).unwrap().next().is_none());
    }
}
