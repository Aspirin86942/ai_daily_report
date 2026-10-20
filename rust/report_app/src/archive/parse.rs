use super::{collect::CollectionMetrics, diff, error, file_hash, identity, store::ArchiveStore};
use crate::{config::AppConfig, ReportError};
use ai_daily_scanner_core::{
    parsers::{register_worker, RegisteredWorker, WorkerCommand},
    session::{SessionError, SessionParams, WorkerSession},
};
use ai_daily_worker_contract::{
    archive::{ArchiveArtifact, ArchiveRequest},
    WorkerKind,
};
use chrono::Utc;
use rusqlite::params;
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{mpsc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Job {
    id: String,
    hash: String,
    extension: String,
    object: PathBuf,
    size: u64,
}
enum Event {
    Start(String, mpsc::SyncSender<()>),
    End(
        Job,
        Box<Result<ArchiveArtifact, ReportError>>,
        u64,
        bool,
        u64,
    ),
    Deferred,
    CompressionUnavailable,
}

pub fn profile(config: &AppConfig) -> Result<String, ReportError> {
    super::parse_identity::profile(config)
}

pub fn drain(
    config: &AppConfig,
    store: &mut ArchiveStore,
    metrics: &mut CollectionMetrics,
    deadline: Instant,
) -> Result<(), ReportError> {
    drain_before(config, store, metrics, deadline, None)
}

pub(crate) fn drain_before(
    config: &AppConfig,
    store: &mut ArchiveStore,
    metrics: &mut CollectionMetrics,
    deadline: Instant,
    before: Option<&str>,
) -> Result<(), ReportError> {
    let started = Instant::now();
    let personal = config.report.profile == crate::config::ReportProfile::Personal;
    if personal && Instant::now() >= deadline {
        metrics.warnings.push("RUN_BUDGET_DEFERRED".into());
        return Ok(());
    }
    let verification = super::parse_identity::verified(config)?;
    let profile = verification.profile(config)?;
    verification.persist(config, store)?;
    loop {
        if personal && Instant::now() >= deadline {
            break;
        }
        let versions = {
            let mut stmt=store.conn.prepare("SELECT v.id,v.sha256,v.extension FROM versions v WHERE v.pruned_at IS NULL AND (?2 IS NULL OR julianday(v.observed_at)<julianday(?2)) AND NOT EXISTS(SELECT 1 FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=v.id AND e.profile=?1) ORDER BY v.observed_at DESC,v.id LIMIT ?3")?;
            let values = stmt
                .query_map(
                    params![&profile, before, if personal { 128i64 } else { -1 }],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        if versions.is_empty() {
            break;
        }
        let tx = store.conn.transaction()?;
        for (version, hash, extension) in versions {
            let id = identity(&["extraction", &hash, &extension, &profile]);
            tx.execute("INSERT OR IGNORE INTO extractions(id,sha256,profile,extension,status) VALUES(?,?,?,?,'queued')",params![id,hash,profile,extension])?;
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO version_extractions VALUES(?,?)",
                params![version, id],
            )?;
            if inserted > 0 {
                let status: String =
                    tx.query_row("SELECT status FROM extractions WHERE id=?", [&id], |r| {
                        r.get(0)
                    })?;
                if matches!(status.as_str(), "complete" | "partial") {
                    metrics.parse_reused += 1;
                }
            }
        }
        tx.commit()?;
        if !personal {
            break;
        }
    }
    let jobs = {
        let mut stmt=store.conn.prepare("SELECT e.id,e.sha256,e.extension,o.size_bytes FROM extractions e JOIN objects o ON o.sha256=e.sha256 WHERE (e.status='queued' OR e.id IN(SELECT value FROM metadata WHERE key LIKE 'retry:%')) AND e.attempts<3 AND e.profile=?1 AND EXISTS(SELECT 1 FROM version_extractions ve JOIN versions v ON v.id=ve.version_id WHERE ve.extraction_id=e.id AND v.pruned_at IS NULL AND (?2 IS NULL OR julianday(v.observed_at)<julianday(?2))) ORDER BY CASE WHEN ?3>0 THEN (SELECT max(v.observed_at) FROM versions v JOIN version_extractions ve ON ve.version_id=v.id WHERE ve.extraction_id=e.id) END DESC,e.extension,o.size_bytes,e.id LIMIT ?3")?;
        let values = stmt
            .query_map(
                params![&profile, before, if personal { 128i64 } else { -1 }],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)? as u64,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    let budget = config
        .archive
        .memory_mib
        .min(available_memory_mib()?.saturating_sub(config.archive.system_reserve_mib));
    metrics.memory_budget_mib = budget;
    let workers = config.archive.parse_workers.min(
        budget
            .saturating_sub(config.archive.parent_memory_mib)
            .checked_div(config.archive.worker_memory_mib)
            .unwrap_or(0) as usize,
    );
    if !jobs.is_empty() && workers > 0 && Instant::now() < deadline {
        let office = jobs
            .iter()
            .any(|(_, _, ext, _)| ext != ".pdf")
            .then(|| registered(config, false, &verification, deadline))
            .transpose()?;
        let pdf = jobs
            .iter()
            .any(|(_, _, ext, _)| ext == ".pdf")
            .then(|| registered(config, true, &verification, deadline))
            .transpose()?;
        let audit = serde_json::json!({
            "profile":profile,"engine_build":ai_daily_scanner_core::ENGINE_BUILD_IDENTITY,
            "parser_content_identity":ai_daily_scanner_core::PARSER_CONTENT_IDENTITY,
            "office":office.as_ref().map(|w| &w.identity),
            "pdf":pdf.as_ref().map(|w| &w.identity),
            "office_sha256":office.as_ref().and_then(|_| verification.executable_hash(false)),
            "pdf_sha256":pdf.as_ref().and_then(|_| verification.executable_hash(true))
        });
        let audit_id = crate::run::hash(&audit)?;
        store.conn.execute(
            "INSERT OR IGNORE INTO metadata(key,value) VALUES(?,?)",
            params![format!("parser-build:{audit_id}"), audit.to_string()],
        )?;
        metrics.worker_starts += u64::from(office.is_some()) + u64::from(pdf.is_some());
        let queue = Mutex::new(
            jobs.into_iter()
                .map(|(id, hash, extension, size)| {
                    Ok(Job {
                        object: store.object(&hash)?,
                        id,
                        hash,
                        extension,
                        size,
                    })
                })
                .collect::<Result<VecDeque<_>, ReportError>>()?,
        );
        let staging = store.root.join("staging");
        let output = store.root.join("extractions");
        let quota = store.quota.clone();
        let (sender, receiver) = mpsc::sync_channel(workers * 2);
        std::thread::scope(|scope| -> Result<(), ReportError> {
            for _ in 0..workers {
                let sender = sender.clone();
                let queue = &queue;
                let staging = &staging;
                let output = &output;
                let office = &office;
                let pdf = &pdf;
                let quota = &quota;
                scope.spawn(move || {
                    let mut session: Option<(bool, WorkerSession)> = None;
                    loop {
                        let Some(job) = queue.lock().unwrap().pop_front() else {
                            break;
                        };
                        if super::collect::free_disk(staging)
                            .unwrap_or(0)
                            .saturating_sub(
                                config.archive.max_output_mib * 1024 * 1024 * workers as u64,
                            )
                            < config.archive.reserve_free_mib * 1024 * 1024
                        {
                            let _ = sender.send(Event::Deferred);
                            continue;
                        }
                        if Instant::now() >= deadline
                            || available_memory_mib().unwrap_or(0)
                                < config.archive.system_reserve_mib
                        {
                            let _ = sender.send(Event::Deferred);
                            continue;
                        }
                        let (ack, ready) = mpsc::sync_channel(0);
                        if sender.send(Event::Start(job.id.clone(), ack)).is_err()
                            || ready.recv().is_err()
                        {
                            break;
                        }
                        let now = Instant::now();
                        let is_pdf = job.extension == ".pdf";
                        let mut spawned = false;
                        let result = (|| {
                            let mut reservation = quota
                                .as_ref()
                                .map(|q| q.reserve(config.archive.max_output_mib * 1024 * 1024))
                                .transpose()?;
                            if job.size > config.archive.max_input_mib * 1024 * 1024 {
                                return Err(error("FILE_TOO_LARGE"));
                            }
                            if super::file_hash_bounded(&job.object, deadline)? != job.hash {
                                return Err(error("ARCHIVE_OBJECT_CORRUPT"));
                            }
                            let worker = if is_pdf {
                                pdf.as_ref()
                            } else {
                                office.as_ref()
                            }
                            .ok_or_else(|| error("ARCHIVE_WORKER_MISSING"))?;
                            if session.as_ref().is_some_and(|(kind, s)| {
                                *kind != is_pdf || !s.is_alive() || s.recycle_due()
                            }) {
                                session = None;
                            }
                            if session.is_none() {
                                let params = SessionParams {
                                    concurrency: 1,
                                    max_requests_per_session: 128,
                                    idle_ttl: Duration::from_secs(60),
                                    rss_limit_bytes: config.archive.worker_memory_mib * 1024 * 1024,
                                };
                                let worker_session = WorkerSession::start(
                                    &worker.command,
                                    &worker.identity,
                                    params,
                                    Duration::from_secs(10)
                                        .min(deadline.saturating_duration_since(Instant::now())),
                                )
                                .map_err(session_error)?;
                                worker_session
                                    .limit_memory(params.rss_limit_bytes)
                                    .map_err(session_error)?;
                                session = Some((is_pdf, worker_session));
                                spawned = true;
                            }
                            let directory = tempfile::Builder::new()
                                .prefix("parse-")
                                .tempdir_in(staging)?;
                            if let Some(reservation) = &mut reservation {
                                reservation.track_empty_directory(directory.path())?;
                            }
                            if config.archive.compress_extractions {
                                // Unsupported filesystems keep the same readable format.
                                if super::storage::compress(directory.path()).is_err() {
                                    let _ = sender.send(Event::CompressionUnavailable);
                                }
                            }
                            let timeout = Duration::from_secs(config.archive.parse_seconds)
                                .min(deadline.saturating_duration_since(Instant::now()));
                            if timeout.is_zero() {
                                return Err(error("PARSE_DEADLINE"));
                            }
                            let worker = &mut session.as_mut().unwrap().1;
                            let artifact = if matches!(
                                job.extension.as_str(),
                                ".txt" | ".md" | ".csv" | ".json" | ".log"
                            ) {
                                let request = super::parse_request::text(
                                    config,
                                    &job.object,
                                    &job.extension,
                                    &job.hash,
                                    directory.path(),
                                    timeout,
                                );
                                worker.archive_text(&request_id(), &request, timeout)
                            } else {
                                let request = ArchiveRequest {
                                    source: super::parse_request::source(
                                        config,
                                        &job.object,
                                        &job.extension,
                                        timeout,
                                    )?,
                                    raw_sha256: job.hash.clone(),
                                    output_dir: directory.path().to_owned(),
                                    max_output_bytes: config.archive.max_output_mib * 1024 * 1024,
                                };
                                worker.archive_parse(&request_id(), &request, timeout)
                            }
                            .map_err(session_error)?;
                            verify_artifact_until(
                                directory.path(),
                                &artifact,
                                &job.hash,
                                Some(deadline),
                            )?;
                            let destination = output.join(&job.id);
                            if destination.exists() {
                                verify_artifact_until(
                                    &destination,
                                    &artifact,
                                    &job.hash,
                                    Some(deadline),
                                )?;
                            } else {
                                if let Some(reservation) = &mut reservation {
                                    reservation.track(&destination)?;
                                }
                                fs::rename(directory.path(), &destination)?;
                            }
                            if let Some(reservation) = reservation {
                                if destination.exists() && !directory.path().exists() {
                                    reservation.commit(&[destination])?;
                                }
                            }
                            Ok(artifact)
                        })();
                        let peak = session
                            .as_ref()
                            .and_then(|(_, s)| s.peak_rss_bytes())
                            .unwrap_or(0);
                        if result.is_err() {
                            session = None;
                        }
                        if sender
                            .send(Event::End(
                                job,
                                Box::new(result),
                                now.elapsed().as_millis() as u64,
                                spawned,
                                peak,
                            ))
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
            drop(sender);
            for event in receiver {
                match event {
                    Event::Start(id, ack) => {
                        // A resumed parse is real work even when its source is unchanged.
                        store.conn.execute(
                            "INSERT OR IGNORE INTO file_metrics(collection_id,path,version_id,status)
                             SELECT ?1,v.path,v.id,'queued' FROM versions v
                             JOIN version_extractions ve ON ve.version_id=v.id
                             WHERE ve.extraction_id=?2 AND v.pruned_at IS NULL
                             ORDER BY v.observed_at DESC,v.rowid DESC",
                            params![metrics.run_id, id],
                        )?;
                        metrics.queue_work_ms += started.elapsed().as_millis() as u64;
                        store.conn.execute("UPDATE extractions SET status=CASE WHEN manifest_json IS NULL THEN 'running' ELSE status END,attempts=attempts+1,started_at=? WHERE id=?",params![Utc::now().to_rfc3339(),id])?;
                        store.conn.execute("UPDATE file_metrics SET queue_ms=? WHERE collection_id=? AND version_id IN(SELECT version_id FROM version_extractions WHERE extraction_id=?)",params![started.elapsed().as_millis() as i64,metrics.run_id,id])?;
                        let _ = ack.send(());
                    }
                    Event::Deferred => {}
                    Event::CompressionUnavailable => {
                        if !metrics
                            .warnings
                            .iter()
                            .any(|w| w == "ARCHIVE_COMPRESSION_UNAVAILABLE")
                        {
                            metrics
                                .warnings
                                .push("ARCHIVE_COMPRESSION_UNAVAILABLE".into());
                        }
                    }
                    Event::End(job, result, elapsed, spawned, peak) => {
                        metrics.parse_work_ms += elapsed;
                        metrics.worker_peak_bytes = metrics.worker_peak_bytes.max(peak);
                        metrics.parse_read_bytes += job.size;
                        metrics.parse_jobs += 1;
                        metrics.worker_starts += u64::from(spawned);
                        let at = Utc::now().to_rfc3339();
                        match *result {
                            Ok(artifact) => {
                                store.conn.execute(
                                    "DELETE FROM metadata WHERE key=?",
                                    [format!("retry:{}", job.id)],
                                )?;
                                store.conn.execute("UPDATE gaps SET resolved_at=? WHERE path=? AND resolved_at IS NULL", params![at,format!("extraction:{}",job.id)])?;
                                let status = if artifact.truncated || !artifact.gaps.is_empty() {
                                    "partial"
                                } else {
                                    "complete"
                                };
                                store.conn.execute("UPDATE extractions SET status=?,ready_at=?,manifest_json=?,error_code=NULL WHERE id=?",params![status,at,serde_json::to_string(&artifact)?,job.id])?;
                                store.conn.execute("UPDATE file_metrics SET parse_ms=?,backend=?,lane=?,status=? WHERE collection_id=? AND version_id IN(SELECT version_id FROM version_extractions WHERE extraction_id=?)",params![elapsed as i64,artifact.parser_backend,artifact.worker_lane,status,metrics.run_id,job.id])?;
                            }
                            Err(e) => {
                                if e.code == "PROCESSING_CACHE_FULL" {
                                    defer_run_budget(store, &job.id)?;
                                    if !metrics.warnings.iter().any(|w| w == e.code) {
                                        metrics.warnings.push(e.code.into());
                                    }
                                    continue;
                                }
                                if Instant::now() >= deadline
                                    && (e.code == "PARSE_DEADLINE" || e.message == "PARSE_DEADLINE")
                                {
                                    defer_run_budget(store, &job.id)?;
                                    if !metrics
                                        .warnings
                                        .iter()
                                        .any(|code| code == "RUN_BUDGET_DEFERRED")
                                    {
                                        metrics.warnings.push("RUN_BUDGET_DEFERRED".into());
                                    }
                                    continue;
                                }
                                metrics.errors += 1;
                                store.gap(
                                    &metrics.run_id,
                                    Some(&format!("extraction:{}", job.id)),
                                    e.code,
                                    &e.message,
                                )?;
                                let retry = matches!(
                                    e.code,
                                    "ARCHIVE_WORKER_TRANSIENT"
                                        | "PARSE_DEADLINE"
                                        | "ARCHIVE_OUTPUT_FAILED"
                                );
                                store.conn.execute("UPDATE extractions SET status=CASE WHEN manifest_json IS NOT NULL THEN status WHEN ? AND attempts<3 THEN 'queued' ELSE 'failed' END,error_code=? WHERE id=?",params![retry,if e.code=="ARCHIVE_PARSE_REJECTED" {e.message.as_str()}else{e.code},job.id])?;
                                store.conn.execute("DELETE FROM metadata WHERE key=? AND (NOT ? OR (SELECT attempts FROM extractions WHERE id=?)>=3)",params![format!("retry:{}",job.id),retry,job.id])?;
                                store.conn.execute("UPDATE file_metrics SET parse_ms=?,status=? WHERE collection_id=? AND version_id IN(SELECT version_id FROM version_extractions WHERE extraction_id=?)",params![elapsed as i64,e.code,metrics.run_id,job.id])?;
                            }
                        }
                    }
                }
            }
            Ok(())
        })?;
    } else if workers == 0 && !jobs.is_empty() {
        metrics.warnings.push("MEMORY_RESERVE".into());
    }
    store.conn.execute("UPDATE file_metrics SET backend=(SELECT json_extract(e.manifest_json,'$.parser_backend') FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=file_metrics.version_id AND e.profile=?1),lane=(SELECT json_extract(e.manifest_json,'$.worker_lane') FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=file_metrics.version_id AND e.profile=?1) WHERE collection_id=?2 AND backend IS NULL", params![profile,metrics.run_id])?;
    metrics.parse_ms = started.elapsed().as_millis() as u64;
    let diff_started = Instant::now();
    if config.report.profile != crate::config::ReportProfile::Personal {
        diff::prepare_changes(store, config, &profile, deadline, before)?;
    }
    metrics.diff_ms = diff_started.elapsed().as_millis() as u64;
    Ok(())
}

fn registered(
    config: &AppConfig,
    pdf: bool,
    verification: &super::parse_identity::Verification,
    deadline: Instant,
) -> Result<RegisteredWorker, ReportError> {
    let program = if pdf {
        config.pdf_worker_path.clone()
    } else {
        config.office_worker_path.clone()
    };
    let command = WorkerCommand {
        current_dir: program.parent().map(|p| p.to_owned()),
        program,
        base_args: vec![],
        expected_kind: if pdf {
            WorkerKind::Pdf
        } else {
            WorkerKind::Office
        },
        required_backends: vec![],
        required_extensions: vec![],
    };
    let timeout = Duration::from_secs(10).min(deadline.saturating_duration_since(Instant::now()));
    if timeout.is_zero() {
        return Err(error("PARSE_DEADLINE"));
    }
    let worker = register_worker(&command, timeout)
        .map_err(|e| ReportError::new("ARCHIVE_WORKER_MISSING", format!("{e:?}")))?;
    verification.check_worker(config, pdf, &worker.identity.worker_build)?;
    Ok(worker)
}
fn session_error(e: SessionError) -> ReportError {
    match e {
        SessionError::Rejected(d) => ReportError::new(
            if d.retryable {
                "ARCHIVE_WORKER_TRANSIENT"
            } else {
                "ARCHIVE_PARSE_REJECTED"
            },
            d.error_code,
        ),
        SessionError::Timeout => error("PARSE_DEADLINE"),
        other => ReportError::new("ARCHIVE_WORKER_TRANSIENT", format!("{other:?}")),
    }
}
pub fn verify_artifact(
    dir: &std::path::Path,
    a: &ArchiveArtifact,
    hash: &str,
) -> Result<(), ReportError> {
    verify_artifact_until(dir, a, hash, None)
}

fn verify_artifact_until(
    dir: &std::path::Path,
    a: &ArchiveArtifact,
    hash: &str,
    deadline: Option<Instant>,
) -> Result<(), ReportError> {
    if a.version != 1 || a.raw_sha256 != hash || a.error.is_some() {
        return Err(error("ARCHIVE_EXTRACTION_INVALID"));
    }
    for (file, expected) in [
        ("body.md", &a.body_sha256),
        ("units.jsonl", &a.units_sha256),
        ("units.idx", &a.index_sha256),
    ] {
        let actual = match deadline {
            Some(deadline) => super::file_hash_bounded(&dir.join(file), deadline)?,
            None => file_hash(&dir.join(file))?,
        };
        if actual != *expected {
            return Err(error("ARCHIVE_EXTRACTION_CORRUPT"));
        }
    }
    if fs::metadata(dir.join("body.md"))?.len() != a.body_bytes
        || fs::metadata(dir.join("units.jsonl"))?.len() != a.units_bytes
        || fs::metadata(dir.join("units.idx"))?.len() != a.units * 8
    {
        return Err(error("ARCHIVE_EXTRACTION_INVALID"));
    }
    Ok(())
}

#[cfg(windows)]
pub fn available_memory_mib() -> Result<u64, ReportError> {
    use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    let mut info: PERFORMANCE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        GetPerformanceInfo(
            &mut info,
            std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((info.PhysicalAvailable as u64).saturating_mul(info.PageSize as u64) / (1024 * 1024))
}
#[cfg(not(windows))]
pub fn available_memory_mib() -> Result<u64, ReportError> {
    Err(error("ARCHIVE_MEMORY_QUERY_UNSUPPORTED"))
}

fn defer_run_budget(store: &ArchiveStore, id: &str) -> Result<(), ReportError> {
    // A bounded queue pass is scheduling, not a failed full per-file attempt.
    store.conn.execute(
        "UPDATE extractions SET status=CASE WHEN manifest_json IS NULL THEN 'queued' ELSE status END,attempts=max(0,attempts-1),error_code=NULL WHERE id=?",
        [id],
    )?;
    Ok(())
}

fn request_id() -> String {
    let id = crate::types::new_run_id();
    format!(
        "{}-{}-{}-{}-{}",
        &id[..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..]
    )
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn run_budget_deferral_preserves_retry_attempts_and_readable_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArchiveStore::at(dir.path(), true).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO objects VALUES('raw',1,'2026-10-19T00:00:00Z')",
                [],
            )
            .unwrap();
        store.conn.execute("INSERT INTO extractions(id,sha256,profile,extension,status) VALUES('job','raw','profile','.pdf','queued')", []).unwrap();
        for _ in 0..4 {
            store
                .conn
                .execute(
                    "UPDATE extractions SET status='running',attempts=attempts+1 WHERE id='job'",
                    [],
                )
                .unwrap();
            defer_run_budget(&store, "job").unwrap();
            let state: (String, i64) = store
                .conn
                .query_row(
                    "SELECT status,attempts FROM extractions WHERE id='job'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                state,
                ("queued".into(), 0),
                "a short run budget must not exhaust per-file retries"
            );
        }
        store.conn.execute("UPDATE extractions SET status='partial',manifest_json='{}',attempts=1 WHERE id='job'", []).unwrap();
        defer_run_budget(&store, "job").unwrap();
        let state: (String, String, i64) = store
            .conn
            .query_row(
                "SELECT status,manifest_json,attempts FROM extractions WHERE id='job'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, ("partial".into(), "{}".into(), 0));
    }
}
