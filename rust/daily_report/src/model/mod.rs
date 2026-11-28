//! Audited execution and replay of personal summary and compaction requests.
pub mod personal;
pub mod plan;
pub mod recovery;
pub mod transport;
use crate::{
    config::Llm,
    infra::json::{canonical_json, digest, read_frozen_report_json},
    snapshots::{SnapshotRef, SnapshotStore},
    store::{CallCompletion, CallReservation, Store},
    ReportError,
};
use chrono::{DateTime, Utc};
use personal::FrozenStep;
use plan::{PlannedStep, BOUNDED_PLAN_VERSION, DETAILED_PLAN_VERSION};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use transport::{ModelTransport, TransportFailure};

const MAX_RESPONSE_BYTES: usize = 128 * 1024;

pub(crate) fn invalid_response() -> ReportError {
    ReportError::new("MODEL_RESPONSE_INVALID", "模型响应结构、内容范围或引用无效")
}

#[derive(Debug)]
pub struct StepResult {
    pub output: personal::PersonalOutput,
    pub effective: PlannedStep,
    pub call_id: i64,
    pub reused: bool,
}
pub struct ModelRunner<'a> {
    pub store: &'a Store,
    pub snapshots: &'a SnapshotStore,
    pub transport: &'a dyn ModelTransport,
    pub llm: &'a Llm,
    pub allow_new_calls: bool,
    pub run_id: &'a str,
    pub started_at: DateTime<Utc>,
    pub started: Instant,
    pub deadline: Instant,
}
fn validate_response(
    frozen: &FrozenStep<'_>,
    body: &[u8],
) -> Result<personal::PersonalOutput, ReportError> {
    let version = frozen.plan.version;
    let response = if version == BOUNDED_PLAN_VERSION {
        serde_json::from_slice(body).map_err(|_| invalid_response())?
    } else {
        std::str::from_utf8(body)
            .map_err(|_| invalid_response())
            .and_then(|text| read_frozen_report_json(text).map_err(|_| invalid_response()))?
    };
    let choices = response["choices"]
        .as_array()
        .filter(|c| c.len() == 1)
        .ok_or_else(invalid_response)?;
    let choice = &choices[0];
    if choice["finish_reason"] == "length" {
        return Err(ReportError::new(
            "MODEL_OUTPUT_TRUNCATED",
            "模型输出超过上限",
        ));
    }
    if choice["finish_reason"] != "stop" || !choice["message"]["refusal"].is_null() {
        return Err(invalid_response());
    }
    // The transport bounds the whole response. Detailed reports apply their combined
    // byte ceiling to the rendered body and evidence, after decoding the model JSON.
    let text = choice["message"]["content"]
        .as_str()
        .filter(|s| {
            matches!(version, DETAILED_PLAN_VERSION | BOUNDED_PLAN_VERSION)
                || s.len() <= MAX_RESPONSE_BYTES
        })
        .ok_or_else(invalid_response)?;
    let content = serde_json::from_str(text).map_err(|_| invalid_response())?;
    personal::validate_response(frozen, content)
}
fn reference(row: &Value, prefix: &str) -> Result<SnapshotRef, ReportError> {
    Ok(SnapshotRef {
        path: row[format!("{prefix}_path")]
            .as_str()
            .ok_or_else(|| ReportError::new("MODEL_CALL_SNAPSHOT_MISSING", "调用快照引用不完整"))?
            .into(),
        sha256: row[format!("{prefix}_sha256")]
            .as_str()
            .ok_or_else(|| ReportError::new("MODEL_CALL_SNAPSHOT_MISSING", "调用快照哈希不完整"))?
            .into(),
    })
}
pub fn retry_after(value: Option<&str>, attempt: usize, now: DateTime<Utc>) -> Duration {
    if let Some(value) = value {
        if let Ok(seconds) = value.trim().parse::<u64>() {
            return Duration::from_secs(seconds);
        }
        if let Ok(date) = DateTime::parse_from_rfc2822(value) {
            return (date.with_timezone(&Utc) - now)
                .to_std()
                .unwrap_or(Duration::ZERO);
        }
    }
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(2))
}

