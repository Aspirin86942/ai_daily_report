//! Frozen personal requests retain their existing serialized shape and hashes.
use super::personal::FrozenStep;
use crate::{
    config::Llm,
    snapshots::{canonical_json, fingerprint},
    ReportError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(super) const DETAILED_PLAN_VERSION: u32 = 7;
pub(super) const BOUNDED_PLAN_VERSION: u32 = 8;

pub(super) fn request_too_large() -> ReportError {
    ReportError::new("MODEL_REQUEST_TOO_LARGE", "冻结模型请求超过容量")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    PersonalSummary,
    Weekly,
    Monthly,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedStep {
    pub version: u32,
    pub kind: StepKind,
    pub logical_step: String,
    pub revision: u32,
    pub parent_sha256: Option<String>,
    pub prompt_sha256: String,
    pub schema_sha256: String,
    pub target: Value,
    pub entries: Vec<Value>,
    pub facts: Vec<Value>,
    pub source_map: Value,
    pub coverage: Value,
    pub output_capacity_bytes: usize,
    pub request: Value,
}

pub(super) fn payload(
    llm: &Llm,
    system: String,
    user: Value,
    max_tokens: u64,
) -> Result<Value, ReportError> {
    let mut value = json!({"model":llm.model_id,"messages":[{"role":"system","content":system},{"role":"user","content":String::from_utf8(canonical_json(&user)?).unwrap()}],"temperature":llm.temperature,"max_tokens":max_tokens,"response_format":{"type":"json_object"}});
    // Opt-in keeps saved requests and configuration fingerprints byte-compatible.
    if let Some(effort) = &llm.reasoning_effort {
        let fields = value.as_object_mut().unwrap();
        fields.remove("max_tokens");
        fields.insert("max_completion_tokens".into(), json!(max_tokens));
        fields.insert("reasoning_effort".into(), json!(effort));
        if effort != "none" {
            fields.remove("temperature");
        }
    }
    if let Some(mode) = &llm.thinking_mode {
        let thinking = json!({"type":mode});
        match llm.thinking_parameter_location.as_deref() {
            Some("extra_body") => value["extra_body"] = json!({"thinking":thinking}),
            Some("top_level") | None => value["thinking"] = thinking,
            _ => {
                return Err(ReportError::new(
                    "MODEL_CAPABILITY_UNVERIFIED",
                    "思考参数位置未确认",
                ))
            }
        }
    }
    Ok(value)
}
pub fn request_limit(llm: &Llm, output: u64) -> Result<usize, ReportError> {
    let context = llm
        .context_window_tokens
        .ok_or_else(|| ReportError::new("MODEL_CAPABILITY_UNVERIFIED", "模型上下文容量尚未确认"))?;
    let maximum = llm
        .verified_max_output_tokens
        .ok_or_else(|| ReportError::new("MODEL_CAPABILITY_UNVERIFIED", "模型输出容量尚未确认"))?;
    if output > maximum.min(llm.max_tokens) || output == 0 {
        return Err(ReportError::new(
            "MODEL_OUTPUT_LIMIT_INVALID",
            "模型输出上限无效",
        ));
    }
    Ok(llm.max_request_utf8_bytes.min(
        usize::try_from(context.saturating_sub(1024).saturating_sub(output)).unwrap_or(usize::MAX),
    ))
}

impl PlannedStep {
    pub fn bytes(&self) -> Result<Vec<u8>, ReportError> {
        canonical_json(&self.request)
    }
    pub fn identity(&self) -> Result<String, ReportError> {
        fingerprint(&serde_json::to_value(self)?)
    }
    pub fn validate(&self, llm: &Llm) -> Result<(), ReportError> {
        self.validate_identity(llm)?;
        self.check_capacity(llm)
    }
    pub fn validate_identity(&self, llm: &Llm) -> Result<(), ReportError> {
        self.checked(llm).map(|_| ())
    }
    pub(super) fn checked(&self, llm: &Llm) -> Result<FrozenStep<'_>, ReportError> {
        if !matches!(
            self.version,
            6 | DETAILED_PLAN_VERSION | BOUNDED_PLAN_VERSION
        ) || self.kind != StepKind::PersonalSummary
            || self.revision > 1
            || self.logical_step.is_empty()
            || self.logical_step.len() > 128
        {
            return Err(ReportError::new(
                "MODEL_PLAN_INVALID",
                "模型计划版本或步骤无效",
            ));
        }
        // A completed call keeps its original output allowance even if today's
        // verified capacity is lower. Model identity and payload fields still match.
        let tokens = self.frozen_output_tokens()?;
        let frozen = FrozenStep::read(self)?;
        let (prompt_hash, schema_hash, request) = frozen.request(llm, tokens)?;
        if prompt_hash != self.prompt_sha256
            || schema_hash != self.schema_sha256
            || canonical_json(&request)? != self.bytes()?
        {
            return Err(ReportError::new(
                "MODEL_PLAN_IDENTITY_MISMATCH",
                "模型计划与提示词、配置或输入不一致",
            ));
        }
        Ok(frozen)
    }
    pub(super) fn frozen_output_tokens(&self) -> Result<u64, ReportError> {
        let tokens = match (
            self.request.get("max_tokens"),
            self.request.get("max_completion_tokens"),
        ) {
            (Some(value), None) | (None, Some(value)) => value.as_u64(),
            _ => None,
        };
        tokens.filter(|&n| n > 0).ok_or_else(|| {
            ReportError::new(
                "MODEL_PLAN_IDENTITY_MISMATCH",
                "冻结模型请求缺少有效的输出上限",
            )
        })
    }
    pub(super) fn byte_limit(&self, llm: &Llm) -> Result<usize, ReportError> {
        request_limit(llm, self.frozen_output_tokens()?)
    }
    pub fn check_capacity(&self, llm: &Llm) -> Result<(), ReportError> {
        if self.bytes()?.len() > self.byte_limit(llm)? {
            return Err(request_too_large());
        }
        Ok(())
    }
}
