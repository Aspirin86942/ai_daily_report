//! Outstanding personal reports stay visible until their current publication is usable.
use crate::{
    infra::json::read_frozen_report_json,
    processing_cache::RETENTION_DAYS,
    store::queries::{personal_failure_summaries, personal_recovery_candidates},
    types, ReportError,
};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::Connection;
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
    if !matches!(report["schema_version"].as_u64(), Some(5 | 6)) {
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
                    | "FILE_SIZE_LIMIT"
                    | "OFFICE_CONTENT_MISSING"
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
    let rows = personal_recovery_candidates(conn, scope)?;
    let mut result = Vec::new();
    for row in rows {
        let target_date = types::parse_date(&row.target_date)
            .map_err(|_| ReportError::new("PERSONAL_RECOVERY_INVALID", "历史日报日期无效"))?;
        let report = row
            .report_json
            .as_deref()
            .map(read_frozen_report_json)
            .transpose()?;
        let (reason_codes, missing_summary) = if let Some(report) = &report {
            (
                reasons(report),
                report["items"].as_array().is_some_and(Vec::is_empty)
                    && report["quality"] != "no_changes",
            )
        } else {
            (
                vec![row
                    .error_code
                    .unwrap_or_else(|| "REPORT_NOT_PUBLISHED".into())],
                true,
            )
        };
        if reason_codes.is_empty() {
            continue;
        }
        let calls_used = match row.calls_budget_json {
            Some(text) => serde_json::from_str::<Value>(&text)?["used"]
                .as_u64()
                .ok_or_else(|| {
                    ReportError::new("PERSONAL_CALL_RECORD_INVALID", "历史调用计数无效")
                })?,
            None => 0,
        };
        result.push(Issue {
            target_date,
            run_id: row.run_id,
            missing_summary,
            reason_codes,
            calls_used,
            material_window_open: target_date < today
                && target_date >= today - chrono::Duration::days(RETENTION_DAYS),
        });
    }
    // Detailed failed runs expire; their compact daily summary must retain the alert.
    for row in personal_failure_summaries(conn, scope)? {
        let summary: Value = serde_json::from_str(&row)?;
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
