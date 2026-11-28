use super::{
    query, RunRecord, CURRENT_PERIOD_PREFIX, PERSONAL_CALLS_PREFIX, PERSONAL_RUN_PREFIX,
    PERSONAL_SUMMARY_PREFIX,
};
use crate::ReportError;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::{BTreeMap, BTreeSet};

const VALID_PUBLISHED_SUMMARY_PREDICATE: &str =
    "(COALESCE(json_extract(p.report_json,'$.schema_version'),0) NOT IN (5,6)
      OR json_array_length(p.report_json,'$.items')>0 OR p.quality='no_changes')";

pub(crate) struct PublishedDailyRow {
    pub run_id: String,
    pub quality: Option<String>,
    pub output_path: Option<String>,
    pub expected_sha256: Option<String>,
    pub report_json: Option<String>,
}

pub(crate) struct RunAttentionCounts {
    pub pending: usize,
    pub recent_failures: usize,
    pub resolved_failures: usize,
    pub failures: Vec<String>,
}

pub(crate) struct PersonalRecoveryCandidateRow {
    pub target_date: String,
    pub run_id: String,
    pub report_json: Option<String>,
    pub error_code: Option<String>,
    pub calls_budget_json: Option<String>,
}

pub(crate) struct UnresolvedPersonalRunRow {
    pub run_id: String,
    pub snapshot_path: Option<String>,
    pub snapshot_sha256: Option<String>,
}

pub(crate) fn personal_run_key(run_id: &str) -> String {
    format!("{PERSONAL_RUN_PREFIX}{run_id}")
}

pub(crate) fn personal_calls_key(scope: &str, date: &str) -> String {
    format!("{PERSONAL_CALLS_PREFIX}{scope}:{date}")
}

pub(crate) fn personal_summary_key(scope: &str, date: &str) -> String {
    format!("{PERSONAL_SUMMARY_PREFIX}{scope}:{date}")
}

pub(crate) fn is_personal_run_meta(metadata: &BTreeMap<String, String>, run_id: &str) -> bool {
    metadata
        .get(&personal_run_key(run_id))
        .is_some_and(|value| value == "5")
}

pub(crate) fn schema_migrations_present(conn: &Connection) -> bool {
    conn.query_row("SELECT COUNT(*) FROM report_schema_migrations", [], |row| {
        row.get::<_, i64>(0)
    })
    .is_ok()
}

pub(crate) fn published_daily_report(
    conn: &Connection,
    target: NaiveDate,
) -> Result<Option<PublishedDailyRow>, ReportError> {
    let row = conn
        .query_row(
            "SELECT r.run_id,r.quality,r.output_path,r.expected_sha256,r.report_json
             FROM report_runs r LEFT JOIN daily_reports d ON d.date=r.target_date
             WHERE r.target_date=?1 AND r.run_kind='report' AND r.status='succeeded' AND r.phase='published'
               AND (d.current_run_id IS NULL OR d.current_run_id=r.run_id)
             ORDER BY r.created_at DESC LIMIT 1",
            [target.to_string()],
            |row| {
                <(
                    String,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                )>::try_from(row)
            },
        )
        .optional()?;
    Ok(row.map(
        |(run_id, quality, output_path, expected_sha256, report_json)| PublishedDailyRow {
            run_id,
            quality,
            output_path,
            expected_sha256,
            report_json,
        },
    ))
}

