//! Per-target-date counters outlive runs and detailed request retention.
use super::Store;
use crate::ReportError;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

impl Store {
    /// Called with the report lock held. Final report bytes and date counters are
    /// permanent; personal processing references and repeat-run detail are bounded.
    pub fn compact_personal(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        root: &std::path::Path,
    ) -> Result<(), ReportError> {
        self.compact_personal_until(
            now,
            root,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
        )
    }

    pub fn compact_personal_until(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        root: &std::path::Path,
        deadline: std::time::Instant,
    ) -> Result<(), ReportError> {
        let mut conn = self.connection()?;
        conn.busy_timeout(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(std::time::Duration::from_secs(2)),
        )?;
        let mut detail_bytes: i64 = conn.query_row(
            "SELECT
             (SELECT COALESCE(sum(length(CAST(c.usage_json AS BLOB))+1024),0) FROM report_model_calls c
              JOIN report_metadata m ON m.key='personal_run:'||c.run_id WHERE m.value='5') +
             (SELECT COALESCE(sum(length(CAST(s.metadata_json AS BLOB))),0) FROM report_run_sources s
              JOIN report_metadata m ON m.key='personal_run:'||s.run_id WHERE m.value='5') +
             (SELECT COALESCE(sum(COALESCE(length(CAST(r.report_json AS BLOB)),0)+COALESCE(length(CAST(r.markdown AS BLOB)),0)+2048),0)
              FROM report_runs r JOIN report_metadata m ON m.key='personal_run:'||r.run_id
              WHERE m.value='5' AND r.run_id NOT IN(SELECT current_run_id FROM daily_reports WHERE current_run_id IS NOT NULL))",
            [], |r| r.get(0))?;
        let cutoff = (now - chrono::Duration::days(30)).to_rfc3339();
        let input_cutoff = (now - chrono::Duration::days(7)).to_rfc3339();
        let mut cursor = (String::new(), String::new());
        loop {
            if std::time::Instant::now() >= deadline {
                break;
            }
            let ids = {
                let mut statement = conn.prepare(
                    "SELECT r.run_id,r.created_at FROM report_runs r JOIN report_metadata m ON m.key='personal_run:'||r.run_id
                     WHERE m.value='5' AND (r.created_at,r.run_id)>(?1,?2)
                     AND (NOT EXISTS(SELECT 1 FROM report_metadata x WHERE x.key='personal_compacted:'||r.run_id)
                          OR r.run_id NOT IN(SELECT current_run_id FROM daily_reports WHERE current_run_id IS NOT NULL))
                     ORDER BY r.created_at,r.run_id LIMIT 128")?;
                let rows = statement
                    .query_map(params![cursor.0, cursor.1], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            if ids.is_empty() {
                break;
            }
            for (id, created) in ids {
                if std::time::Instant::now() >= deadline {
                    return Ok(());
                }
                cursor = (created, id.clone());
                let mut run = self.get(&id)?;
                let referenced = run
                    .snapshot_path
                    .as_ref()
                    .is_some_and(|p| root.join(p).is_file());
                if run.created_at < input_cutoff || run.snapshot_path.is_some() && !referenced {
                    conn.execute("UPDATE report_runs SET snapshot_path=NULL,snapshot_sha256=NULL WHERE run_id=?", [&id])?;
                    conn.execute("UPDATE report_model_calls SET request_path=NULL,request_sha256=NULL,response_path=NULL,response_sha256=NULL WHERE run_id=?", [&id])?;
                    if matches!(run.status, super::RunStatus::Running)
                        && !matches!(
                            run.phase,
                            crate::types::RunPhase::Published
                                | crate::types::RunPhase::Preview
                                | crate::types::RunPhase::DbCommitted
                        )
                    {
                        conn.execute("UPDATE report_runs SET status='failed',error_code='PERSONAL_INPUT_EXPIRED',finished_at=? WHERE run_id=?",params![now.to_rfc3339(),id])?;
                        run = self.get(&id)?;
                    }
                }
                if run.created_at >= cutoff && detail_bytes <= 8 * 1024 * 1024 {
                    continue;
                }
                if matches!(run.status, super::RunStatus::Running)
                    || run.phase == crate::types::RunPhase::DbCommitted
                {
                    continue;
                }
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let current: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM daily_reports WHERE current_run_id=?)",
                    [&id],
                    |r| r.get(0),
                )?;
                let compacted: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM report_metadata WHERE key=?)",
                    [format!("personal_compacted:{id}")],
                    |r| r.get(0),
                )?;
                let key = format!("personal_summary:{}:{}", run.scope_id, run.target_date);
                let calls = used(
                    &tx,
                    &format!("personal_calls:{}:{}", run.scope_id, run.target_date),
                )?;
                let prior: Option<String> = tx
                    .query_row(
                        "SELECT value FROM report_metadata WHERE key=?",
                        [&key],
                        |r| r.get(0),
                    )
                    .optional()?;
                let mut summary = prior
                    .map(|v| serde_json::from_str::<Value>(&v))
                    .transpose()?
                    .unwrap_or_else(|| json!({}));
                summary["calls"] = json!(calls);
                summary["target_date"] = json!(run.target_date);
                // This is the persisted run lifetime, including time between resumptions.
                let elapsed_ms = run.finished_at.as_deref().and_then(|finished| {
                    let start = chrono::DateTime::parse_from_rfc3339(&run.created_at).ok()?;
                    let end = chrono::DateTime::parse_from_rfc3339(finished).ok()?;
                    u64::try_from((end - start).num_milliseconds()).ok()
                });
                if !compacted {
                    summary["runs"] = json!(summary["runs"].as_u64().unwrap_or(0) + 1);
                    summary["elapsed_ms_total"] = json!(summary["elapsed_ms_total"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_add(elapsed_ms.unwrap_or(0)));
                }
                if !compacted && run.error_code.is_some() {
                    summary["failures"] = json!(summary["failures"].as_u64().unwrap_or(0) + 1);
                    if summary["last_failure_at"]
                        .as_str()
                        .is_none_or(|at| run.created_at.as_str() >= at)
                    {
                        summary["last_failure_at"] = json!(run.created_at);
                        summary["last_failure"] = json!({"run_id":id,"error_code":run.error_code});
                    }
                }
                if summary["last_created_at"]
                    .as_str()
                    .is_none_or(|at| run.created_at.as_str() >= at)
                {
                    summary["last_created_at"] = json!(run.created_at);
                    summary["last_run"] = json!({"run_id":id,"status":run.status,"quality":run.quality,"finished_at":run.finished_at,"elapsed_ms":elapsed_ms});
                }
                if current {
                    summary["published"] = json!({"run_id":id,"quality":run.quality,"finished_at":run.finished_at,"elapsed_ms":elapsed_ms});
                }
                tx.execute("INSERT INTO report_metadata(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key,summary.to_string()])?;
                let removed: i64 = tx.query_row(
                "SELECT (SELECT COALESCE(sum(length(CAST(usage_json AS BLOB))+1024),0) FROM report_model_calls WHERE run_id=?1)
                 +(SELECT COALESCE(sum(length(CAST(metadata_json AS BLOB))),0) FROM report_run_sources WHERE run_id=?1)
                 +CASE WHEN ?2 THEN 0 ELSE (SELECT COALESCE(length(CAST(report_json AS BLOB)),0)+COALESCE(length(CAST(markdown AS BLOB)),0)+2048 FROM report_runs WHERE run_id=?1) END",
                params![id,current], |r|r.get(0))?;
                tx.execute("DELETE FROM report_model_calls WHERE run_id=?", [&id])?;
                tx.execute("DELETE FROM report_run_sources WHERE run_id=?", [&id])?;
                if !current {
                    tx.execute("DELETE FROM report_runs WHERE run_id=?", [&id])?;
                    tx.execute(
                        "DELETE FROM report_metadata WHERE key=?",
                        [format!("personal_run:{id}")],
                    )?;
                    tx.execute(
                        "DELETE FROM report_metadata WHERE key=?",
                        [format!("personal_compacted:{id}")],
                    )?;
                } else {
                    tx.execute("UPDATE report_runs SET snapshot_path=NULL,snapshot_sha256=NULL WHERE run_id=?", [&id])?;
                    tx.execute(
                        "INSERT OR IGNORE INTO report_metadata(key,value) VALUES(?,'1')",
                        [format!("personal_compacted:{id}")],
                    )?;
                }
                tx.commit()?;
                detail_bytes = detail_bytes.saturating_sub(removed);
            }
        }
        Ok(())
    }
    pub fn register_personal_run(&self, id: &str) -> Result<(), ReportError> {
        self.get(id)?;
        self.connection()?.execute(
            "INSERT OR IGNORE INTO report_metadata(key,value) VALUES(?,'5')",
            [format!("personal_run:{id}")],
        )?;
        Ok(())
    }
    pub fn is_personal_run(&self, id: &str) -> Result<bool, ReportError> {
        Ok(self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM report_metadata WHERE key=? AND value='5')",
            [format!("personal_run:{id}")],
            |r| r.get(0),
        )?)
    }
    pub fn personal_calls(&self, scope: &str, date: &str) -> Result<u64, ReportError> {
        used(
            &self.connection()?,
            &format!("personal_calls:{scope}:{date}"),
        )
    }
}

