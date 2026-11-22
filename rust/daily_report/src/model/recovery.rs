//! Serialized records retained for historical model job evidence.

use serde::{Deserialize, Serialize};
use serde_json::Value;
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
pub struct JobContext {
    pub job_id: String,
    // Keep retained snapshot bytes stable without applying former quota fields.
    // New jobs leave these fields absent, including formerly nullable fields.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "legacy_field",
        rename = "tokens_limit"
    )]
    _tokens_limit: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "legacy_field",
        rename = "calls_limit"
    )]
    _calls_limit: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "legacy_field",
        rename = "cost_limit_microunits"
    )]
    _cost_limit_microunits: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "legacy_field",
        rename = "revision_sha256"
    )]
    _revision_sha256: Option<Value>,
}

fn legacy_field<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl JobContext {
    pub fn new(job_id: String) -> Self {
        Self {
            job_id,
            _tokens_limit: None,
            _calls_limit: None,
            _cost_limit_microunits: None,
            _revision_sha256: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryInputs {
    pub job_budget: Option<JobContext>,
    pub reused_calls: BTreeMap<String, FrozenCall>,
    pub prior_calls: Vec<FrozenCall>,
}
