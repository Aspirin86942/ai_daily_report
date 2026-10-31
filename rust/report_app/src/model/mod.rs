//! Audited execution and replay of personal summary and compaction requests.
pub mod contracts;
pub mod personal;
pub mod plan;
pub mod recovery;
pub mod slicing;
pub mod transport;
use crate::{
    config::Llm,
    snapshots::{canonical_json, digest, SnapshotRef, SnapshotStore},
    store::{CallCompletion, CallReservation, Store},
    ReportError,
};
use chrono::{DateTime, Utc};
use contracts::MAX_RESPONSE_BYTES;
use plan::PlannedStep;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use transport::ModelTransport;

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
fn response_content(body: &[u8]) -> Result<(Value, Value), ReportError> {
    let response: Value = serde_json::from_slice(body).map_err(|_| contracts::invalid())?;
    let choices = response["choices"]
        .as_array()
        .filter(|c| c.len() == 1)
        .ok_or_else(contracts::invalid)?;
    let choice = &choices[0];
    if choice["finish_reason"] == "length" {
        return Err(ReportError::new(
            "MODEL_OUTPUT_TRUNCATED",
            "模型输出超过上限",
        ));
    }
    if choice["finish_reason"] != "stop" || !choice["message"]["refusal"].is_null() {
        return Err(contracts::invalid());
    }
    let text = choice["message"]["content"]
        .as_str()
        .filter(|s| s.len() <= MAX_RESPONSE_BYTES)
        .ok_or_else(contracts::invalid)?;
    Ok((
        serde_json::from_str(text).map_err(|_| contracts::invalid())?,
        response["usage"].clone(),
    ))
}

