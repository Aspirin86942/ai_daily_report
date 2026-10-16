use crate::{
    snapshots::{canonical_json, digest},
    store::{RunRecord, Store},
    ReportError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenCall {
    pub version: u32,
    pub call_id: i64,
    pub run_id: String,
    pub generation_id: String,
    pub config_fingerprint: String,
    pub logical_step: String,
    pub status: String,
    pub request_fingerprint: String,
    pub request_path: String,
    pub request_sha256: String,
    pub response_path: Option<String>,
    pub response_sha256: Option<String>,
    pub row_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobBudget {
    pub job_id: String,
    pub tokens_limit: u64,
    pub calls_limit: u64,
    pub cost_limit_microunits: Option<u64>,
    pub revision_sha256: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryInputs {
    pub job_budget: Option<JobBudget>,
    pub reused_calls: BTreeMap<String, FrozenCall>,
    pub prior_calls: Vec<FrozenCall>,
}

impl FrozenCall {
    pub fn freeze(run: &RunRecord, row: &Value) -> Result<Self, ReportError> {
        if row["run_id"] != run.run_id || row["generation_id"] != run.generation_id {
            return Err(mismatch());
        }
        let mut value = row.clone();
        let object = value.as_object_mut().ok_or_else(mismatch)?;
        object.retain(|key, _| {
            [
                "call_id",
                "run_id",
                "generation_id",
                "logical_step",
                "status",
                "request_fingerprint",
                "request_path",
                "request_sha256",
                "response_path",
                "response_sha256",
            ]
            .contains(&key.as_str())
        });
        object.insert("version".into(), json!(1));
        object.insert("config_fingerprint".into(), json!(run.config_fingerprint));
        object.insert("row_sha256".into(), json!(digest(&canonical_json(row)?)));
        serde_json::from_value(value).map_err(|_| mismatch())
    }

    pub fn read(
        &self,
        store: &Store,
        current: &RunRecord,
        job_id: Option<&str>,
    ) -> Result<(RunRecord, Value), ReportError> {
        let parent = store.get(&self.run_id)?;
        let row = store
            .calls(&self.run_id)?
            .into_iter()
            .find(|row| row["call_id"] == self.call_id)
            .ok_or_else(mismatch)?;
        if self.version != 1
            || self.call_id < 1
            || parent.scope_id != current.scope_id
            || parent.target_date != current.target_date
            || canonical_json(&serde_json::to_value(Self::freeze(&parent, &row)?)?)?
                != canonical_json(&serde_json::to_value(self)?)?
        {
            return Err(mismatch());
        }
        let usage: Value = serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
        if usage["reserved_budget"]["job_id"].as_str() != job_id {
            return Err(ReportError::new(
                "MODEL_REUSE_JOB_MISMATCH",
                "引用调用不属于当前覆盖作业",
            ));
        }
        Ok((parent, row))
    }
}

fn mismatch() -> ReportError {
    ReportError::new(
        "MODEL_REUSE_CALL_MISMATCH",
        "引用调用的记录或所属运行已改变",
    )
}
