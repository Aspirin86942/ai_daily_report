pub mod budget;
pub mod contracts;
pub mod legacy;
pub mod plan;
pub mod recovery;
pub mod slicing;
pub mod transport;

use crate::{
    config::{Llm, Recovery},
    snapshots::{canonical_json, digest, SnapshotRef, SnapshotStore},
    store::{CallCompletion, CallReservation, Store},
    ReportError,
};
use chrono::{DateTime, Utc};
use contracts::{ComposedReport, FactsResponse, SelectedFact, MAX_RESPONSE_BYTES};
use plan::{ComposeGroup, DailyPlan, PlannedStep, StepKind};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use transport::ModelTransport;

#[derive(Debug)]
pub enum StepOutput {
    Facts(Vec<SelectedFact>),
    Compose(ComposedReport),
    Period(Value),
    Legacy(legacy::LegacySummary),
}
#[derive(Debug)]
pub struct StepResult {
    pub output: StepOutput,
    pub effective: PlannedStep,
    pub call_id: i64,
    pub reused: bool,
}
#[derive(Debug)]
pub struct GeneratedContent {
    pub fact_wall_ms: u64,
    pub composition_wall_ms: u64,
    pub facts: Vec<SelectedFact>,
    pub report: Option<ComposedReport>,
    pub steps: Vec<StepResult>,
}

pub struct ModelRunner<'a> {
    pub store: &'a Store,
    pub snapshots: &'a SnapshotStore,
    pub transport: &'a dyn ModelTransport,
    pub llm: &'a Llm,
    pub recovery: &'a Recovery,
    pub recovery_inputs: Option<&'a recovery::RecoveryInputs>,
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

