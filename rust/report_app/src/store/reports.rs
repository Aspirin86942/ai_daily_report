use super::{changed_one, query, timestamp, RunRecord, Store};
use crate::{
    snapshots::digest,
    types::{parse_date, RunContext, RunPhase},
    ReportError,
};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use std::collections::BTreeMap;

pub struct StagedReport<'a> {
    pub report: &'a Value,
    pub markdown: &'a str,
    pub output_path: &'a str,
    pub publication_fingerprint: &'a str,
}

pub(super) fn read_run(conn: &Connection, id: &str) -> Result<RunRecord, ReportError> {
    let value = query(conn, "SELECT * FROM report_runs WHERE run_id=?", [id])?
        .into_iter()
        .next()
        .ok_or_else(|| ReportError::new("RUN_NOT_FOUND", "运行记录不存在"))?;
    serde_json::from_value(value).map_err(Into::into)
}

fn validate_report(context_json: &str, report: &Value, markdown: &str) -> Result<(), ReportError> {
    if markdown.is_empty() || markdown.contains('\r') || markdown.starts_with('\u{feff}') {
        return Err(ReportError::new(
            "REPORT_MARKDOWN_INVALID",
            "正文须为非空 UTF-8 文本、LF 换行且无 BOM",
        ));
    }
    let context: RunContext = serde_json::from_str(context_json)?;
    context.validate()?;
    let start = report["window_start_utc"]
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok());
    if report["date"].as_str() != Some(context.target_date.to_string().as_str())
        || start.map(|value| value.with_timezone(&Utc)) != Some(context.window_start_utc)
        || !matches!(report["schema_version"].as_u64(), Some(2..=5))
        || !matches!(
            report["quality"].as_str(),
            Some("complete" | "partial" | "no_changes")
        )
        || !report["sources"].is_array()
    {
        return Err(ReportError::new(
            "REPORT_CONTEXT_MISMATCH",
            "报告与冻结日期或格式不一致",
        ));
    }
    if report["schema_version"] == 5 {
        let personal: crate::reports::personal::PersonalReport =
            serde_json::from_value(report.clone())?;
        personal.validate()?;
        if markdown.len() > crate::reports::personal::MAX_REPORT_BYTES {
            return Err(ReportError::new(
                "PERSONAL_REPORT_TOO_LARGE",
                "日报与依据超过文件上限",
            ));
        }
    }
    Ok(())
}

impl Store {
    pub fn current(&self, date: &str) -> Result<Option<RunRecord>, ReportError> {
        query(&self.connection()?, "SELECT r.* FROM report_runs r JOIN daily_reports d ON d.current_run_id=r.run_id WHERE d.date=?", [date])?
            .into_iter().next().map(|value| serde_json::from_value(value).map_err(Into::into)).transpose()
    }

