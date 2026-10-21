//! Process durable archive bytes without a new source inventory or model request.
use super::{collect, error, parse, store::ArchiveStore};
use crate::{config::AppConfig, store::locking::ReportLock, types, ReportError};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::params;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub fn pending(config: &AppConfig, before: Option<&str>) -> Result<Value, ReportError> {
    let store = ArchiveStore::open_collection(config, false)?;
    pending_in(&store, config, before)
}

pub(crate) fn pending_in(
    store: &ArchiveStore,
    config: &AppConfig,
    before: Option<&str>,
) -> Result<Value, ReportError> {
    let profile = parse::profile(config)?;
    let scope = super::demand::scope(
        config,
        store,
        before,
        Instant::now() + Duration::from_secs(30),
    )?;
    // Count one job per extraction, including versions not yet queued for this profile.
    let (queued, oldest): (i64, Option<String>) = store.conn.query_row(
        "WITH jobs AS (
          SELECT e.sha256||e.extension AS id,min(v.observed_at) AS observed_at
          FROM extractions e JOIN version_extractions ve ON ve.extraction_id=e.id
          JOIN versions v ON v.id=ve.version_id
          WHERE e.profile=?1 AND (e.status IN ('queued','running') OR e.id IN (SELECT value FROM metadata WHERE key LIKE 'retry:%'))
          AND v.pruned_at IS NULL AND (?2 IS NULL OR julianday(v.observed_at)<julianday(?2))
          AND (?3 IS NULL OR v.id IN (SELECT value FROM json_each(?3))
               OR e.id IN (SELECT value FROM metadata WHERE key LIKE 'retry:%')) GROUP BY e.id
          UNION ALL
          SELECT v.sha256||v.extension,min(v.observed_at)
          FROM versions v WHERE v.pruned_at IS NULL AND (?2 IS NULL OR julianday(v.observed_at)<julianday(?2))
          AND (?3 IS NULL OR v.id IN (SELECT value FROM json_each(?3)))
          AND NOT EXISTS(SELECT 1 FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=v.id AND e.profile=?1)
          GROUP BY v.sha256,v.extension
        ) SELECT count(DISTINCT id),min(observed_at) FROM jobs",
        params![profile, before, scope],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let age = oldest
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| (Utc::now() - at.with_timezone(&Utc)).num_seconds().max(0));
    let changes = if config.report.profile == crate::config::ReportProfile::Personal {
        0
    } else {
        pending_changes(store, &profile, before)?
    };
    let (standby, unavailable) = if scope.is_some() {
        let standby: i64 = store.conn.query_row(
            "SELECT count(*) FROM (SELECT v.sha256,v.extension FROM versions v
             LEFT JOIN extractions e ON e.sha256=v.sha256 AND e.extension=v.extension AND e.profile=?1
             WHERE v.baseline=1 AND v.pruned_at IS NULL
             AND (e.id IS NULL OR (e.status NOT IN ('complete','partial') AND e.id NOT IN
                  (SELECT value FROM metadata WHERE key LIKE 'retry:%')))
             AND NOT EXISTS(SELECT 1 FROM versions needed WHERE needed.id IN (SELECT value FROM json_each(?2))
                 AND needed.sha256=v.sha256 AND needed.extension=v.extension)
             GROUP BY v.sha256,v.extension)", params![profile,scope], |r| r.get(0))?;
        let unavailable: i64 = store.conn.query_row(
            "SELECT count(*) FROM (SELECT v.sha256,v.extension FROM versions v
             LEFT JOIN extractions e ON e.sha256=v.sha256 AND e.extension=v.extension AND e.profile=?1
             WHERE v.id IN (SELECT value FROM json_each(?2)) AND
             (v.pruned_at IS NOT NULL OR e.status IN ('failed','expired') OR
              (e.status IN ('queued','running') AND e.attempts>=3)) GROUP BY v.sha256,v.extension)",
            params![profile,scope], |r| r.get(0))?;
        (standby, unavailable)
    } else {
        (0, 0)
    };
    Ok(
        json!({"queued":queued,"pending_changes":changes,"oldest_observed_at":oldest,
        "oldest_age_seconds":age,"standby_baselines":standby,"unavailable":unavailable}),
    )
}