pub(crate) fn partial_daily_report_json(conn: &Connection, target: NaiveDate) -> Option<String> {
    conn.query_row(
        "SELECT r.report_json FROM report_runs r JOIN daily_reports d ON d.current_run_id=r.run_id WHERE d.date=? AND r.quality='partial'",
        [target.to_string()],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

pub(crate) fn active_report_runs(
    conn: &Connection,
    scope_id: &str,
    run_budget_seconds: u64,
    now: DateTime<Utc>,
) -> Result<Vec<(String, String)>, ReportError> {
    let mut statement = conn.prepare(
        "SELECT run_id,target_date,created_at FROM report_runs WHERE scope_id=?
         AND run_kind='report' AND status='running' AND finished_at IS NULL AND error_code IS NULL
         AND phase IN ('created','scanning','prepared','generating','db_committed')",
    )?;
    let rows = statement.query_map([scope_id], |row| row.try_into())?;
    let mut active = Vec::new();
    for row in rows {
        let (id, date, created_at): (String, String, String) = row?;
        if DateTime::parse_from_rfc3339(&created_at).is_ok_and(|started| {
            (now - started.with_timezone(&Utc))
                .to_std()
                .is_ok_and(|elapsed| elapsed.as_secs() < run_budget_seconds)
        }) {
            active.push((id, date));
        }
    }
    Ok(active)
}

pub(crate) fn run_attention_counts(
    conn: &Connection,
    now: DateTime<Utc>,
    active: &[(String, String)],
) -> Result<RunAttentionCounts, ReportError> {
    let pending_sql = format!(
        "SELECT run_id FROM report_runs r WHERE run_kind='report' AND phase IN ('prepared','generating','db_committed')
         AND status!='succeeded' AND COALESCE(error_code,'')!='RUN_SUPERSEDED' AND COALESCE(regeneration_reason,'') NOT LIKE 'coverage_publish:%'
         AND NOT EXISTS(SELECT 1 FROM daily_reports d JOIN report_runs p ON p.run_id=d.current_run_id
             WHERE p.scope_id=r.scope_id AND p.target_date=r.target_date AND p.created_at>r.created_at
             AND p.status='succeeded' AND p.phase='published'
             AND {VALID_PUBLISHED_SUMMARY_PREDICATE})"
    );
    let mut pending_statement = conn.prepare(&pending_sql)?;
    let pending = pending_statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|id| !active.iter().any(|(active_id, _)| active_id == id))
        .count();
    let cutoff = now - Duration::hours(24);
    let failures_sql = format!(
        "SELECT r.created_at, (
             SELECT MAX(COALESCE(p.finished_at,p.created_at)) FROM report_runs p
             WHERE p.scope_id=r.scope_id AND p.target_date=r.target_date
               AND p.run_kind='report' AND p.status='succeeded' AND p.phase='published'
               AND {VALID_PUBLISHED_SUMMARY_PREDICATE}
         ) FROM report_runs r WHERE r.status='failed'"
    );
    let mut statement = conn.prepare(&failures_sql)?;
    let rows = statement.query_map([], |row| row.try_into())?;
    let mut recent_failures = 0usize;
    let mut resolved_failures = 0usize;
    let mut failures = Vec::new();
    for row in rows {
        let (text, published): (String, Option<String>) = row?;
        let Ok(at) = DateTime::parse_from_rfc3339(&text) else {
            continue;
        };
        let at = at.with_timezone(&Utc);
        if at < cutoff {
            continue;
        }
        let resolved = published
            .and_then(|published_at| DateTime::parse_from_rfc3339(&published_at).ok())
            .is_some_and(|published_at| published_at > at);
        if resolved {
            resolved_failures += 1;
            continue;
        }
        recent_failures += 1;
        if failures.len() < 5 {
            failures.push(text);
        }
    }
    Ok(RunAttentionCounts {
        pending,
        recent_failures,
        resolved_failures,
        failures,
    })
}

pub(crate) fn has_verified_model_check(
    conn: &Connection,
    config_fingerprint: &str,
) -> Result<bool, ReportError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM report_runs WHERE run_kind='model_check' AND status='succeeded' AND config_fingerprint=? LIMIT 1",
            [config_fingerprint],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

pub(crate) fn run_phase(conn: &Connection, run_id: &str) -> String {
    conn.query_row(
        "SELECT phase FROM report_runs WHERE run_id=?",
        [run_id],
        |row| row.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "unknown".into())
}