    pub fn stage_report(
        &self,
        id: &str,
        staged: &StagedReport<'_>,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = read_run(&tx, id)?;
        if run.run_kind != "report"
            || !matches!(run.phase, RunPhase::Prepared | RunPhase::Generating)
            || !matches!(run.status, super::RunStatus::Running)
        {
            return Err(ReportError::new("RUN_PHASE_INVALID", "报告尚未准备好提交"));
        }
        validate_report(&run.context_json, staged.report, staged.markdown)?;
        let previous: Option<(String, String)> = tx.query_row("SELECT r.run_id,r.expected_sha256 FROM daily_reports d JOIN report_runs r ON r.run_id=d.current_run_id WHERE d.date=?", [&run.target_date], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
        tx.execute("UPDATE report_runs SET phase='db_committed',quality=?,report_json=?,markdown=?,expected_sha256=?,previous_sha256=?,previous_run_id=?,output_path=?,publication_fingerprint=?,updated_at=? WHERE run_id=?",
            params![staged.report["quality"].as_str(),staged.report.to_string(),staged.markdown,digest(staged.markdown.as_bytes()),previous.as_ref().map(|value| &value.1),previous.as_ref().map(|value| &value.0),staged.output_path,staged.publication_fingerprint,timestamp(now),id])?;
        tx.execute("DELETE FROM report_run_sources WHERE run_id=?", [id])?;
        for source in staged.report["sources"].as_array().unwrap() {
            let source_id = source["source_id"]
                .as_str()
                .ok_or_else(|| ReportError::new("SOURCE_INVALID", "报告来源缺少身份"))?;
            tx.execute(
                "INSERT INTO report_run_sources(run_id,source_id,metadata_json) VALUES (?,?,?)",
                params![id, source_id, source.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_published(
        &self,
        id: &str,
        verified_sha256: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        if matches!(self.get(id)?.run_kind.as_str(), "weekly" | "monthly") {
            return self.mark_period_published(id, verified_sha256, now);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = read_run(&tx, id)?;
        if run.phase != RunPhase::DbCommitted
            || run.expected_sha256.as_deref() != Some(verified_sha256)
        {
            return Err(ReportError::new(
                "PUBLICATION_NOT_VERIFIED",
                "正文发布校验未通过",
            ));
        }
        let current: Option<String> = tx
            .query_row(
                "SELECT current_run_id FROM daily_reports WHERE date=?",
                [&run.target_date],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if current != run.previous_run_id {
            return Err(ReportError::new(
                "PUBLICATION_VERSION_CHANGED",
                "正式版本在发布期间改变",
            ));
        }
        let report: Value = serde_json::from_str(
            run.report_json
                .as_deref()
                .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "缺少已提交正文"))?,
        )?;
        let version = report["schema_version"]
            .as_i64()
            .ok_or_else(|| ReportError::new("REPORT_SCHEMA_UNSUPPORTED", "报告格式无效"))?;
        tx.execute("INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,schema_version,report_kind,current_run_id) VALUES (?,'','','',?,?,?,?) ON CONFLICT(date) DO UPDATE SET completed_work='',work_summary='',next_plan='',raw_json=excluded.raw_json,schema_version=excluded.schema_version,report_kind=excluded.report_kind,current_run_id=excluded.current_run_id,updated_at=?",
            params![run.target_date,run.report_json,version,report["report_kind"].as_str().unwrap_or("file_evidence"),id,timestamp(now)])?;
        tx.execute("UPDATE report_runs SET phase='published',status='succeeded',error_code=NULL,updated_at=?,finished_at=? WHERE run_id=?", params![timestamp(now),timestamp(now),id])?;
        if run.regeneration_reason.is_some() {
            tx.execute("UPDATE report_runs SET status='cancelled',error_code='RUN_SUPERSEDED',updated_at=?,finished_at=? WHERE run_kind='report' AND scope_id=? AND target_date=? AND phase IN ('prepared','generating','db_committed') AND status!='succeeded' AND run_id!=? AND created_at<=?", params![timestamp(now),timestamp(now),run.scope_id,run.target_date,id,run.created_at])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn finish_preview(
        &self,
        id: &str,
        report: &Value,
        markdown: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = read_run(&tx, id)?;
        if run.run_kind != "preview" {
            return Err(ReportError::new(
                "RUN_PHASE_INVALID",
                "当前运行不是报告预览",
            ));
        }
        validate_report(&run.context_json, report, markdown)?;
        let count = tx.execute("UPDATE report_runs SET phase='preview',status='succeeded',quality=?,report_json=?,markdown=?,expected_sha256=?,updated_at=?,finished_at=? WHERE run_id=? AND status='running'", params![report["quality"].as_str(),report.to_string(),markdown,digest(markdown.as_bytes()),timestamp(now),timestamp(now),id])?;
        changed_one(count, "RUN_PHASE_INVALID")?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish_observation(
        &self,
        id: &str,
        current: &RunRecord,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        changed_one(self.connection()?.execute("UPDATE report_runs SET status='succeeded',quality=?,previous_run_id=?,updated_at=?,finished_at=? WHERE run_id=? AND phase='prepared' AND status='running'", params![current.quality,current.run_id,timestamp(now),timestamp(now),id])?, "RUN_PHASE_INVALID")
    }

    pub fn finish_scan_preview(
        &self,
        id: &str,
        quality: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        changed_one(self.connection()?.execute("UPDATE report_runs SET status='succeeded',quality=?,updated_at=?,finished_at=? WHERE run_id=? AND run_kind='preview' AND phase='prepared' AND status='running'",params![quality,timestamp(now),timestamp(now),id])?,"RUN_PHASE_INVALID")
    }

    pub fn finish_model_check(
        &self,
        id: &str,
        result: &Value,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        changed_one(self.connection()?.execute("UPDATE report_runs SET phase='preview',status='succeeded',quality='complete',report_json=?,updated_at=?,finished_at=? WHERE run_id=? AND run_kind='model_check' AND phase IN ('prepared','generating') AND status='running'",params![result.to_string(),timestamp(now),timestamp(now),id])?,"RUN_PHASE_INVALID")
    }

    pub fn activate(&self, first: NaiveDate) -> Result<(), ReportError> {
        self.connection()?.execute(
            "INSERT OR IGNORE INTO report_metadata(key,value) VALUES ('first_target_date',?)",
            [first.to_string()],
        )?;
        Ok(())
    }

    pub fn missed_dates(&self, target: NaiveDate) -> Result<Vec<String>, ReportError> {
        let conn = self.connection()?;
        let first: Option<String> = conn
            .query_row(
                "SELECT value FROM report_metadata WHERE key='first_target_date'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(first) = first else {
            return Ok(vec![]);
        };
        let first = parse_date(&first)?;
        if first >= target {
            return Ok(vec![]);
        }
        Ok(self.reports_in_range(first, target - Duration::days(1))?.1)
    }

    pub fn save_scan_failure(
        &self,
        id: &str,
        snapshot: &crate::snapshots::SnapshotRef,
        sources: &[Value],
    ) -> Result<(), ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        changed_one(tx.execute("UPDATE report_runs SET snapshot_path=?,snapshot_sha256=?,quality='unavailable' WHERE run_id=? AND phase='scanning'",params![snapshot.path,snapshot.sha256,id])?,"RUN_PHASE_INVALID")?;
        for source in sources {
            tx.execute(
                "INSERT INTO report_run_sources(run_id,source_id,metadata_json) VALUES (?,?,?)",
                params![
                    id,
                    source["source_id"]
                        .as_str()
                        .ok_or_else(|| ReportError::new("SOURCE_INVALID", "缺少来源身份"))?,
                    source.to_string()
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_reports(&self) -> Result<Vec<String>, ReportError> {
        Ok(query(
            &self.connection()?,
            "SELECT date FROM daily_reports ORDER BY date",
            [],
        )?
        .into_iter()
        .map(|row| row["date"].as_str().unwrap().to_owned())
        .collect())
    }

    pub fn reports_in_range(
        &self,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<(Vec<Value>, Vec<String>), ReportError> {
        let conn = self.connection()?;
        let rows = query(
            &conn,
            "SELECT date,raw_json FROM daily_reports WHERE date>=? AND date<=? ORDER BY date",
            params![start.to_string(), end.to_string()],
        )?;
        let activation: Option<String> = conn
            .query_row(
                "SELECT value FROM report_metadata WHERE key='first_target_date'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let activation = activation.as_deref().map(parse_date).transpose()?;
        let mut reports = BTreeMap::new();
        for row in rows {
            let data: Value = serde_json::from_str(row["raw_json"].as_str().unwrap())?;
            let version = data["schema_version"].as_u64().unwrap_or(1);
            if !(1..=5).contains(&version) {
                return Err(ReportError::new(
                    "REPORT_SCHEMA_UNSUPPORTED",
                    "历史报告格式不支持",
                ));
            }
            if data["date"] != row["date"] {
                return Err(ReportError::new(
                    "REPORT_CONTEXT_MISMATCH",
                    "历史报告日期与数据库索引不一致",
                ));
            }
            if version == 5 {
                serde_json::from_value::<crate::reports::personal::PersonalReport>(data.clone())?
                    .validate()?;
            }
            reports.insert(parse_date(row["date"].as_str().unwrap())?, data);
        }
        let mut missing = Vec::new();
        let mut day = start;
        while day <= end {
            if !reports.contains_key(&day)
                && activation
                    .map(|first| day >= first)
                    .unwrap_or_else(|| day.weekday().number_from_monday() <= 5)
            {
                missing.push(day.to_string());
            }
            day += Duration::days(1);
        }
        Ok((reports.into_values().collect(), missing))
    }
}