impl ModelRunner<'_> {
    fn now(&self) -> DateTime<Utc> {
        self.started_at + chrono::Duration::from_std(self.started.elapsed()).unwrap_or_default()
    }
    fn remaining(&self) -> Result<Duration, ReportError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| ReportError::new("MODEL_RUN_DEADLINE_EXCEEDED", "本次运行时间已用完"))
    }
    fn check_history(
        &self,
        root: &PlannedStep,
        root_sha: &str,
        generation: &str,
    ) -> Result<bool, ReportError> {
        let calls = self
            .store
            .generation_calls(generation, &root.logical_step)?;
        let mut unknown = false;
        for row in calls {
            unknown |= matches!(row["status"].as_str(), Some("unknown" | "reserved"));
            let usage: Value = serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
            let audit = request_audit(&usage);
            if audit["model_plan_version"] != root.version || audit["root_plan_sha256"] != root_sha
            {
                return Err(ReportError::new(
                    "MODEL_PLAN_IDENTITY_MISMATCH",
                    "已有步骤对应不同的冻结模型计划",
                ));
            }
            let plan_ref: SnapshotRef = serde_json::from_value(audit["plan_revision"].clone())
                .map_err(|_| {
                    ReportError::new("MODEL_PLAN_IDENTITY_MISMATCH", "冻结模型计划引用缺失")
                })?;
            let prior: PlannedStep = serde_json::from_value(self.snapshots.read(&plan_ref)?)
                .map_err(|_| {
                    ReportError::new("MODEL_PLAN_IDENTITY_MISMATCH", "冻结模型计划无效")
                })?;
            if prior.identity()? != root_sha {
                return Err(ReportError::new(
                    "MODEL_PLAN_IDENTITY_MISMATCH",
                    "模型修订不符合一次缩批规则",
                ));
            }
        }
        Ok(unknown)
    }
    pub fn execute(&self, root: &PlannedStep) -> Result<StepResult, ReportError> {
        self.execute_frozen(&root.checked(self.llm)?)
    }

    fn try_reuse_call(
        &self,
        frozen: &FrozenStep<'_>,
        run: &crate::store::RunRecord,
        identity: &str,
        bytes: &[u8],
    ) -> Result<Option<(personal::PersonalOutput, i64)>, ReportError> {
        let Some(row) = self.store.successful_call(&run.generation_id, identity)? else {
            return Ok(None);
        };
        let prior_run = self.store.get(row["run_id"].as_str().unwrap_or(""))?;
        if prior_run.scope_id != run.scope_id
            || prior_run.target_date != run.target_date
            || prior_run.config_fingerprint != run.config_fingerprint
            || self.snapshots.read_bytes(&reference(&row, "request")?)? != bytes
        {
            return Err(ReportError::new(
                "MODEL_REUSE_IDENTITY_MISMATCH",
                "成功响应的输入身份不一致",
            ));
        }
        let output = validate_response(
            frozen,
            &self.snapshots.read_bytes(&reference(&row, "response")?)?,
        )?;
        Ok(Some((output, row["call_id"].as_i64().unwrap())))
    }

    fn send_and_record(
        &self,
        frozen: &FrozenStep<'_>,
        root_sha: &str,
        identity: &str,
        bytes: &[u8],
    ) -> Result<(i64, Result<personal::PersonalOutput, ReportError>), ReportError> {
        let step = frozen.plan;
        self.remaining()?;
        if !self.allow_new_calls {
            return Err(ReportError::new(
                "RECOVERY_CONFIG_CHANGED",
                "配置已改变且仍有未完成的模型步骤",
            ));
        }
        step.check_capacity(self.llm)?;
        self.transport.prepare()?;
        let response_reservation = self.snapshots.reserve_response()?;
        let request = self
            .snapshots
            .put_bytes(self.run_id, "model-request", bytes)?;
        let revision =
            self.snapshots
                .put(self.run_id, "model-plan", &serde_json::to_value(step)?)?;
        let audit = json!({"model_plan_version":step.version,"root_plan_sha256":root_sha,"plan_revision":revision,"source_ranges":{},"response_schema_sha256":step.schema_sha256,"prompt_sha256":step.prompt_sha256});
        let call_id = self.store.reserve_call(CallReservation {
            run_id: self.run_id,
            fingerprint: identity,
            logical_step: &step.logical_step,
            request: &request,
            job_id: None,
            audit: &audit,
            max_attempts: self.llm.max_retries,
            now: self.now(),
        })?;
        let started = Instant::now();
        let response = self
            .transport
            .before_send()
            .and_then(|()| self.remaining())
            .map_err(|error| (error, false))
            .and_then(|remaining| {
                let timeout =
                    remaining.min(Duration::from_secs_f64(self.llm.request_timeout_seconds));
                self.transport.send(bytes, timeout).map_err(|failure| {
                    let unknown = !matches!(
                        failure,
                        TransportFailure::Configuration | TransportFailure::PermissionDenied
                    );
                    (
                        ReportError::new(failure.code(), "模型通信未取得完整响应"),
                        unknown,
                    )
                })
            });
        drop(response_reservation);
        let mut completion = CallCompletion {
            call_id,
            status: "failed",
            response: None,
            usage: json!({}),
            error_code: None,
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            now: self.now(),
        };
        let response = match response {
            Ok(response) => response,
            Err((error, unknown)) => {
                if unknown {
                    completion.status = "unknown";
                }
                completion.error_code = Some(error.code);
                self.store.finish_call(completion)?;
                return Err(error);
            }
        };
        let saved = match self
            .snapshots
            .put_bytes(self.run_id, "model-response", &response.body)
        {
            Ok(saved) => saved,
            Err(error) => {
                completion.status = "unknown";
                completion.error_code = Some("MODEL_RESPONSE_SAVE_FAILED");
                completion.now = self.now();
                let _ = self.store.finish_call(completion);
                return Err(error);
            }
        };
        let output = if (200..300).contains(&response.status) {
            validate_response(frozen, &response.body)
        } else {
            Err(ReportError::new(
                "MODEL_HTTP_ERROR",
                format!("模型服务返回 HTTP {}", response.status),
            ))
        };
        completion.response = Some(&saved);
        completion.usage = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .map_or(Value::Null, |response| response["usage"].clone());
        completion.error_code = output.as_ref().err().map(|error| error.code);
        if output.is_ok() {
            completion.status = "succeeded";
        }
        completion.now = self.now();
        self.store.finish_call(completion)?;
        Ok((call_id, output))
    }

    fn wait_before_retry(
        &self,
        generation_id: &str,
        logical_step: &str,
        error: ReportError,
    ) -> Result<(), ReportError> {
        let attempts = self
            .store
            .generation_calls(generation_id, logical_step)?
            .len();
        if attempts >= self.llm.max_retries as usize {
            return Err(error);
        }
        let delay = retry_after(None, attempts, self.now());
        if delay >= self.remaining()? {
            return Err(ReportError::new(
                "MODEL_RUN_DEADLINE_EXCEEDED",
                "剩余时间不足以等待模型重试",
            ));
        }
        std::thread::sleep(delay);
        Ok(())
    }

    fn execute_frozen(&self, frozen: &FrozenStep<'_>) -> Result<StepResult, ReportError> {
        let step = frozen.plan;
        let run = self.store.get(self.run_id)?;
        let root_sha = step.identity()?;
        let unknown = self.check_history(step, &root_sha, &run.generation_id)?;
        let bytes = step.bytes()?;
        let identity = request_identity(&run, &root_sha, &bytes)?;
        let mut invalid_retries = 0u32;
        let (output, call_id, reused) = loop {
            if let Some((output, call_id)) = self.try_reuse_call(frozen, &run, &identity, &bytes)? {
                break (output, call_id, true);
            }
            if step.version == BOUNDED_PLAN_VERSION && unknown {
                return Err(ReportError::new(
                    "MODEL_CALL_RESULT_UNKNOWN",
                    "同一冻结步骤已有结果未知的调用",
                ));
            }
            let (call_id, output) = self.send_and_record(frozen, &root_sha, &identity, &bytes)?;
            let error = match output {
                Ok(output) => break (output, call_id, false),
                Err(error) => error,
            };
            if error.code != "MODEL_RESPONSE_INVALID"
                || invalid_retries >= self.llm.invalid_response_retries
            {
                return Err(error);
            }
            invalid_retries += 1;
            self.wait_before_retry(&run.generation_id, &step.logical_step, error)?;
        };
        Ok(StepResult {
            output,
            effective: step.clone(),
            call_id,
            reused,
        })
    }

    pub fn audited_calls(&self) -> Result<Vec<Value>, ReportError> {
        self.store.calls(self.run_id)
    }
}
fn request_identity(
    run: &crate::store::RunRecord,
    plan_sha256: &str,
    request: &[u8],
) -> Result<String, ReportError> {
    Ok(digest(&canonical_json(
        &json!({"generation_id":run.generation_id,"config_fingerprint":run.config_fingerprint,"plan_sha256":plan_sha256,"request_sha256":digest(request)}),
    )?))
}

pub(crate) fn request_audit(usage: &Value) -> &Value {
    usage
        .get("request_audit")
        .unwrap_or(&usage["reserved_budget"]["request_audit"])
}
