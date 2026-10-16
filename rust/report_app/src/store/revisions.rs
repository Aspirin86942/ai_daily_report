//! Append-only job budget revisions. Old reservations and their consumption never change.
use super::{query, Store};
use crate::{
    config::ModelPricing,
    model::budget::Reservation,
    snapshots::{canonical_json, digest, valid_hash},
    ReportError,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobBudgetRevision {
    pub version: u32,
    pub job_id: String,
    pub plan_sha256: String,
    pub previous_calls_limit: u64,
    pub calls_limit: u64,
    pub tokens_limit: u64,
    pub cost_limit_microunits: Option<u64>,
    pub pricing: Option<ModelPricing>,
    pub retained_call_ids: Vec<i64>,
    pub retained_calls_sha256: String,
    pub authorization_note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_revision_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_tokens_limit: Option<u64>,
}

pub type RevisionChain = Vec<(JobBudgetRevision, String)>;
fn changed() -> ReportError {
    ReportError::new(
        "MODEL_JOB_BUDGET_CHANGED",
        "作业预算修订链或保留的调用记录不一致",
    )
}
fn hash(value: &Value) -> Result<String, ReportError> {
    Ok(digest(&canonical_json(value)?))
}
fn metadata(conn: &Connection, key: &str) -> Result<Option<Value>, ReportError> {
    conn.query_row(
        "SELECT value FROM report_metadata WHERE key=?",
        [key],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|text| serde_json::from_str(&text).map_err(|_| changed()))
    .transpose()
}
pub fn job_rows(conn: &Connection, job_id: &str) -> Result<Vec<Value>, ReportError> {
    query(conn,"SELECT call_id,run_id,status,error_code,usage_json,request_sha256,response_sha256 FROM report_model_calls WHERE json_extract(usage_json,'$.reserved_budget.job_id')=? ORDER BY call_id",[job_id])
}
fn reservation(row: &Value) -> Result<Reservation, ReportError> {
    let usage: Value = serde_json::from_str(row["usage_json"].as_str().ok_or_else(changed)?)
        .map_err(|_| changed())?;
    let prior: Reservation =
        serde_json::from_value(usage["reserved_budget"].clone()).map_err(|_| changed())?;
    prior.validate()?;
    Ok(prior)
}
impl JobBudgetRevision {
    pub fn validate(&self) -> Result<(), ReportError> {
        if !matches!(self.version, 1 | 2)
            || self.job_id.is_empty()
            || self.job_id.len() > 128
            || !self
                .job_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            || !valid_hash(&self.plan_sha256)
            || !valid_hash(&self.retained_calls_sha256)
            || self.previous_calls_limit == 0
            || self.calls_limit <= self.previous_calls_limit
            || self.tokens_limit == 0
            || self.authorization_note.trim().is_empty()
            || self.authorization_note.chars().count() > 1000
            || self.retained_call_ids.is_empty()
            || self.retained_call_ids.len() > 10000
            || self.retained_call_ids.iter().any(|id| *id < 1)
            || self
                .retained_call_ids
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || (self.cost_limit_microunits.is_some() && self.pricing.is_none())
            || match self.version {
                1 => {
                    self.previous_revision_sha256.is_some() || self.previous_tokens_limit.is_some()
                }
                _ => {
                    self.previous_revision_sha256
                        .as_deref()
                        .is_none_or(|s| !valid_hash(s))
                        || self
                            .previous_tokens_limit
                            .is_none_or(|n| n == 0 || n > self.tokens_limit)
                }
            }
        {
            return Err(changed());
        }
        Ok(())
    }
    fn same_scope(&self, parent: &Self) -> Result<bool, ReportError> {
        Ok(self.job_id == parent.job_id
            && self.plan_sha256 == parent.plan_sha256
            && self.cost_limit_microunits == parent.cost_limit_microunits
            && serde_json::to_value(&self.pricing)? == serde_json::to_value(&parent.pricing)?)
    }
}
fn decode(
    envelope: Value,
    identity: Option<&str>,
) -> Result<(JobBudgetRevision, String), ReportError> {
    let actual = hash(&envelope["revision"])?;
    if envelope["sha256"] != actual || identity.is_some_and(|expected| expected != actual) {
        return Err(changed());
    }
    let revision: JobBudgetRevision =
        serde_json::from_value(envelope["revision"].clone()).map_err(|_| changed())?;
    revision.validate()?;
    Ok((revision, actual))
}
pub fn read_chain(conn: &Connection, job_id: &str) -> Result<RevisionChain, ReportError> {
    let base = metadata(conn, &format!("job_calls_revision:{job_id}"))?;
    let head = metadata(conn, &format!("job_budget_revision_head:{job_id}"))?;
    let Some(base) = base else {
        if head.is_some() {
            return Err(changed());
        }
        return Ok(vec![]);
    };
    let anchor = decode(base, None)?;
    if anchor.0.version != 1 || anchor.0.job_id != job_id {
        return Err(changed());
    }
    let mut identity = head
        .as_ref()
        .map(|v| v["sha256"].as_str().ok_or_else(changed))
        .transpose()?
        .unwrap_or(&anchor.1)
        .to_owned();
    let mut seen = BTreeSet::new();
    let mut reverse = Vec::new();
    while identity != anchor.1 {
        if seen.len() >= 32 || !seen.insert(identity.clone()) {
            return Err(changed());
        }
        let item = decode(
            metadata(conn, &format!("job_budget_revision:{job_id}:{identity}"))?
                .ok_or_else(changed)?,
            Some(&identity),
        )?;
        if item.0.version != 2 {
            return Err(changed());
        }
        identity = item
            .0
            .previous_revision_sha256
            .clone()
            .ok_or_else(changed)?;
        reverse.push(item);
    }
    let mut chain = vec![anchor];
    chain.extend(reverse.into_iter().rev());
    let rows = job_rows(conn, job_id)?;
    for (index, (revision, _)) in chain.iter().enumerate() {
        let retained: Vec<_> = rows
            .iter()
            .filter(|row| {
                revision
                    .retained_call_ids
                    .contains(&row["call_id"].as_i64().unwrap_or(0))
            })
            .cloned()
            .collect();
        if retained
            .iter()
            .map(|r| r["call_id"].as_i64().unwrap())
            .collect::<Vec<_>>()
            != revision.retained_call_ids
            || hash(&json!(retained))? != revision.retained_calls_sha256
        {
            return Err(changed());
        }
        if index > 0 {
            let (parent, parent_hash) = &chain[index - 1];
            if revision.previous_revision_sha256.as_ref() != Some(parent_hash)
                || revision.previous_calls_limit != parent.calls_limit
                || revision.previous_tokens_limit != Some(parent.tokens_limit)
                || !revision.same_scope(parent)?
                || parent
                    .retained_call_ids
                    .iter()
                    .any(|id| !revision.retained_call_ids.contains(id))
            {
                return Err(changed());
            }
        }
    }
    Ok(chain)
}
pub fn current_approved(current: &Reservation, chain: &RevisionChain) -> Result<bool, ReportError> {
    if let Some((head, identity)) = chain.last() {
        Ok(
            current.request_audit["job_calls_revision_sha256"] == *identity
                && current.job_calls_limit == Some(head.calls_limit)
                && current.job_tokens_limit == Some(head.tokens_limit)
                && current.job_cost_limit_microunits == head.cost_limit_microunits
                && serde_json::to_value(&current.pricing)? == serde_json::to_value(&head.pricing)?,
        )
    } else {
        Ok(current.request_audit["job_calls_revision_sha256"].is_null())
    }
}
pub fn reservation_approved(
    prior: &Reservation,
    call_id: i64,
    chain: &RevisionChain,
    current: &Reservation,
) -> Result<bool, ReportError> {
    if prior.job_cost_limit_microunits != current.job_cost_limit_microunits
        || serde_json::to_value(&prior.pricing)? != serde_json::to_value(&current.pricing)?
    {
        return Ok(false);
    }
    let identity = prior.request_audit["job_calls_revision_sha256"].as_str();
    if chain.is_empty() {
        return Ok(
            identity == current.request_audit["job_calls_revision_sha256"].as_str()
                && prior.job_calls_limit == current.job_calls_limit
                && prior.job_tokens_limit == current.job_tokens_limit,
        );
    }
    let anchor = &chain[0].0;
    if identity.is_none()
        && prior.job_calls_limit == Some(anchor.previous_calls_limit)
        && prior.job_tokens_limit == Some(anchor.tokens_limit)
        && anchor.retained_call_ids.contains(&call_id)
    {
        return Ok(true);
    }
    Ok(chain.iter().enumerate().any(|(index, (r, key))| {
        identity == Some(key.as_str())
            && prior.job_calls_limit == Some(r.calls_limit)
            && prior.job_tokens_limit == Some(r.tokens_limit)
            && chain
                .get(index + 1)
                .is_none_or(|(next, _)| next.retained_call_ids.contains(&call_id))
    }))
}
impl Store {
    pub fn job_calls(&self, job_id: &str) -> Result<Vec<Value>, ReportError> {
        job_rows(&self.connection()?, job_id)
    }
    pub fn job_budget_revisions(&self, job_id: &str) -> Result<RevisionChain, ReportError> {
        read_chain(&self.connection()?, job_id)
    }
    pub fn register_job_budget_revision(
        &self,
        revision: &JobBudgetRevision,
    ) -> Result<String, ReportError> {
        revision.validate()?;
        let value = serde_json::to_value(revision)?;
        let identity = hash(&value)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let chain = read_chain(&tx, &revision.job_id)?;
        if chain.iter().any(|(_, key)| key == &identity) {
            return Ok(identity);
        }
        let rows = job_rows(&tx, &revision.job_id)?;
        if rows
            .iter()
            .map(|r| r["call_id"].as_i64().unwrap())
            .collect::<Vec<_>>()
            != revision.retained_call_ids
            || hash(&json!(rows))? != revision.retained_calls_sha256
        {
            return Err(changed());
        }
        if let Some((parent, parent_hash)) = chain.last() {
            if revision.version != 2
                || revision.previous_revision_sha256.as_ref() != Some(parent_hash)
                || revision.previous_calls_limit != parent.calls_limit
                || revision.previous_tokens_limit != Some(parent.tokens_limit)
                || !revision.same_scope(parent)?
            {
                return Err(changed());
            }
        } else if revision.version != 1 {
            return Err(changed());
        }
        let current = reservation(rows.last().ok_or_else(changed)?)?;
        for row in &rows {
            let prior = reservation(row)?;
            if !matches!(row["status"].as_str(), Some("succeeded" | "failed"))
                || !reservation_approved(
                    &prior,
                    row["call_id"].as_i64().unwrap(),
                    &chain,
                    &current,
                )?
                || (chain.is_empty()
                    && (prior.job_calls_limit != Some(revision.previous_calls_limit)
                        || prior.job_tokens_limit != Some(revision.tokens_limit)
                        || prior.job_cost_limit_microunits != revision.cost_limit_microunits
                        || serde_json::to_value(&prior.pricing)?
                            != serde_json::to_value(&revision.pricing)?))
            {
                return Err(changed());
            }
        }
        let key = if revision.version == 1 {
            format!("job_calls_revision:{}", revision.job_id)
        } else {
            format!("job_budget_revision:{}:{identity}", revision.job_id)
        };
        tx.execute(
            "INSERT INTO report_metadata(key,value) VALUES (?,?)",
            params![key, json!({"revision":value,"sha256":identity}).to_string()],
        )?;
        if revision.version == 2 {
            tx.execute("INSERT INTO report_metadata(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![format!("job_budget_revision_head:{}",revision.job_id),json!({"sha256":identity}).to_string()])?;
        }
        tx.commit()?;
        Ok(identity)
    }
}