fn validate_response(step: &PlannedStep, body: &[u8]) -> Result<StepOutput, ReportError> {
    let (content, _) = response_content(body)?;
    match step.kind {
        StepKind::LegacySummary => {
            let batch: legacy::LegacyBatch =
                serde_json::from_value(step.target["legacy_batch"].clone())?;
            Ok(StepOutput::Legacy(legacy::validate_summary(
                content, &batch,
            )?))
        }
        StepKind::Facts => {
            let response: FactsResponse =
                serde_json::from_value(content).map_err(|_| contracts::invalid())?;
            let unverified: BTreeSet<_> = step
                .entries
                .iter()
                .filter(|e| e.text_unverified)
                .map(|e| e.source_id.clone())
                .collect();
            if response.schema_version != if step.version == 5 { 4 } else { 3 } {
                return Err(contracts::invalid());
            }
            let mut selected = response.select(
                &step.logical_step,
                &step.source_ids(),
                &unverified,
                step.output_capacity_bytes,
            )?;
            crate::archive::model_input::bind_facts(step, &mut selected)?;
            Ok(StepOutput::Facts(selected))
        }
        StepKind::Compose => {
            let report: ComposedReport =
                serde_json::from_value(content).map_err(|_| contracts::invalid())?;
            if report.schema_version != if step.version == 5 { 4 } else { 3 } {
                return Err(contracts::invalid());
            }
            report.validate_part(&step.facts)?;
            Ok(StepOutput::Compose(report))
        }
        StepKind::Weekly | StepKind::Monthly => {
            crate::reports::period::validate(step.kind, &content, &step.target)?;
            Ok(StepOutput::Period(content))
        }
    }
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
            let audit = &usage["reserved_budget"]["request_audit"];
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
            let expected = if prior.revision == root.revision {
                root.clone()
            } else {
                root.shrink_once(self.llm)?
            };
            if prior.identity()? != expected.identity()? {
                return Err(ReportError::new(
                    "MODEL_PLAN_IDENTITY_MISMATCH",
                    "模型修订不符合一次缩批规则",
                ));
            }
            effective = prior;
            if matches!(root.kind, StepKind::Facts | StepKind::Compose) {
                if let Some(code) = row["error_code"].as_str().and_then(split_error) {
                    let body = self.snapshots.read_bytes(&reference(&row, "response")?)?;
                    if validate_response(&effective, &body)
                        .err()
                        .is_some_and(|error| error.code == code)
                    {
                        return Err(ReportError::new(
                            code,
                            "复用已记录的容量不足结果，继续完整拆分",
                        ));
                    }
                    return Err(ReportError::new(
                        "MODEL_PLAN_IDENTITY_MISMATCH",
                        "拆分依据与原始响应不一致",
                    ));
                }
            }
            if row["error_code"] == "MODEL_OUTPUT_TRUNCATED"
                && effective.revision == 0
                && effective.kind == StepKind::LegacySummary
            {
                effective = root.shrink_once(self.llm)?;
            }
        }
        Ok(effective)
    }

    pub fn execute(&self, root: &PlannedStep) -> Result<StepResult, ReportError> {
        root.validate_identity(self.llm)?;
        let run = self.store.get(self.run_id)?;
        if let Some(pin) = self
            .recovery_inputs
            .and_then(|inputs| inputs.reused_calls.get(&root.logical_step))
        {
            let job_id = self
                .recovery_inputs
                .and_then(|inputs| inputs.job_budget.as_ref())
                .map(|job| job.job_id.as_str());
            let (parent, row) = pin.read(self.store, &run, job_id)?;
            let usage: Value = serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
            let audit = &usage["reserved_budget"]["request_audit"];
            let plan_ref: SnapshotRef = serde_json::from_value(audit["plan_revision"].clone())
                .map_err(|_| ReportError::new("MODEL_REUSE_NOT_VALIDATED", "引用的模型计划缺失"))?;
            let step: PlannedStep = serde_json::from_value(self.snapshots.read(&plan_ref)?)?;
            let expected = if step.revision == root.revision {
                root.clone()
            } else {
                root.shrink_once(self.llm)?
            };
            if row["status"] != "succeeded"
                || pin.logical_step != root.logical_step
                || audit["root_plan_sha256"] != root.identity()?
                || step.identity()? != expected.identity()?
                || row["request_fingerprint"] != request_identity(&parent, &step)?
                || self.snapshots.read_bytes(&reference(&row, "request")?)? != step.bytes()?
                || audit["source_ranges"] != source_ranges(&step)
                || audit["response_schema_sha256"] != step.schema_sha256
            {
                return Err(ReportError::new(
                    "MODEL_REUSE_NOT_VALIDATED",
                    "引用响应与冻结请求、范围或版本不一致",
                ));
            }
            let output = validate_response(
                &step,
                &self.snapshots.read_bytes(&reference(&row, "response")?)?,
            )?;
            return Ok(StepResult {
                output,
                effective: step,
                call_id: pin.call_id,
                reused: true,
            });
        }
        if matches!(root.kind, StepKind::Facts | StepKind::Compose) {
            if let Some(inputs) = self.recovery_inputs {
                for pin in &inputs.prior_calls {
                    if pin.logical_step != root.logical_step || pin.status != "failed" {
                        continue;
                    }
                    let (parent, row) = pin.read(
                        self.store,
                        &run,
                        inputs.job_budget.as_ref().map(|job| job.job_id.as_str()),
                    )?;
                    let usage: Value =
                        serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
                    let audit = &usage["reserved_budget"]["request_audit"];
                    if audit["root_plan_sha256"] != root.identity()? {
                        continue;
                    }
                    if let Some(code) = row["error_code"].as_str().and_then(split_error) {
                        if audit["model_plan_version"] != root.version
                            || row["request_fingerprint"] != request_identity(&parent, root)?
                            || self.snapshots.read_bytes(&reference(&row, "request")?)?
                                != root.bytes()?
                            || audit["source_ranges"] != source_ranges(root)
                            || validate_response(
                                root,
                                &self.snapshots.read_bytes(&reference(&row, "response")?)?,
                            )
                            .err()
                            .is_none_or(|error| error.code != code)
                        {
                            return Err(ReportError::new(
                                "MODEL_REUSE_NOT_VALIDATED",
                                "拆分依据与冻结输入或响应不一致",
                            ));
                        }
                        return Err(ReportError::new(code, "复用先前容量不足结果，继续完整拆分"));
                    }
                }
            }
        }
        let mut step = self.effective(root, &run.generation_id)?;
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
            let request = self
                .snapshots
                .put_bytes(self.run_id, "model-request", &bytes)?;
            let revision =
                self.snapshots
                    .put(self.run_id, "model-plan", &serde_json::to_value(&step)?)?;
            let audit = json!({"model_plan_version":root.version,"root_plan_sha256":root.identity()?,"plan_revision":revision,"source_ranges":source_ranges(&step),"response_schema_sha256":step.schema_sha256,"prompt_sha256":step.prompt_sha256});
            let mut reservation = budget::Reservation::new(
                self.llm,
                self.recovery,
                bytes.len(),
                step.request["max_tokens"].as_u64().unwrap(),
                audit,
            );
            if let Some(job) = self
                .recovery_inputs
                .and_then(|inputs| inputs.job_budget.as_ref())
            {
                reservation.job_id = Some(job.job_id.clone());
                reservation.job_tokens_limit = Some(job.tokens_limit);
                reservation.job_calls_limit = Some(job.calls_limit);
                reservation.job_cost_limit_microunits = job.cost_limit_microunits;
                if let Some(revision) = &job.revision_sha256 {
                    reservation.request_audit["job_calls_revision_sha256"] = json!(revision);
                }
            }
            let call_id = self.store.reserve_call(CallReservation {
                run_id: self.run_id,
                fingerprint: &identity,
                logical_step: &step.logical_step,
                request: &request,
                budget: &reservation,
                recovery: self.recovery,
                max_attempts: self.llm.max_retries,
                now: self.now(),
            })?;
            let started = Instant::now();
            let timeout = self
                .remaining()?
                .min(Duration::from_secs_f64(self.llm.request_timeout_seconds));
            let result = self.transport.send(&bytes, timeout);
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
                        failure.retryable(),
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
                            matches!(response.status, 408 | 429 | 500..=599),
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
                                if error.code == "MODEL_OUTPUT_TRUNCATED"
                                    && step.kind == StepKind::LegacySummary
                                    && step.revision == 0
                                {
                                    step = step.shrink_once(self.llm)?;
                                    continue;
                                }
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

    pub fn generate(&self, plan: &DailyPlan) -> Result<GeneratedContent, ReportError> {
        self.generate_with_workers(plan, 1)
    }

    pub fn execute_complete(&self, root: &PlannedStep) -> Result<Vec<StepResult>, ReportError> {
        let mut pending = vec![root.clone()];
        let mut completed = Vec::new();
        while let Some(step) = pending.pop() {
            match self.execute(&step) {
                Ok(result) => completed.push(result),
                Err(error)
                    if split_error(error.code).is_some()
                        && matches!(step.kind, StepKind::Facts | StepKind::Compose) =>
                {
                    let [left, right] = step.split(self.llm)?;
                    pending.extend([right, left]);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(completed)
    }

    pub fn generate_with_workers(
        &self,
        plan: &DailyPlan,
        workers: u8,
    ) -> Result<GeneratedContent, ReportError> {
        if !(1..=4).contains(&workers) {
            return Err(ReportError::new(
                "MODEL_WORKERS_INVALID",
                "模型并发数须在 1 至 4 之间",
            ));
        }
        self.audited_calls()?;
        let mut steps = Vec::new();
        let mut facts = Vec::new();
        let fact_started = Instant::now();
        for batches in plan.batches.chunks(usize::from(workers)) {
            let results = std::thread::scope(|scope| {
                let handles: Vec<_> = batches
                    .iter()
                    .map(|batch| scope.spawn(move || self.execute_complete(batch)))
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle.join().unwrap_or_else(|_| {
                            Err(ReportError::new(
                                "MODEL_WORKER_FAILED",
                                "模型执行线程异常终止",
                            ))
                        })
                    })
                    .collect::<Vec<_>>()
            });
            // Join every in-flight request before returning an error; each has its own ledger row.
            for result in results {
                for result in result? {
                    if let StepOutput::Facts(selected) = &result.output {
                        facts.extend(selected.iter().cloned());
                    }
                    steps.push(result);
                }
            }
        }
        let fact_wall_ms = fact_started.elapsed().as_millis() as u64;
        if facts.is_empty() {
            return Ok(GeneratedContent {
                fact_wall_ms,
                composition_wall_ms: 0,
                facts,
                report: None,
                steps,
            });
        }
        let composition_started = Instant::now();
        let mut parts = Vec::new();
        let schema_version = if plan.batches.first().map(|batch| batch.version) == Some(5) {
            4
        } else {
            3
        };
        for group in plan.compose_groups(self.llm, facts.clone())? {
            match group {
                ComposeGroup::Step(step) => {
                    for result in self.execute_complete(&step)? {
                        if let StepOutput::Compose(report) = &result.output {
                            parts.push(report.clone());
                        }
                        steps.push(result);
                    }
                }
                ComposeGroup::Degraded(fact) => {
                    parts.push(ComposedReport::pending_from(&fact, schema_version));
                }
            }
        }
        let report = Some(ComposedReport::merge(parts, &facts)?);
        Ok(GeneratedContent {
            fact_wall_ms,
            composition_wall_ms: composition_started.elapsed().as_millis() as u64,
            facts,
            report,
            steps,
        })
    }

    pub fn audited_calls(&self) -> Result<Vec<Value>, ReportError> {
        let run = self.store.get(self.run_id)?;
        let mut rows: BTreeMap<i64, Value> = self
            .store
            .calls(self.run_id)?
            .into_iter()
            .map(|row| (row["call_id"].as_i64().unwrap(), row))
            .collect();
        if let Some(inputs) = self.recovery_inputs {
            for pin in inputs.reused_calls.values().chain(&inputs.prior_calls) {
                let (_, row) = pin.read(
                    self.store,
                    &run,
                    inputs.job_budget.as_ref().map(|job| job.job_id.as_str()),
                )?;
                rows.insert(pin.call_id, row);
            }
        }
        Ok(rows.into_values().collect())
    }
}

fn split_error(code: &str) -> Option<&'static str> {
    match code {
        "MODEL_OUTPUT_TRUNCATED" => Some("MODEL_OUTPUT_TRUNCATED"),
        "MODEL_FACT_CAPACITY_EXCEEDED" => Some("MODEL_FACT_CAPACITY_EXCEEDED"),
        "MODEL_FACT_COUNT_EXCEEDED" => Some("MODEL_FACT_COUNT_EXCEEDED"),
        "MODEL_COMPOSE_CAPACITY_INSUFFICIENT" => Some("MODEL_COMPOSE_CAPACITY_INSUFFICIENT"),
        _ => None,
    }
}

fn request_identity(
    run: &crate::store::RunRecord,
    step: &PlannedStep,
) -> Result<String, ReportError> {
    if step.kind == StepKind::LegacySummary {
        let batch: legacy::LegacyBatch =
            serde_json::from_value(step.target["legacy_batch"].clone())?;
        return batch.identity(run);
    }
    Ok(digest(&canonical_json(
        &json!({"generation_id":run.generation_id,"config_fingerprint":run.config_fingerprint,"plan_sha256":step.identity()?,"request_sha256":digest(&step.bytes()?)}),
    )?))
}

fn source_ranges(step: &PlannedStep) -> Value {
    let mut ranges = serde_json::Map::new();
    for entry in &step.entries {
        let value = ranges.entry(entry.source_id.clone()).or_insert(json!([]));
        value
            .as_array_mut()
            .unwrap()
            .extend(entry.text_ranges.iter().map(|r| json!(r)));
    }
    Value::Object(ranges)
}
