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
            let mut report: ComposedReport =
                serde_json::from_value(content).map_err(|_| contracts::invalid())?;
            if report.schema_version != if step.version == 5 { 4 } else { 3 } {
                return Err(contracts::invalid());
            }
            if step.version == 5 {
                bind_composition(&mut report, &step.facts)?;
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

fn bind_composition(
    report: &mut ComposedReport,
    facts: &[SelectedFact],
) -> Result<(), ReportError> {
    let by_id: BTreeMap<_, _> = facts
        .iter()
        .map(|fact| (fact.fact_id.clone(), fact))
        .collect();
    let bind = |item: &mut contracts::CitedText| -> Result<(), ReportError> {
        if item.fact_ids.is_empty() || item.fact_ids.iter().any(|id| !by_id.contains_key(id)) {
            return Err(contracts::invalid());
        }
        item.source_ids = item
            .fact_ids
            .iter()
            .flat_map(|id| by_id[id].fact.source_ids.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(())
    };
    // Notes without fact IDs have no verified basis. Required facts still pass coverage checks.
    report.pending.retain(|item| !item.fact_ids.is_empty());
    report.overview.retain(|item| !item.fact_ids.is_empty());
    for item in report.overview.iter_mut().chain(&mut report.pending) {
        bind(item)?;
    }
    for matter in &mut report.matters {
        let mut matters = BTreeSet::new();
        for item in matter
            .background
            .iter_mut()
            .chain(&mut matter.details)
            .chain(&mut matter.status)
        {
            bind(item)?;
            matters.extend(item.fact_ids.iter().map(|id| by_id[id].matter_id.clone()));
        }
        if matters.len() > 1 {
            return Err(ReportError::new(
                "MODEL_COMPOSE_MATTER_MIXED",
                "成稿混合了不同事项，需完整拆分本组事实。",
            ));
        }
        matter.matter_id = Some(
            matters
                .into_iter()
                .next()
                .flatten()
                .ok_or_else(contracts::invalid)?,
        );
    }
    Ok(())
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
            if root.kind != StepKind::Compose || audit["root_plan_sha256"] == root.identity()? {
                let plan_ref: SnapshotRef = serde_json::from_value(audit["plan_revision"].clone())
                    .map_err(|_| {
                        ReportError::new("MODEL_REUSE_NOT_VALIDATED", "引用的模型计划缺失")
                    })?;
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
                                    && matches!(step.kind, StepKind::Facts | StepKind::Compose)
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
            let requests: Vec<_> = batches.iter().collect();
            let results = self.execute_parallel(&requests);
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
        let mut degraded = BTreeSet::new();
        let schema_version = if plan.batches.first().map(|batch| batch.version) == Some(5) {
            4
        } else {
            3
        };
        // Pending facts already have verified wording and provenance. Preserve them
        // directly so a second model pass cannot turn uncertainty into a conclusion.
        for fact in facts.iter().filter(|fact| fact.pending) {
            parts.push(ComposedReport::pending_from(fact, schema_version));
        }
        let confirmed: Vec<_> = facts.iter().filter(|fact| !fact.pending).cloned().collect();
        let groups = if confirmed.is_empty() {
            Vec::new()
        } else {
            plan.compose_groups(self.llm, confirmed)?
        };
        for groups in groups.chunks(usize::from(workers)) {
            let requests: Vec<_> = groups
                .iter()
                .filter_map(|group| match group {
                    ComposeGroup::Step(step) => Some(step),
                    ComposeGroup::Degraded(_) => None,
                })
                .collect();
            let mut results = self.execute_parallel(&requests).into_iter();
            for group in groups {
                match group {
                    ComposeGroup::Step(_) => {
                        for result in results.next().unwrap()? {
                            if let StepOutput::Compose(report) = &result.output {
                                parts.push(report.clone());
                            }
                            steps.push(result);
                        }
                    }
                    ComposeGroup::Degraded(fact) => {
                        degraded.insert(fact.fact_id.clone());
                        parts.push(ComposedReport::pending_from(fact, schema_version));
                    }
                }
            }
        }
        // 降级事实必须在返回的事实列表中同样标记为待确认，否则合并校验会拒绝。
        for fact in facts.iter_mut() {
            if degraded.contains(&fact.fact_id) && !fact.pending {
                fact.pending = true;
                if fact.fact.uncertainty.is_none() {
                    fact.fact.uncertainty = Some("成稿请求容量不足，本事实未纳入成稿依据。".into());
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

    fn execute_parallel(
        &self,
        steps: &[&PlannedStep],
    ) -> Vec<Result<Vec<StepResult>, ReportError>> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = steps
                .iter()
                .map(|step| scope.spawn(move || self.execute_complete(step)))
                .collect();
            // Finish and audit every in-flight request, including when a sibling fails.
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
                .collect()
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
        "MODEL_COMPOSE_COVERAGE_INSUFFICIENT" => Some("MODEL_COMPOSE_COVERAGE_INSUFFICIENT"),
        "MODEL_COMPOSE_MATTER_MIXED" => Some("MODEL_COMPOSE_MATTER_MIXED"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an explicit local snapshot corpus and run ID; never calls a model"]
    fn replay_saved_composition_responses() {
        let root = std::path::PathBuf::from(std::env::var_os("AI_DAILY_REPLAY_DATA").unwrap());
        let run = std::env::var("AI_DAILY_REPLAY_RUN").unwrap();
        let database = std::path::PathBuf::from(std::env::var_os("AI_DAILY_REPLAY_DB").unwrap());
        let connection = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = connection.prepare(
            "SELECT call_id,status,response_path,response_sha256,usage_json FROM report_model_calls WHERE run_id=? AND logical_step LIKE 'compose_%' AND response_path IS NOT NULL ORDER BY call_id",
        ).unwrap();
        let rows = statement
            .query_map([run], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .unwrap();
        let mut accepted = 0;
        let mut split = 0;
        let mut repaired = 0;
        let mut pending_preserved = 0;
        for row in rows {
            let (call, status, path, sha, usage) = row.unwrap();
            let usage: Value = serde_json::from_str(&usage).unwrap();
            let reference: SnapshotRef = serde_json::from_value(
                usage["reserved_budget"]["request_audit"]["plan_revision"].clone(),
            )
            .unwrap();
            let plan_bytes = std::fs::read(root.join(&reference.path)).unwrap();
            assert_eq!(digest(&plan_bytes), reference.sha256);
            let step: PlannedStep = serde_json::from_slice(&plan_bytes).unwrap();
            let response = std::fs::read(root.join(path)).unwrap();
            assert_eq!(digest(&response), sha);
            match validate_response(&step, &response) {
                Ok(_) => {
                    accepted += 1;
                    repaired += usize::from(status == "failed");
                }
                Err(error) if status == "failed" && split_error(error.code).is_some() => split += 1,
                Err(_) if step.facts.iter().all(|fact| fact.pending) => {
                    let parts = step
                        .facts
                        .iter()
                        .map(|fact| ComposedReport::pending_from(fact, 4))
                        .collect();
                    ComposedReport::merge(parts, &step.facts).unwrap();
                    pending_preserved += 1;
                }
                Err(error) => panic!("saved call {call} remains invalid: {}", error.code),
            }
        }
        assert!(accepted > 0);
        println!(
            "{}",
            json!({"accepted":accepted,"previously_failed_now_valid":repaired,
            "complete_splits_required":split,"pending_preserved_without_model":pending_preserved,
            "model_calls":0,"status":"passed"})
        );
    }

    #[test]
    fn uncited_model_notes_are_removed_and_fact_coverage_remains_strict() {
        let fact: SelectedFact = serde_json::from_value(json!({
            "fact_id":"f1", "pending":false, "matter_id":"m1", "fact":{
                "topic":"topic", "detail":"verified detail", "background":null,
                "conditions":[], "status":null, "source_ids":["s1"], "uncertainty":null
            }
        }))
        .unwrap();
        let cited = json!({"text":"verified detail","fact_ids":["f1"],"source_ids":["s1"]});
        let mut report: ComposedReport = serde_json::from_value(json!({
            "schema_version":4, "overview":[cited],
            "matters":[{"matter_id":"m1","title":"topic","background":null,
                "details":[cited],"status":null}],
            "pending":[{"text":"unattributed model note","fact_ids":[],"source_ids":[]}]
        }))
        .unwrap();
        assert!(report.validate_part(std::slice::from_ref(&fact)).is_err());
        bind_composition(&mut report, std::slice::from_ref(&fact)).unwrap();
        report.validate_part(std::slice::from_ref(&fact)).unwrap();
        assert!(report.pending.is_empty());
        report.overview.push(
            serde_json::from_value(json!({
                "text":"unattributed overview note","fact_ids":[],"source_ids":[]
            }))
            .unwrap(),
        );
        bind_composition(&mut report, std::slice::from_ref(&fact)).unwrap();
        assert_eq!(report.overview.len(), 1);
        report.matters[0].matter_id = Some("mistyped identifier".into());
        report.overview[0].source_ids = vec!["mistyped source".into()];
        report.matters[0].details[0].source_ids = vec!["mistyped source".into()];
        bind_composition(&mut report, std::slice::from_ref(&fact)).unwrap();
        assert_eq!(report.matters[0].matter_id.as_deref(), Some("m1"));
        assert_eq!(report.overview[0].source_ids, vec!["s1"]);
        assert_eq!(report.matters[0].details[0].text, "verified detail");
        report.validate_part(std::slice::from_ref(&fact)).unwrap();
        let mut second = fact.clone();
        second.fact_id = "f2".into();
        second.matter_id = Some("m2".into());
        let pair = [fact.clone(), second];
        assert_eq!(
            report.validate_part(&pair).unwrap_err().code,
            "MODEL_COMPOSE_COVERAGE_INSUFFICIENT"
        );
        let mut mixed = report.clone();
        mixed.matters[0].details[0].fact_ids.push("f2".into());
        assert_eq!(
            bind_composition(&mut mixed, &pair).unwrap_err().code,
            "MODEL_COMPOSE_MATTER_MIXED"
        );
        report.pending.push(
            serde_json::from_value(json!({
                "text":"unknown citation", "fact_ids":["unknown"], "source_ids":["s1"]
            }))
            .unwrap(),
        );
        assert!(bind_composition(&mut report, std::slice::from_ref(&fact)).is_err());
        assert_eq!(report.pending.len(), 1);
        assert!(report.validate_part(std::slice::from_ref(&fact)).is_err());
        report.pending.clear();
        report.matters.clear();
        assert!(report.validate_part(std::slice::from_ref(&fact)).is_err());
    }
}
