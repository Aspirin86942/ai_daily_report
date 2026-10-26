use super::{changed_one, query, timestamp, Store};
use crate::{snapshots::SnapshotRef, types::shanghai, ReportError};
use chrono::{DateTime, Utc};
use rusqlite::{params, TransactionBehavior};
use serde_json::{json, Value};

pub struct CallReservation<'a> {
    pub run_id: &'a str,
    pub fingerprint: &'a str,
    pub logical_step: &'a str,
    pub request: &'a SnapshotRef,
    pub job_id: Option<&'a str>,
    pub audit: &'a Value,
    pub max_attempts: u32,
    pub now: DateTime<Utc>,
}

pub struct CallCompletion<'a> {
    pub call_id: i64,
    pub status: &'a str,
    pub response: Option<&'a SnapshotRef>,
    pub usage: Value,
    pub error_code: Option<&'a str>,
    pub duration_ms: u64,
    pub now: DateTime<Utc>,
}

impl Store {
    pub fn generation_calls(
        &self,
        generation: &str,
        step: &str,
    ) -> Result<Vec<Value>, ReportError> {
        query(&self.connection()?, "SELECT * FROM report_model_calls WHERE generation_id=? AND logical_step=? ORDER BY call_id", params![generation, step])
    }

    pub fn reserve_call(&self, input: CallReservation<'_>) -> Result<i64, ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = query(&tx, "SELECT * FROM report_runs WHERE run_id=? AND status='running' AND phase IN ('prepared','generating')", [input.run_id])?.into_iter().next()
            .ok_or_else(|| ReportError::new("RUN_NOT_GENERATING", "运行尚未准备好模型输入"))?;
        let generation = run["generation_id"].as_str().unwrap();
        if let Some(job) = input.job_id {
            if job.is_empty()
                || job.len() > 128
                || !job
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(ReportError::new(
                    "MODEL_JOB_INVALID",
                    "模型调用作业身份无效",
                ));
            }
            let closed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM report_metadata WHERE key=?)",
                [format!("backfill_complete:{job}")],
                |r| r.get(0),
            )?;
            if closed {
                return Err(ReportError::new(
                    "BACKFILL_ALREADY_COMPLETE",
                    "已完成的补跑作业不能新增模型调用",
                ));
            }
        }
        let attempt: i64 = tx.query_row(
            "SELECT COUNT(*) FROM report_model_calls WHERE generation_id=? AND logical_step=?",
            params![generation, input.logical_step],
            |row| row.get(0),
        )?;
        if !(1..=3).contains(&input.max_attempts) || attempt >= i64::from(input.max_attempts) {
            return Err(ReportError::new(
                "MODEL_STEP_ATTEMPTS_EXHAUSTED",
                "当前步骤已达到总尝试上限",
            ));
        }
        let day = input
            .now
            .with_timezone(&shanghai())
            .date_naive()
            .to_string();
        let usage = json!({"job_id":input.job_id,"request_audit":input.audit});
        super::personal::record_call(&tx, &run)?;
        tx.execute("INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,logical_step,attempt,call_date,status,request_path,request_sha256,usage_json,started_at) VALUES (?,?,?,?,?,?,'reserved',?,?,?,?)",
            params![input.run_id,generation,input.fingerprint,input.logical_step,attempt+1,day,input.request.path,input.request.sha256,usage.to_string(),timestamp(input.now)])?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE report_runs SET phase='generating',updated_at=? WHERE run_id=?",
            params![timestamp(input.now), input.run_id],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn finish_call(&self, input: CallCompletion<'_>) -> Result<(), ReportError> {
        if !matches!(input.status, "succeeded" | "failed" | "unknown")
            || (input.status == "succeeded" && input.response.is_none())
        {
            return Err(ReportError::new(
                "MODEL_CALL_STATE_INVALID",
                "模型调用状态无效",
            ));
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = query(
            &tx,
            "SELECT usage_json FROM report_model_calls WHERE call_id=? AND status='reserved'",
            [input.call_id],
        )?
        .into_iter()
        .next()
        .ok_or_else(|| ReportError::new("MODEL_CALL_STATE_INVALID", "模型调用已结束或不存在"))?;
        let old: Value = serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
        let mut usage = input.usage.as_object().cloned().unwrap_or_default();
        for name in [
            "job_id",
            "request_audit",
            "reserved_budget",
            "budget_limits_enforced",
        ] {
            usage.remove(name);
            if let Some(value) = old.get(name) {
                usage.insert(name.to_owned(), value.clone());
            }
        }
        changed_one(tx.execute("UPDATE report_model_calls SET status=?,response_path=?,response_sha256=?,usage_json=?,error_code=?,finished_at=?,duration_ms=? WHERE call_id=? AND status='reserved'",
            params![input.status,input.response.map(|r| &r.path),input.response.map(|r| &r.sha256),Value::Object(usage).to_string(),input.error_code,timestamp(input.now),i64::try_from(input.duration_ms).unwrap_or(i64::MAX),input.call_id])?, "MODEL_CALL_STATE_INVALID")?;
        tx.commit()?;
        Ok(())
    }
}
