//! Outstanding personal reports stay visible until their current publication is usable.
use crate::{config::AppConfig, processing_cache::RETENTION_DAYS, types, ReportError};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Serialize)]
pub struct Issue {
    pub target_date: NaiveDate,
    pub run_id: String,
    pub missing_summary: bool,
    pub reason_codes: Vec<String>,
    pub calls_used: u64,
    pub material_window_open: bool,
}

pub fn reasons(report: &Value) -> Vec<String> {
    if report["schema_version"] != 5 {
        return vec![];
    }
    let mut reasons = BTreeSet::new();
    for gap in report["gaps"].as_array().into_iter().flatten() {
        let code = gap["code"].as_str().unwrap_or("");
        if code.starts_with("MODEL_")
            || matches!(
                code,
                "PERSONAL_SUMMARY_UNAVAILABLE"
                    | "PERSONAL_CALL_LIMIT"
                    | "PARSE_UNAVAILABLE"
                    | "ARCHIVE_INPUT_PENDING"
                    | "MEMORY_RESERVE"
                    | "PROCESSING_CACHE_FULL"
                    | "PROCESSING_DEADLINE"
                    | "RUN_BUDGET_DEFERRED"
            )
        {
            reasons.insert(code.to_owned());
        }
    }
    if report["selection"]["omissions"]["read_failed"]
        .as_u64()
        .unwrap_or(0)
        > 0
    {
        reasons.insert("PARSE_UNAVAILABLE".into());
    }
    if report["items"].as_array().is_some_and(Vec::is_empty)
        && (report["selection"]["observed_files"].as_u64().unwrap_or(0) > 0
            || !report["user_notes"].as_str().unwrap_or("").is_empty())
    {
        reasons.insert("PERSONAL_SUMMARY_UNAVAILABLE".into());
    }
    reasons.into_iter().collect()
}

