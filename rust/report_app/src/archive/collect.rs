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
}

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

fn recover_captures(store: &mut ArchiveStore) -> Result<(), ReportError> {
    for entry in fs::read_dir(store.root.join("staging"))? {
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
            if super::file_hash(temp)? != receipt.version.sha256 {
                return Err(error("ARCHIVE_CAPTURE_CORRUPT"));
            }
            fs::create_dir_all(object.parent().unwrap())?;
            fs::rename(temp, &object)?;
        }
        store.verify_object(&receipt.version.sha256)?;
        register_receipt(store, &receipt)?;
        fs::remove_file(entry.path())?;
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
    tx.execute("UPDATE documents SET fs_id=?,guard=?,current_version=?,audit_day=? WHERE id=? AND (current_version IS ? OR current_version=?)",params![receipt.fs_id,receipt.guard,v.id,receipt.day,v.document_id,v.parent_id,v.id])?;
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
    let metrics = run(config, baseline, now)?;
    let mut value = serde_json::to_value(&metrics)?;
    value["resources"] = budget.sample()?;
    let store = ArchiveStore::open(config, true)?;
    store.conn.execute(
        "UPDATE collections SET metrics_json=? WHERE id=?",
        params![serde_json::to_string(&value)?, metrics.run_id],
    )?;
    Ok(value)
}

