use crate::{
    config::{Llm, ModelPricing, Recovery},
    ReportError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub version: u32,
    pub input_tokens_upper: u64,
    pub output_tokens_upper: u64,
    pub legacy_tokens_upper: u64,
    pub pricing: Option<ModelPricing>,
    pub cost_upper_microunits: Option<u64>,
    pub run_tokens_limit: u64,
    pub day_tokens_limit: u64,
    pub run_cost_limit_microunits: Option<u64>,
    pub day_cost_limit_microunits: Option<u64>,
    pub job_id: Option<String>,
    pub job_tokens_limit: Option<u64>,
    pub job_calls_limit: Option<u64>,
    pub job_cost_limit_microunits: Option<u64>,
    #[serde(default)]
    pub request_audit: Value,
}

pub fn cost(input: u64, output: u64, pricing: &ModelPricing) -> u64 {
    let numerator = (u128::from(input) * u128::from(pricing.input_per_million_microunits))
        .saturating_add(u128::from(output) * u128::from(pricing.output_per_million_microunits));
    u64::try_from(numerator.div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

impl Reservation {
    pub fn new(llm: &Llm, recovery: &Recovery, bytes: usize, output: u64, audit: Value) -> Self {
        let input = (bytes as u64).saturating_add(1024);
        let maximum_output = llm.max_tokens.max(llm.facts_output_tokens());
        let legacy = (llm.max_request_utf8_bytes as u64)
            .saturating_add(1024)
            .saturating_add(maximum_output);
        let pricing = recovery.model_pricing.clone();
        let worst_cost = pricing
            .as_ref()
            .map(|p| cost(legacy - maximum_output, maximum_output, p));
        Self {
            version: 1,
            input_tokens_upper: input,
            output_tokens_upper: output,
            legacy_tokens_upper: legacy,
            cost_upper_microunits: pricing.as_ref().map(|p| cost(input, output, p)),
            pricing,
            run_tokens_limit: recovery
                .max_tokens_per_run
                .unwrap_or_else(|| legacy.saturating_mul(recovery.max_calls_per_run)),
            day_tokens_limit: recovery
                .max_tokens_per_calendar_day
                .unwrap_or_else(|| legacy.saturating_mul(recovery.max_calls_per_calendar_day)),
            run_cost_limit_microunits: recovery.max_cost_per_run_microunits.or_else(|| {
                worst_cost
                    .filter(|v| *v > 0)
                    .map(|v| v.saturating_mul(recovery.max_calls_per_run))
            }),
            day_cost_limit_microunits: recovery.max_cost_per_calendar_day_microunits.or_else(
                || {
                    worst_cost
                        .filter(|v| *v > 0)
                        .map(|v| v.saturating_mul(recovery.max_calls_per_calendar_day))
                },
            ),
            job_id: None,
            job_tokens_limit: None,
            job_calls_limit: None,
            job_cost_limit_microunits: None,
            request_audit: audit,
        }
    }

    pub fn total(&self) -> u64 {
        self.input_tokens_upper
            .saturating_add(self.output_tokens_upper)
    }

    pub fn validate(&self) -> Result<(), ReportError> {
        if self.version != 1
            || self.output_tokens_upper == 0
            || self.legacy_tokens_upper < self.total()
            || self.run_tokens_limit == 0
            || self.day_tokens_limit == 0
            || self.cost_upper_microunits
                != self
                    .pricing
                    .as_ref()
                    .map(|p| cost(self.input_tokens_upper, self.output_tokens_upper, p))
            || (self.pricing.is_none()
                && [
                    self.run_cost_limit_microunits,
                    self.day_cost_limit_microunits,
                    self.job_cost_limit_microunits,
                ]
                .iter()
                .any(Option::is_some))
            || (self.job_id.is_none()
                && [
                    self.job_calls_limit,
                    self.job_tokens_limit,
                    self.job_cost_limit_microunits,
                ]
                .iter()
                .any(Option::is_some))
            || self.job_id.as_ref().is_some_and(|id| {
                id.is_empty()
                    || id.len() > 128
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        {
            return Err(ReportError::new("MODEL_BUDGET_INVALID", "模型预算记录无效"));
        }
        Ok(())
    }
}

pub fn usage_tokens(usage: &Value) -> (Option<u64>, Option<u64>, Option<u64>) {
    let input = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64);
    let output = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64);
    let total = match (input, output, usage["total_tokens"].as_u64()) {
        (Some(i), Some(o), total) => Some(total.unwrap_or(0).max(i.saturating_add(o))),
        (_, _, total) => total,
    };
    (input, output, total)
}

pub fn charged(status: &str, prior: &Reservation, usage: &Value) -> (u64, Option<u64>) {
    let (input, output, total) = usage_tokens(usage);
    let known = matches!(status, "succeeded" | "failed");
    let tokens = if known {
        total.unwrap_or(prior.total())
    } else {
        prior.total()
    };
    let mut money = prior.cost_upper_microunits;
    if let (true, Some(i), Some(o), Some(p)) = (known, input, output, &prior.pricing) {
        let mut actual = cost(i, o, p);
        let extra = total.unwrap_or(0).saturating_sub(i.saturating_add(o));
        if extra > 0 {
            let rate = p
                .input_per_million_microunits
                .max(p.output_per_million_microunits);
            let extra_cost =
                u64::try_from((u128::from(extra) * u128::from(rate)).div_ceil(1_000_000))
                    .unwrap_or(u64::MAX);
            actual = actual.saturating_add(extra_cost).max(money.unwrap_or(0));
        }
        money = Some(actual);
    }
    (tokens, money)
}