pub(crate) fn personal_recovery_candidates(
    conn: &Connection,
    scope: &str,
) -> Result<Vec<PersonalRecoveryCandidateRow>, ReportError> {
    let calls_prefix = PERSONAL_CALLS_PREFIX;
    let run_prefix = PERSONAL_RUN_PREFIX;
    let sql = format!(
        "SELECT r.target_date,r.run_id,r.report_json,r.error_code,
         (SELECT value FROM report_metadata WHERE key='{calls_prefix}'||r.scope_id||':'||r.target_date)
         FROM report_runs r WHERE r.scope_id=?1 AND r.run_kind='report'
         AND EXISTS(SELECT 1 FROM report_metadata m WHERE m.key='{run_prefix}'||r.run_id)
         AND (EXISTS(SELECT 1 FROM daily_reports d WHERE d.current_run_id=r.run_id)
           OR (NOT EXISTS(SELECT 1 FROM daily_reports d WHERE d.date=r.target_date)
             AND COALESCE(r.error_code,'')!='RUN_SUPERSEDED'
             AND NOT EXISTS(SELECT 1 FROM report_runs n WHERE n.scope_id=r.scope_id
                 AND n.target_date=r.target_date AND n.run_kind='report'
                 AND (n.created_at>r.created_at OR (n.created_at=r.created_at AND n.rowid>r.rowid)))))
         ORDER BY r.target_date,r.run_id"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([scope], |r| {
        Ok(PersonalRecoveryCandidateRow {
            target_date: r.get(0)?,
            run_id: r.get(1)?,
            report_json: r.get(2)?,
            error_code: r.get(3)?,
            calls_budget_json: r.get(4)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(crate) fn personal_failure_summaries(
    conn: &Connection,
    scope: &str,
) -> Result<Vec<String>, ReportError> {
    let mut statement = conn.prepare(
        "SELECT value FROM report_metadata WHERE key LIKE ?1
         AND json_extract(value,'$.last_failure') IS NOT NULL
         AND NOT EXISTS(SELECT 1 FROM daily_reports d WHERE d.date=json_extract(value,'$.target_date'))",
    )?;
    let rows = statement.query_map([format!("{PERSONAL_SUMMARY_PREFIX}{scope}:%")], |r| {
        r.get(0)
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(crate) fn all_run_records(conn: &Connection) -> Result<Vec<RunRecord>, ReportError> {
    query(conn, "SELECT * FROM report_runs ORDER BY run_id", [])?
        .into_iter()
        .map(serde_json::from_value::<RunRecord>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(crate) fn current_referenced_run_ids(
    conn: &Connection,
    runs: &[RunRecord],
) -> Result<BTreeSet<String>, ReportError> {
    let sql = format!(
        "SELECT current_run_id FROM daily_reports WHERE current_run_id IS NOT NULL
         UNION SELECT value FROM report_metadata WHERE key LIKE '{CURRENT_PERIOD_PREFIX}%'"
    );
    let mut current = conn
        .prepare(&sql)?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    for run in runs {
        if let Some(id) = &run.previous_run_id {
            current.insert(id.clone());
        }
    }
    Ok(current)
}

pub(crate) fn all_metadata(conn: &Connection) -> Result<BTreeMap<String, String>, ReportError> {
    Ok(conn
        .prepare("SELECT key,value FROM report_metadata")?
        .query_map([], |row| row.try_into())?
        .collect::<Result<BTreeMap<String, String>, _>>()?)
}

pub(crate) fn open_job_call_run_ids(
    conn: &Connection,
    oldest_open_job: i64,
    retained_calls_json: &str,
) -> Result<Vec<String>, ReportError> {
    let mut statement = conn.prepare(
        "SELECT run_id FROM report_model_calls
         WHERE call_id>?1 OR call_id IN (SELECT value FROM json_each(?2))
         UNION SELECT c.run_id FROM report_model_calls c
         JOIN report_metadata m ON m.key='backfill_state:'||json_extract(c.usage_json,'$.job_id')
         WHERE NOT EXISTS(SELECT 1 FROM report_metadata done
           WHERE done.key='backfill_complete:'||json_extract(c.usage_json,'$.job_id'))",
    )?;
    let rows = statement.query_map(params![oldest_open_job, retained_calls_json], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(crate) fn clear_run_snapshot_and_calls(
    conn: &mut Connection,
    run_id: &str,
    delete_run: bool,
) -> Result<bool, ReportError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE report_runs SET snapshot_path=NULL,snapshot_sha256=NULL WHERE run_id=?",
        [run_id],
    )?;
    tx.execute(
        "UPDATE report_model_calls SET request_path=NULL,request_sha256=NULL,response_path=NULL,response_sha256=NULL WHERE run_id=?",
        [run_id],
    )?;
    if delete_run {
        tx.execute("DELETE FROM report_runs WHERE run_id=?", [run_id])?;
    }
    tx.commit()?;
    Ok(delete_run)
}

pub(crate) fn pending_personal_runs_with_unresolved_calls(
    conn: &Connection,
    scope_id: &str,
) -> Result<Vec<UnresolvedPersonalRunRow>, rusqlite::Error> {
    let sql = format!(
        "SELECT r.run_id, r.snapshot_path, r.snapshot_sha256
         FROM report_runs r
         WHERE r.scope_id=?1 AND r.run_kind='report'
           AND r.phase IN ('prepared','generating') AND r.status!='succeeded'
           AND coalesce(r.error_code,'')!='RUN_SUPERSEDED'
           AND coalesce(r.regeneration_reason,'') NOT LIKE 'coverage_publish:%'
           AND EXISTS (SELECT 1 FROM report_metadata m WHERE m.key='{PERSONAL_RUN_PREFIX}'||r.run_id AND m.value='5')
           AND NOT EXISTS (
               SELECT 1 FROM daily_reports d JOIN report_runs published ON published.run_id=d.current_run_id
               WHERE d.date=r.target_date AND published.target_date=r.target_date
                 AND published.scope_id=r.scope_id AND published.phase='published' AND published.status='succeeded'
           )
           AND EXISTS (
               SELECT 1 FROM report_model_calls c
               WHERE c.generation_id=r.generation_id AND c.status IN ('unknown','reserved')
                 AND NOT EXISTS (
                     SELECT 1 FROM report_model_calls s
                     WHERE s.generation_id=c.generation_id AND s.request_fingerprint=c.request_fingerprint
                       AND s.status='succeeded'
                 )
           )
         ORDER BY r.target_date, r.created_at, r.run_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([scope_id], |row| {
        Ok(UnresolvedPersonalRunRow {
            run_id: row.get(0)?,
            snapshot_path: row.get(1)?,
            snapshot_sha256: row.get(2)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
}