pub fn run(
    config: &AppConfig,
    baseline: bool,
    now: DateTime<Utc>,
) -> Result<CollectionMetrics, ReportError> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(config.archive.run_seconds);
    let _lock = ReportLock::acquire(
        &config.paths.data_dir.join("archive/collector"),
        "archive",
        Duration::ZERO,
    )?;
    let mut store = ArchiveStore::open(config, true)?;
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
    let timestamp = now.to_rfc3339();
    super::schedule::activate_config(&store, &config.archive, now)?;
    store.conn.execute(
        "INSERT INTO collections(id,started_at,status,baseline) VALUES(?,?,'running',?)",
        params![run_id, timestamp, baseline],
    )?;
    // Work interrupted after a durable raw copy is requeued; originals are immutable.
    store.conn.execute(
        "UPDATE extractions SET status='queued' WHERE status='running' AND attempts<3",
        [],
    )?;
    store.conn.execute("UPDATE extractions SET status='failed',error_code='INTERRUPTED_RETRY_EXHAUSTED' WHERE status='running'",[])?;
    store.conn.execute("INSERT INTO gaps(collection_id,path,code,detail) SELECT ?, 'extraction:'||e.id,'INTERRUPTED_RETRY_EXHAUSTED','retry interrupted after maximum attempts' FROM extractions e JOIN metadata m ON m.value=e.id AND m.key='retry:'||e.id WHERE e.attempts>=3",[&run_id])?;
    store.conn.execute("DELETE FROM metadata WHERE key LIKE 'retry:%' AND value IN(SELECT id FROM extractions WHERE attempts>=3)",[])?;
    let mut metrics = CollectionMetrics {
        run_id: run_id.clone(),
        ..Default::default()
    };
    let previous: Option<String> = store.conn.query_row(
        "SELECT max(started_at) FROM collections WHERE id<>? AND started_at<?",
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
    for root in &roots {
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
        match discover_files_with_diagnostics(&request) {
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
    let tx = store.conn.transaction()?;
    for (root, file) in files.into_values() {
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
        if parent.is_some() && guard.is_some() && old_guard == guard && !audit {
            metrics.unchanged += 1;
            continue;
        }
        let known_hash = parent
            .as_ref()
            .map(|id| tx.query_row("SELECT sha256 FROM versions WHERE id=?", [id], |r| r.get(0)))
            .transpose()?;
        let audit_only = old_guard.is_some() && old_guard == guard;
        jobs.push_back(CaptureJob {
            file,
            document: doc,
            parent,
            known_hash,
            fs_id,
            guard,
            baseline: base,
            audit,
            audit_only,
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
    tx.commit()?;
    let free = free_disk(&store.root)?;
    if free < config.archive.warn_free_mib * 1024 * 1024 {
        metrics.warnings.push("LOW_DISK_SPACE".into());
    }
    let job_queue = Mutex::new(jobs);
    let disk_pending = Arc::new(Mutex::new(0));
    let capture_started = Instant::now();
    let staging = store.root.join("staging");
    let (sender, receiver) = mpsc::sync_channel(config.archive.read_workers * 2);
    std::thread::scope(|scope| -> Result<(), ReportError> {
        for _ in 0..config.archive.read_workers {
            let sender = sender.clone();
            let queue = &job_queue;
            let staging = &staging;
            let disk_pending = &disk_pending;
            scope.spawn(move || loop {
                let Some(mut job) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                let time = Instant::now();
                let result = if time >= deadline {
                    Err(error("COLLECTION_DEADLINE"))
                } else {
                    capture(&job, staging, config, deadline, disk_pending).or_else(|e| {
                        if e.code == "SOURCE_CHANGED" || e.code == "SOURCE_BUSY" {
                            let fresh = fs::metadata(&job.file.path)?;
                            job.file.size_bytes = fresh.len();
                            let identity =
                                archive_identity(Path::new(&job.file.path)).ok().flatten();
                            (job.fs_id, job.guard) = identity
                                .map(|(id, guard)| (Some(id), Some(guard)))
                                .unwrap_or_default();
                            job.audit_only = false;
                            capture(&job, staging, config, deadline, disk_pending)
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
    metrics.capture_ms = capture_started.elapsed().as_millis() as u64;
    parse::drain(config, &mut store, &mut metrics, deadline)?;
    metrics.queued = store.conn.query_row(
        "SELECT count(*) FROM extractions WHERE profile=? AND (status IN('queued','running') OR id IN(SELECT value FROM metadata WHERE key LIKE 'retry:%'))",
        [parse::profile(config)?],
        |r| r.get::<_, i64>(0),
    )? as u64;
    metrics.wall_ms = started.elapsed().as_millis() as u64;
    let status = if metrics.errors == 0 && metrics.queued == 0 {
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

fn capture(
    job: &CaptureJob,
    staging: &Path,
    config: &AppConfig,
    run_deadline: Instant,
    disk_pending: &Arc<Mutex<u64>>,
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
    let stable = archive_identity(path).ok().flatten();
    if before.len() != job.file.size_bytes
        || (job.guard.is_some() && stable.as_ref().map(|(_, g)| g) != job.guard.as_ref())
    {
        return Err(error("SOURCE_CHANGED"));
    }
    let audit_only = job.audit_only && job.known_hash.is_some();
    let mut reservation = None;
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
    if audit_only && Some(&hash) != job.known_hash.as_ref() {
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
        let mut second = Sha256::new();
        loop {
            if Instant::now() >= deadline {
                return Err(error("COPY_DEADLINE"));
            }
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
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
    let captured = match result {
        Ok(value) => value,
        Err(e) => {
            metrics.errors += 1;
            store.gap(run, Some(&job.file.path), e.code, &e.message)?;
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
        store.put_json(
            &receipt_path,
            &CaptureReceipt {
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
            },
        )?;
    }
    if !object.exists() {
        fs::create_dir_all(object.parent().unwrap())?;
        let temp = temp_path
            .as_ref()
            .ok_or_else(|| error("ARCHIVE_OBJECT_MISSING"))?;
        fs::rename(temp, &object)?;
        metrics.bytes_written += job.file.size_bytes;
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
    if !unchanged {
        tx.execute("INSERT INTO file_metrics(collection_id,path,version_id,capture_ms,bytes_read,status) VALUES(?,?,?,?,?,?)",params![run,job.file.path,version,elapsed as i64,captured.bytes as i64,"captured"])?;
    }
    tx.commit()?;
    if !unchanged {
        fs::remove_file(receipt_path)?;
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