pub(crate) fn pending_changes(
    store: &ArchiveStore,
    profile: &str,
    before: Option<&str>,
) -> Result<u64, ReportError> {
    let count: i64 = store.conn.query_row(
        "SELECT count(*) FROM versions v JOIN version_extractions ve ON ve.version_id=v.id JOIN extractions e ON e.id=ve.extraction_id
         WHERE v.baseline=0 AND v.pruned_at IS NULL AND e.profile=?1 AND e.status IN('complete','partial')
         AND (?2 IS NULL OR julianday(v.observed_at)<julianday(?2))
         AND NOT EXISTS(SELECT 1 FROM changes c WHERE c.version_id=v.id AND c.extraction_id=e.id)",
        params![profile,before], |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// Bounded material preparation before a new scheduled archive report is frozen.
pub fn prepare_daily(
    config: &AppConfig,
    target: NaiveDate,
    now: DateTime<Utc>,
) -> Result<Value, ReportError> {
    use chrono::TimeZone;
    let started = Instant::now();
    let (_, end) = types::utc_window(target)?;
    let before = end.to_rfc3339();
    let initial = pending(config, Some(&before))?;
    let drain = if initial["queued"].as_u64().unwrap_or(0) > 0
        || initial["pending_changes"].as_u64().unwrap_or(0) > 0
    {
        let result = execute(config, true, Some(target), 120, now);
        crate::operations::append_scheduled_log(
            config,
            "parse",
            started.elapsed().as_millis() as u64,
            None,
            Some(&target.to_string()),
            &result,
        )?;
        result?
    } else {
        json!({"queued":0,"model_calls":0})
    };
    let remaining = pending(config, Some(&before))?;
    let queued = remaining["queued"].as_u64().unwrap_or(0);
    let changes = remaining["pending_changes"].as_u64().unwrap_or(0);
    let materials_pending = queued + changes > 0;
    let due_day = target
        .succ_opt()
        .ok_or_else(|| error("ARCHIVE_TIME_INVALID"))?;
    let due = types::shanghai()
        .from_local_datetime(&due_day.and_time(config.schedule.due_time()?))
        .single()
        .ok_or_else(|| error("ARCHIVE_TIME_INVALID"))?;
    let cutoff = due.with_timezone(&Utc) + chrono::Duration::minutes(30);
    let finished = now + chrono::Duration::from_std(started.elapsed()).unwrap_or_default();
    let deferred = materials_pending && finished < cutoff;
    Ok(json!({
        "status": if deferred { "deferred" } else { "succeeded" },
        "action":if deferred { "deferred" } else { "ready" },
        "execution_status":if deferred { "deferred" } else { "succeeded" },
        "material_status":if materials_pending { "partial" } else { "complete" },
        "publication_status":"not_published","target_date":target,"queued":queued,"pending_changes":changes,
        "reason_codes":if materials_pending { vec!["ARCHIVE_INPUT_PENDING"] } else { vec![] },
        "cutoff_at":cutoff,"drain":drain,"model_calls":0
    }))
}

fn skipped(reason: &str) -> Value {
    json!({
        "operation":"parse","status":"skipped","execution_status":"skipped",
        "action":"skipped","reason":reason,"reason_codes":[reason],
        "material_status":"unknown","publication_status":"not_applicable",
        "model_calls":0,"bytes_read":0,"versions":0,"parse_jobs":0,"worker_starts":0
    })
}

pub fn execute(
    config: &AppConfig,
    scheduled: bool,
    target_date: Option<NaiveDate>,
    max_seconds: u64,
    now: DateTime<Utc>,
) -> Result<Value, ReportError> {
    if !(1..=1200).contains(&max_seconds) {
        return Err(error("ARCHIVE_PARSE_BUDGET_INVALID"));
    }
    if scheduled
        && config
            .archive
            .collection_workday_window
            .as_ref()
            .is_some_and(|window| !window.allows(now))
    {
        return Ok(skipped("outside_collection_workday_window"));
    }
    // Validate the existing archive before creating locks or process resources.
    let existing = ArchiveStore::open_collection(config, false)?;
    let initialized: bool = existing.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='baseline')",
        [],
        |row| row.get(0),
    )?;
    if !initialized {
        return Err(error("ARCHIVE_BASELINE_REQUIRED"));
    }
    drop(existing);
    let started = Instant::now();
    let personal = config.report.profile == crate::config::ReportProfile::Personal;
    let deadline = started
        + Duration::from_secs(
            max_seconds
                .min(config.archive.run_seconds)
                .min(if personal { 120 } else { 1200 }),
        );
    let processing = if personal {
        match crate::processing_cache::ProcessingSession::at(
            &config.paths.data_dir.join("daily_cache"),
            now,
            deadline,
            crate::processing_cache::MAX_BYTES,
            crate::processing_cache::CONTROL_BYTES,
            Duration::ZERO,
        ) {
            Ok(session) => Some(session),
            Err(e) if e.code == "REPORT_ALREADY_RUNNING" => return Ok(skipped("processing_busy")),
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    let _lock = if !personal {
        Some(
            match ReportLock::acquire(
                &config.paths.data_dir.join("archive/collector"),
                "archive",
                Duration::ZERO,
            ) {
                Ok(lock) => lock,
                Err(error) if scheduled && error.code == "REPORT_ALREADY_RUNNING" => {
                    return Ok(skipped("archive_busy"))
                }
                Err(error) => return Err(error),
            },
        )
    } else {
        None
    };
    let resources = super::resources::Budget::new(config.archive.memory_mib)?;
    let store = if let Some(session) = &processing {
        session.open_archive(config)?
    } else {
        ArchiveStore::open(config, true)?
    };
    let mut result = drain_store(config, store, target_date, now, deadline)?;
    result["resources"] = resources.sample()?;
    Ok(result)
}

pub(crate) fn prepare_personal(
    config: &AppConfig,
    session: &crate::processing_cache::ProcessingSession,
    target_date: NaiveDate,
    now: DateTime<Utc>,
    deadline: Instant,
) -> Result<Value, ReportError> {
    let store = session.open_archive(config)?;
    drain_store(
        config,
        store,
        Some(target_date),
        now,
        deadline.min(Instant::now() + Duration::from_secs(120)),
    )
}

fn drain_store(
    config: &AppConfig,
    mut store: ArchiveStore,
    target_date: Option<NaiveDate>,
    now: DateTime<Utc>,
    deadline: Instant,
) -> Result<Value, ReportError> {
    let started = Instant::now();
    let before = target_date
        .map(types::utc_window)
        .transpose()?
        .map(|(_, end)| end.to_rfc3339());
    collect::recover_captures(&mut store)?;
    store.conn.execute(
        "UPDATE collections SET status='parse_interrupted',finished_at=? WHERE status='parse_running'",
        [now.to_rfc3339()],
    )?;
    let run_id = types::new_run_id();
    store.conn.execute(
        "INSERT INTO collections(id,started_at,status,baseline,metrics_json) VALUES(?,?,'parse_running',0,?)",
        params![run_id,now.to_rfc3339(),json!({"operation":"parse","target_date":target_date}).to_string()],
    )?;
    let mut metrics = collect::CollectionMetrics {
        run_id: run_id.clone(),
        ..Default::default()
    };
    let result = (|| {
        collect::recover_parses(&mut store, &run_id)?;
        parse::drain_before(
            config,
            &mut store,
            &mut metrics,
            deadline,
            before.as_deref(),
        )
    })();
    metrics.wall_ms = started.elapsed().as_millis() as u64;
    let remaining = pending_in(&store, config, before.as_deref())?;
    metrics.queued = remaining["queued"].as_u64().unwrap_or(0);
    let changes = remaining["pending_changes"].as_u64().unwrap_or(0);
    let unavailable = remaining["unavailable"].as_u64().unwrap_or(0);
    let mut value = collect::result_value(&metrics, "parse")?;
    value["pending_changes"] = json!(changes);
    value["standby_baselines"] = remaining["standby_baselines"].clone();
    value["unavailable"] = json!(unavailable);
    if changes > 0 || unavailable > 0 {
        value["status"] = json!("partial");
        value["material_status"] = json!("partial");
    }
    value["target_date"] = json!(target_date);
    let mut reasons = metrics.warnings.clone();
    if unavailable > 0 {
        reasons.push("PERSONAL_INPUT_UNAVAILABLE".into());
    }
    if changes > 0 {
        reasons.push("ARCHIVE_CHANGE_PENDING".into());
    }
    if metrics.queued > 0 && metrics.warnings.is_empty() {
        reasons.push("ARCHIVE_PARSE_DEFERRED".into());
    }
    value["reason_codes"] = json!(reasons);
    let status = if let Err(error) = &result {
        value["status"] = json!("failed");
        value["execution_status"] = json!("failed");
        value["error_code"] = json!(error.code);
        "parse_failed"
    } else if metrics.errors == 0 && metrics.queued == 0 && changes == 0 && unavailable == 0 {
        "parse_complete"
    } else {
        "parse_partial"
    };
    store.conn.execute(
        "UPDATE collections SET status=?,finished_at=?,metrics_json=? WHERE id=?",
        params![status, Utc::now().to_rfc3339(), value.to_string(), run_id],
    )?;
    result?;
    Ok(value)
}
