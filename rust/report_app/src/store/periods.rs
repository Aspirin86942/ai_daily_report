use super::{query, reports::read_run, timestamp, RunRecord, RunStatus, StagedReport, Store};
use crate::{reports::period, snapshots::digest, types::RunPhase, ReportError};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;

fn table(kind: &str) -> Result<(&'static str, &'static str), ReportError> {
    match kind {
        "weekly" => Ok(("weekly_reports", "week_label")),
        "monthly" => Ok(("monthly_reports", "year_month")),
        _ => Err(ReportError::new("RUN_KIND_INVALID", "周月报类型无效")),
    }
}
fn key(kind: &str, label: &str) -> String {
    format!("current_period:{kind}:{label}")
}
fn current(conn: &Connection, kind: &str, label: &str) -> Result<Option<RunRecord>, ReportError> {
    let (table, column) = table(kind)?;
    let id: Option<String> = conn
        .query_row(
            "SELECT value FROM report_metadata WHERE key=?",
            [key(kind, label)],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else { return Ok(None) };
    let run = read_run(conn, &id)?;
    let raw: Option<String> = conn
        .query_row(
            &format!("SELECT raw_json FROM {table} WHERE {column}=?"),
            [label],
            |row| row.get(0),
        )
        .optional()?;
    if run.run_kind != kind || run.phase != RunPhase::Published || run.report_json != raw {
        return Err(ReportError::new(
            "PUBLICATION_VERSION_CHANGED",
            "周月报数据库正文与正式版本不一致",
        ));
    }
    Ok(Some(run))
}

impl Store {
    pub fn current_period(
        &self,
        kind: &str,
        label: &str,
    ) -> Result<Option<RunRecord>, ReportError> {
        current(&self.connection()?, kind, label)
    }
    pub fn publication_current(&self, run: &RunRecord) -> Result<Option<RunRecord>, ReportError> {
        if matches!(run.run_kind.as_str(), "weekly" | "monthly") {
            let report: Value = serde_json::from_str(
                run.report_json
                    .as_deref()
                    .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "周月报正文缺失"))?,
            )?;
            self.current_period(
                &run.run_kind,
                &period::validate_identity(&run.run_kind, &report)?,
            )
        } else {
            self.current(&run.target_date)
        }
    }
    pub fn pending_period(&self, kind: &str, target: &str) -> Result<Vec<RunRecord>, ReportError> {
        table(kind)?;
        query(&self.connection()?,"SELECT * FROM report_runs WHERE run_kind=? AND target_date=? AND phase IN ('prepared','generating','db_committed') AND status!='succeeded' AND COALESCE(error_code,'')!='RUN_SUPERSEDED' ORDER BY CASE WHEN phase='db_committed' THEN 0 ELSE 1 END,created_at,run_id",params![kind,target])?.into_iter().map(|v|serde_json::from_value(v).map_err(Into::into)).collect()
    }
    pub fn stage_period(
        &self,
        id: &str,
        staged: &StagedReport<'_>,
        quality: &str,
        save: bool,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = read_run(&tx, id)?;
        let label = period::validate_identity(&run.run_kind, staged.report)?;
        if !matches!(run.phase, RunPhase::Prepared | RunPhase::Generating)
            || !matches!(run.status, RunStatus::Running)
            || staged.markdown.is_empty()
            || !matches!(quality, "complete" | "partial")
        {
            return Err(ReportError::new(
                "RUN_PHASE_INVALID",
                "周月报尚未准备好提交",
            ));
        }
        let previous = current(&tx, &run.run_kind, &label)?;
        tx.execute("UPDATE report_runs SET phase=?,status=?,quality=?,report_json=?,markdown=?,expected_sha256=?,previous_sha256=?,previous_run_id=?,output_path=?,publication_fingerprint=?,updated_at=?,finished_at=? WHERE run_id=?",params![if save{"db_committed"}else{"preview"},if save{"running"}else{"succeeded"},quality,staged.report.to_string(),staged.markdown,digest(staged.markdown.as_bytes()),previous.as_ref().and_then(|r|r.expected_sha256.as_ref()),previous.as_ref().map(|r|&r.run_id),if save{Some(staged.output_path)}else{None},staged.publication_fingerprint,timestamp(now),if save{None}else{Some(timestamp(now))},id])?;
        tx.commit()?;
        Ok(())
    }
    pub(super) fn mark_period_published(
        &self,
        id: &str,
        verified: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = read_run(&tx, id)?;
        if run.phase != RunPhase::DbCommitted || run.expected_sha256.as_deref() != Some(verified) {
            return Err(ReportError::new(
                "PUBLICATION_NOT_VERIFIED",
                "周月报正文未通过发布校验",
            ));
        }
        let report: Value = serde_json::from_str(
            run.report_json
                .as_deref()
                .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "周月报正文缺失"))?,
        )?;
        let label = period::validate_identity(&run.run_kind, &report)?;
        if current(&tx, &run.run_kind, &label)?.map(|r| r.run_id) != run.previous_run_id {
            return Err(ReportError::new(
                "PUBLICATION_VERSION_CHANGED",
                "正式周月报版本已经改变",
            ));
        }
        let (table, column) = table(&run.run_kind)?;
        let fields = period::fields(if run.run_kind == "weekly" {
            crate::model::plan::StepKind::Weekly
        } else {
            crate::model::plan::StepKind::Monthly
        });
        let mut columns = fields.join(",");
        columns.push_str(",raw_json,updated_at");
        let marks = vec!["?"; fields.len() + 2].join(",");
        let updates = columns
            .split(',')
            .filter(|name| *name != column)
            .map(|name| format!("{name}=excluded.{name}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut values: Vec<String> = fields
            .iter()
            .map(|name| report[*name].as_str().unwrap().to_owned())
            .collect();
        values.push(run.report_json.unwrap());
        values.push(timestamp(now));
        tx.execute(&format!("INSERT INTO {table}({columns}) VALUES ({marks}) ON CONFLICT({column}) DO UPDATE SET {updates}"),rusqlite::params_from_iter(values))?;
        tx.execute("INSERT INTO report_metadata(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key(&run.run_kind,&label),id])?;
        tx.execute("UPDATE report_runs SET phase='published',status='succeeded',error_code=NULL,updated_at=?,finished_at=? WHERE run_id=?",params![timestamp(now),timestamp(now),id])?;
        tx.commit()?;
        Ok(())
    }
}
