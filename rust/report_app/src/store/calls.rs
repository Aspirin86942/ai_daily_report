use super::{changed_one, query, timestamp, Store};
use crate::{
    config::Recovery,
    model::budget::{charged, usage_tokens, Reservation},
    snapshots::SnapshotRef,
    types::shanghai,
    ReportError,
};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

pub struct CallReservation<'a> {
    pub run_id: &'a str,
    pub fingerprint: &'a str,
    pub logical_step: &'a str,
    pub request: &'a SnapshotRef,
    pub budget: &'a Reservation,
    pub recovery: &'a Recovery,
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
        input.budget.validate()?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = query(&tx, "SELECT * FROM report_runs WHERE run_id=? AND status='running' AND phase IN ('prepared','generating')", [input.run_id])?.into_iter().next()
            .ok_or_else(|| ReportError::new("RUN_NOT_GENERATING", "运行尚未准备好模型输入"))?;
        let generation = run["generation_id"].as_str().unwrap();
        if let Some(job) = &input.budget.job_id {
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
            let value: Option<String> = tx
                .query_row(
                    "SELECT value FROM report_metadata WHERE key=?",
                    [format!("backfill_budget:{job}")],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(value) = value {
                let value: Value = serde_json::from_str(&value)?;
                let baseline = value["baseline_call_id"]
                    .as_i64()
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| {
                        ReportError::new("BACKFILL_BUDGET_INVALID", "补跑消耗起点无效")
                    })?;
                let limit = value["calls_limit"]
                    .as_u64()
                    .ok_or_else(|| ReportError::new("BACKFILL_BUDGET_INVALID", "补跑预算无效"))?;
                let used = tx.query_row(
                    "SELECT COUNT(*) FROM report_model_calls WHERE call_id>?",
                    [baseline],
                    |r| r.get::<_, i64>(0),
                )? as u64;
                if input.budget.job_calls_limit != Some(limit) || used >= limit {
                    return Err(ReportError::new(
                        "BACKFILL_CALL_BUDGET_EXHAUSTED",
                        "补跑调用预算已用完，既有记录均计入消耗",
                    ));
                }
            }
        }
        let attempt: i64 = tx.query_row(
            "SELECT COUNT(*) FROM report_model_calls WHERE generation_id=? AND logical_step=?",
            params![generation, input.logical_step],
            |row| row.get(0),
        )?;
        if !(1..=3).contains(&input.max_attempts) || attempt >= i64::from(input.max_attempts) {
            return Err(ReportError::new(
                "MODEL_STEP_BUDGET_EXHAUSTED",
                "当前步骤已达到总尝试上限",
            ));
        }
        let day = input
            .now
            .with_timezone(&shanghai())
            .date_naive()
            .to_string();
        if input.recovery.enforce_model_budgets || input.budget.job_id.is_some() {
            let chain = input
                .budget
                .job_id
                .as_deref()
                .map(|id| super::revisions::read_chain(&tx, id))
                .transpose()?
                .unwrap_or_default();
            if !super::revisions::current_approved(input.budget, &chain)? {
                return Err(ReportError::new(
                    "MODEL_JOB_BUDGET_CHANGED",
                    "当前作业预算与批准的修订不一致",
                ));
            }
            let rows = query(&tx, "SELECT * FROM report_model_calls WHERE run_id=? OR call_date=? OR (? IS NOT NULL AND json_extract(usage_json,'$.reserved_budget.job_id')=?)", params![input.run_id,day,input.budget.job_id,input.budget.job_id])?;
            check_budgets(&rows, &input, &day, &chain)?;
        }
        let mut usage = json!({"reserved_budget": input.budget});
        if !input.recovery.enforce_model_budgets {
            usage["budget_limits_enforced"] = json!(false);
        }
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
        usage.remove("reserved_budget");
        usage.remove("budget_limits_enforced");
        for name in ["reserved_budget", "budget_limits_enforced"] {
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

fn check_budgets(
    rows: &[Value],
    input: &CallReservation<'_>,
    day: &str,
    chain: &super::revisions::RevisionChain,
) -> Result<(), ReportError> {
    let b = input.budget;
    let mut totals = [(0u64, 0u64, Some(0u64)); 3];
    for row in rows {
        let usage: Value = serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
        let status = row["status"].as_str().unwrap_or("unknown");
        let (tokens, money) = if let Some(stored) = usage.get("reserved_budget") {
            let prior: Reservation = serde_json::from_value(stored.clone())
                .map_err(|_| ReportError::new("MODEL_BUDGET_INVALID", "既有调用预算格式无效"))?;
            prior.validate()?;
            if b.job_id.is_some()
                && b.job_id == prior.job_id
                && !super::revisions::reservation_approved(
                    &prior,
                    row["call_id"].as_i64().unwrap(),
                    chain,
                    b,
                )?
            {
                return Err(ReportError::new(
                    "MODEL_JOB_BUDGET_CHANGED",
                    "作业预算与既有调用不一致",
                ));
            }
            let (tokens, money) = charged(status, &prior, &usage);
            let same_currency = prior.pricing.as_ref().map(|p| &p.currency)
                == b.pricing.as_ref().map(|p| &p.currency);
            (tokens, money.filter(|_| same_currency))
        } else {
            let total = usage_tokens(&usage).2;
            (
                if matches!(status, "failed" | "succeeded") {
                    total.unwrap_or(b.legacy_tokens_upper)
                } else {
                    b.legacy_tokens_upper
                },
                None,
            )
        };
        let scopes = [
            row["run_id"] == input.run_id,
            row["call_date"] == day,
            b.job_id
                .as_ref()
                .is_some_and(|id| usage["reserved_budget"]["job_id"] == *id),
        ];
        for (index, included) in scopes.into_iter().enumerate() {
            if included {
                totals[index].0 = totals[index].0.saturating_add(1);
                totals[index].1 = totals[index].1.saturating_add(tokens);
                totals[index].2 = totals[index].2.zip(money).map(|(a, b)| a.saturating_add(b));
            }
        }
    }
    let limits = [
        (
            Some(input.recovery.max_calls_per_run),
            Some(b.run_tokens_limit),
            b.run_cost_limit_microunits,
        ),
        (
            Some(input.recovery.max_calls_per_calendar_day),
            Some(b.day_tokens_limit),
            b.day_cost_limit_microunits,
        ),
        (
            b.job_calls_limit,
            b.job_tokens_limit,
            b.job_cost_limit_microunits,
        ),
    ];
    for (scope, ((calls, tokens, money), (call_limit, token_limit, money_limit))) in
        totals.into_iter().zip(limits).enumerate()
    {
        // Explicit job budgets are enforced even when optional daily/run limits are disabled.
        if scope < 2 && !input.recovery.enforce_model_budgets {
            continue;
        }
        if call_limit.is_some_and(|limit| calls.saturating_add(1) > limit) {
            return Err(ReportError::new(
                "MODEL_CALL_BUDGET_EXHAUSTED",
                format!("模型调用预算不足，范围 {scope}"),
            ));
        }
        if token_limit.is_some_and(|limit| tokens.saturating_add(b.total()) > limit) {
            return Err(ReportError::new(
                "MODEL_TOKEN_BUDGET_EXHAUSTED",
                format!("模型用量预算不足，范围 {scope}"),
            ));
        }
        if let Some(limit) = money_limit {
            let required = money
                .zip(b.cost_upper_microunits)
                .map(|(a, b)| a.saturating_add(b))
                .ok_or_else(|| {
                    ReportError::new("MODEL_LEGACY_COST_UNKNOWN", "既有调用费用无法确定")
                })?;
            if required > limit {
                return Err(ReportError::new(
                    "MODEL_COST_BUDGET_EXHAUSTED",
                    "模型费用预算不足",
                ));
            }
        }
    }
    Ok(())
}