fn used(conn: &Connection, key: &str) -> Result<u64, ReportError> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM report_metadata WHERE key=?",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    match value {
        None => Ok(0),
        Some(value) => serde_json::from_str::<Value>(&value)?["used"]
            .as_u64()
            .ok_or_else(|| ReportError::new("PERSONAL_BUDGET_INVALID", "已用额度记录无效")),
    }
}

pub(super) fn reserve(conn: &Connection, run: &Value) -> Result<(), ReportError> {
    let personal: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM report_metadata WHERE key=? AND value='5')",
        [format!(
            "personal_run:{}",
            run["run_id"].as_str().unwrap_or("")
        )],
        |r| r.get(0),
    )?;
    if !personal {
        return Ok(());
    }
    let key = format!(
        "personal_calls:{}:{}",
        run["scope_id"].as_str().unwrap_or(""),
        run["target_date"].as_str().unwrap_or("")
    );
    let count = used(conn, &key)?;
    if count >= 3 {
        return Err(ReportError::new(
            "PERSONAL_CALL_LIMIT",
            "目标日期的 3 次请求额度已用完",
        ));
    }
    conn.execute(
        "INSERT INTO report_metadata(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, json!({"used":count+1,"last_run_id":run["run_id"]}).to_string()],
    )?;
    Ok(())
}