pub fn issues(
    conn: &Connection,
    scope: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Issue>, ReportError> {
    let today = now.with_timezone(&types::shanghai()).date_naive();
    let mut statement = conn.prepare(
        "SELECT r.target_date,r.run_id,r.report_json,r.error_code,
         (SELECT value FROM report_metadata WHERE key='personal_calls:'||r.scope_id||':'||r.target_date)
         FROM report_runs r WHERE r.scope_id=?1 AND r.run_kind='report'
         AND EXISTS(SELECT 1 FROM report_metadata m WHERE m.key='personal_run:'||r.run_id)
         AND (EXISTS(SELECT 1 FROM daily_reports d WHERE d.current_run_id=r.run_id)
           OR (NOT EXISTS(SELECT 1 FROM daily_reports d WHERE d.date=r.target_date)
             AND COALESCE(r.error_code,'')!='RUN_SUPERSEDED'
             AND NOT EXISTS(SELECT 1 FROM report_runs n WHERE n.scope_id=r.scope_id
                 AND n.target_date=r.target_date AND n.run_kind='report'
                 AND (n.created_at>r.created_at OR (n.created_at=r.created_at AND n.rowid>r.rowid)))))
         ORDER BY r.target_date,r.run_id",
    )?;
    let rows = statement.query_map([scope], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut result = Vec::new();
    for row in rows {
        let (date, run_id, report, error, budget) = row?;
        let target_date = types::parse_date(&date)
            .map_err(|_| ReportError::new("PERSONAL_RECOVERY_INVALID", "历史日报日期无效"))?;
        let report = report
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?;
        let (reason_codes, missing_summary) = if let Some(report) = &report {
            (
                reasons(report),
                report["items"].as_array().is_some_and(Vec::is_empty)
                    && report["quality"] != "no_changes",
            )
        } else {
            (
                vec![error.unwrap_or_else(|| "REPORT_NOT_PUBLISHED".into())],
                true,
            )
        };
        if reason_codes.is_empty() {
            continue;
        }
        let calls_used = match budget {
            Some(text) => serde_json::from_str::<Value>(&text)?["used"]
                .as_u64()
                .ok_or_else(|| {
                    ReportError::new("PERSONAL_CALL_RECORD_INVALID", "历史调用计数无效")
                })?,
            None => 0,
        };
        result.push(Issue {
            target_date,
            run_id,
            missing_summary,
            reason_codes,
            calls_used,
            material_window_open: target_date < today
                && target_date >= today - chrono::Duration::days(RETENTION_DAYS),
        });
    }
    // Detailed failed runs expire; their compact daily summary must retain the alert.
    let mut summaries = conn.prepare("SELECT value FROM report_metadata WHERE key LIKE ?1
        AND json_extract(value,'$.last_failure') IS NOT NULL
        AND NOT EXISTS(SELECT 1 FROM daily_reports d WHERE d.date=json_extract(value,'$.target_date'))")?;
    for row in summaries.query_map([format!("personal_summary:{scope}:%")], |r| {
        r.get::<_, String>(0)
    })? {
        let summary: Value = serde_json::from_str(&row?)?;
        let date = types::parse_date(summary["target_date"].as_str().unwrap_or(""))
            .map_err(|_| ReportError::new("PERSONAL_RECOVERY_INVALID", "历史日报摘要日期无效"))?;
        if result.iter().any(|issue| issue.target_date == date) {
            continue;
        }
        result.push(Issue {
            target_date: date,
            run_id: summary["last_failure"]["run_id"]
                .as_str()
                .unwrap_or("")
                .into(),
            missing_summary: true,
            reason_codes: vec![summary["last_failure"]["error_code"]
                .as_str()
                .unwrap_or("REPORT_NOT_PUBLISHED")
                .into()],
            calls_used: summary["calls"].as_u64().unwrap_or(0),
            material_window_open: false,
        });
    }
    result.sort_by_key(|issue| issue.target_date);
    Ok(result)
}

pub fn material_dates(
    config: &AppConfig,
    now: DateTime<Utc>,
) -> Result<Vec<NaiveDate>, ReportError> {
    let path = config.paths.db_dir.join("reports.sqlite3");
    if !path.is_file() {
        return Ok(vec![]);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(issues(&conn, &config.report.scope_id, now)?
        .into_iter()
        .filter(|issue| issue.material_window_open)
        .map(|issue| issue.target_date)
        .collect())
}

/// Recover an older date only after the currently due day has a publication.
pub fn scheduled_request(
    app: &crate::run::App,
    request: &mut types::ReportRequest,
) -> Result<(), ReportError> {
    if !request.scheduled
        || app
            .store
            .current(&request.context.target_date.to_string())?
            .is_none()
    {
        return Ok(());
    }
    for issue in issues(
        &app.store.connection()?,
        &app.config.report.scope_id,
        request.context.started_at,
    )? {
        if !issue.material_window_open {
            continue;
        }
        let run = app.store.get(&issue.run_id)?;
        request.input = if let Some(report) = run.report_json.as_deref() {
            serde_json::from_str::<Value>(report)?["user_notes"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        } else if let (Some(path), Some(sha256)) = (&run.snapshot_path, &run.snapshot_sha256) {
            let snapshots = crate::run::personal::snapshots(app)?;
            let value = match snapshots.read(&crate::snapshots::SnapshotRef {
                path: path.clone(),
                sha256: sha256.clone(),
            }) {
                Ok(value) => value,
                Err(error)
                    if matches!(error.code, "SNAPSHOT_MISSING" | "PERSONAL_INPUT_EXPIRED") =>
                {
                    continue
                }
                Err(error) => return Err(error),
            };
            let input: crate::run::personal::PersonalInput = serde_json::from_value(value)?;
            input.context.validate()?;
            input.selection.validate()?;
            if input.context.run_id != run.run_id || input.context.scope_id != run.scope_id {
                return Err(ReportError::new(
                    "SNAPSHOT_CONTEXT_MISMATCH",
                    "历史恢复输入身份不一致",
                ));
            }
            input
                .selection
                .materials
                .iter()
                .find(|m| m.source.source_id == "user_input")
                .map(|m| m.text.clone())
                .unwrap_or_default()
        } else {
            continue;
        };
        request.context = types::RunContext::new(
            request.context.run_id.clone(),
            request.context.started_at,
            issue.target_date,
            request.context.config_fingerprint.clone(),
        )?;
        request.period.start = issue.target_date;
        request.period.end = issue.target_date;
        request.period.label = issue.target_date.to_string();
        request.regeneration_reason = Some(format!("automatic_personal_recovery:{}", issue.run_id));
        return Ok(());
    }
    Ok(())
}