fn validate_response(
    step: &PlannedStep,
    body: &[u8],
) -> Result<personal::PersonalOutput, ReportError> {
    let (content, _) = response_content(body)?;
    personal::validate_response(step, content)
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
    fn effective(&self, root: &PlannedStep, generation: &str) -> Result<PlannedStep, ReportError> {
        let calls = self
            .store
            .generation_calls(generation, &root.logical_step)?;
        let root_sha = root.identity()?;
        let mut effective = root.clone();
        for row in calls {
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
            let expected = root.clone();
            if prior.identity()? != expected.identity()? {
                return Err(ReportError::new(
                    "MODEL_PLAN_IDENTITY_MISMATCH",
                    "模型修订不符合一次缩批规则",
                ));
            }
            effective = prior;
        }
        Ok(effective)
    }
    pub fn execute(&self, root: &PlannedStep) -> Result<StepResult, ReportError> {
        root.validate_identity(self.llm)?;
        let run = self.store.get(self.run_id)?;
        let step = self.effective(root, &run.generation_id)?;
        let mut invalid_retries = 0u32;
        loop {
            let bytes = step.bytes()?;
            let identity = request_identity(&run, &step)?;
            if let Some(row) = self.store.successful_call(&run.generation_id, &identity)? {
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
                    &step,
                    &self.snapshots.read_bytes(&reference(&row, "response")?)?,
                )?;
                return Ok(StepResult {
                    output,
                    effective: step,
                    call_id: row["call_id"].as_i64().unwrap(),
                    reused: true,
                });
            }
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
                .put_bytes(self.run_id, "model-request", &bytes)?;
            let revision =
                self.snapshots
                    .put(self.run_id, "model-plan", &serde_json::to_value(&step)?)?;
            let audit = json!({"model_plan_version":root.version,"root_plan_sha256":root.identity()?,"plan_revision":revision,"source_ranges":source_ranges(&step),"response_schema_sha256":step.schema_sha256,"prompt_sha256":step.prompt_sha256});
            let call_id = self.store.reserve_call(CallReservation {
                run_id: self.run_id,
                fingerprint: &identity,
                logical_step: &step.logical_step,
                request: &request,
                job_id: None,
                audit: &audit,
                max_attempts: self.llm.max_retries,
                now: self.now(),
            })?;
            let started = Instant::now();
            let timeout = self
                .remaining()?
                .min(Duration::from_secs_f64(self.llm.request_timeout_seconds));
            let result = self.transport.send(&bytes, timeout);
            drop(response_reservation);
            let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            let (error, retry, after) = match result {
                Err(failure) => {
                    self.store.finish_call(CallCompletion {
                        call_id,
                        status: "unknown",
                        response: None,
                        usage: json!({}),
                        error_code: Some(failure.code()),
                        duration_ms,
                        now: self.now(),
                    })?;
                    (
                        ReportError::new(failure.code(), "模型通信未取得完整响应"),
                        // Retry a personal transport failure on a later scheduled run.
                        false,
                        None,
                    )
                }
                Ok(response) => {
                    // Once a response arrives, any persistence failure stops this
                    // execution. Its reservation remains chargeable and recoverable.
                    let saved = match self.snapshots.put_bytes(
                        self.run_id,
                        "model-response",
                        &response.body,
                    ) {
                        Ok(saved) => saved,
                        Err(error) => {
                            let _ = self.store.finish_call(CallCompletion {
                                call_id,
                                status: "unknown",
                                response: None,
                                usage: json!({}),
                                error_code: Some("MODEL_RESPONSE_SAVE_FAILED"),
                                duration_ms,
                                now: self.now(),
                            });
                            return Err(error);
                        }
                    };
                    let usage: Value = serde_json::from_slice::<Value>(&response.body)
                        .ok()
                        .map(|v| v["usage"].clone())
                        .unwrap_or(Value::Null);
                    if !(200..300).contains(&response.status) {
                        self.store.finish_call(CallCompletion {
                            call_id,
                            status: "failed",
                            response: Some(&saved),
                            usage,
                            error_code: Some("MODEL_HTTP_ERROR"),
                            duration_ms,
                            now: self.now(),
                        })?;
                        (
                            ReportError::new(
                                "MODEL_HTTP_ERROR",
                                format!("模型服务返回 HTTP {}", response.status),
                            ),
                            false,
                            response.retry_after,
                        )
                    } else {
                        match validate_response(&step, &response.body) {
                            Ok(output) => {
                                self.store.finish_call(CallCompletion {
                                    call_id,
                                    status: "succeeded",
                                    response: Some(&saved),
                                    usage,
                                    error_code: None,
                                    duration_ms,
                                    now: self.now(),
                                })?;
                                return Ok(StepResult {
                                    output,
                                    effective: step,
                                    call_id,
                                    reused: false,
                                });
                            }
                            Err(error) => {
                                self.store.finish_call(CallCompletion {
                                    call_id,
                                    status: "failed",
                                    response: Some(&saved),
                                    usage,
                                    error_code: Some(error.code),
                                    duration_ms,
                                    now: self.now(),
                                })?;
                                if error.code == "MODEL_RESPONSE_INVALID"
                                    && invalid_retries < self.llm.invalid_response_retries
                                {
                                    invalid_retries += 1;
                                    (error, true, None)
                                } else {
                                    (error, false, None)
                                }
                            }
                        }
                    }
                }
            };
            if !retry {
                return Err(error);
            }
            let attempts = self
                .store
                .generation_calls(&run.generation_id, &step.logical_step)?
                .len();
            if attempts >= self.llm.max_retries as usize {
                return Err(error);
            }
            let delay = retry_after(after.as_deref(), attempts, self.now());
            if delay >= self.remaining()? {
                return Err(ReportError::new(
                    "MODEL_RUN_DEADLINE_EXCEEDED",
                    "剩余时间不足以等待模型重试",
                ));
            }
            std::thread::sleep(delay);
        }
    }

    pub fn audited_calls(&self) -> Result<Vec<Value>, ReportError> {
        self.store.calls(self.run_id)
    }
}
fn request_identity(
    run: &crate::store::RunRecord,
    step: &PlannedStep,
) -> Result<String, ReportError> {
    Ok(digest(&canonical_json(
        &json!({"generation_id":run.generation_id,"config_fingerprint":run.config_fingerprint,"plan_sha256":step.identity()?,"request_sha256":digest(&step.bytes()?)}),
    )?))
}

fn source_ranges(_: &PlannedStep) -> Value {
    json!({})
}
pub(crate) fn request_audit(usage: &Value) -> &Value {
    usage
        .get("request_audit")
        .unwrap_or(&usage["reserved_budget"]["request_audit"])
}
