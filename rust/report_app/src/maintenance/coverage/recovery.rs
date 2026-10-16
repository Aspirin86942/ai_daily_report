use super::*;
use crate::{model::recovery::FrozenCall, store::revisions::JobBudgetRevision};
use std::collections::BTreeMap;

fn retained(app: &App, job: &str, record: &Value) -> Result<Vec<Value>, ReportError> {
    let ids = record["retained_call_ids"]
        .as_array()
        .ok_or_else(|| error("COVERAGE_RECOVERY_INVALID"))?;
    let rows: Vec<_> = app
        .store
        .job_calls(job)?
        .into_iter()
        .filter(|row| ids.contains(&row["call_id"]))
        .collect();
    if rows
        .iter()
        .map(|row| row["call_id"].clone())
        .collect::<Vec<_>>()
        != *ids
        || fingerprint(&json!(rows))? != record["retained_calls_sha256"]
    {
        return Err(error("COVERAGE_RETAINED_CALLS_CHANGED"));
    }
    Ok(rows)
}
fn record(
    app: &App,
    plan: &RepairPlan,
    hash: &str,
    path: &Path,
    key: &str,
) -> Result<Option<(Value, String)>, ReportError> {
    if !path.exists() {
        return Ok(None);
    }
    let (value, sha) = read_envelope(path, key)?;
    if value["job_id"] != plan.job_id
        || value["plan_sha256"] != hash
        || !matches!(value["version"].as_u64(), Some(1 | 3))
    {
        return Err(error("COVERAGE_RECOVERY_INVALID"));
    }
    retained(app, &plan.job_id, &value)?;
    Ok(Some((value, sha)))
}
fn recoveries(
    app: &App,
    plan: &RepairPlan,
    hash: &str,
    dir: &Path,
) -> Result<Vec<(Value, String, PathBuf)>, ReportError> {
    let mut records = Vec::new();
    for index in 0..=32 {
        let path = dir.join(if index == 0 {
            "execution-recovery.json".into()
        } else {
            format!("execution-recovery-{index:03}.json")
        });
        let Some((value, sha)) = record(app, plan, hash, &path, "recovery")? else {
            break;
        };
        if index > 0
            && value["previous_recovery_sha256"]
                != records
                    .last()
                    .map(|(_, sha, _)| json!(sha))
                    .unwrap_or(Value::Null)
        {
            return Err(error("COVERAGE_RECOVERY_INVALID"));
        }
        records.push((value, sha, path));
    }
    Ok(records)
}
fn prepared(app: &App, input: &Value) -> Result<Value, ReportError> {
    let context: RunContext = serde_json::from_value(input["context"].clone())?;
    context.validate()?;
    let snapshot = app.snapshots.put(&context.run_id, "input", input)?;
    Ok(
        json!({"run_id":context.run_id,"generation_id":context.run_id,"snapshot_path":snapshot.path,"snapshot_sha256":snapshot.sha256,"input_fingerprint":input["input_fingerprint"]}),
    )
}
fn restore(
    app: &App,
    plan: &RepairPlan,
    hash: &str,
    dir: &Path,
    value: &Value,
) -> Result<(), ReportError> {
    let entries = value["prepared_runs"]
        .as_object()
        .ok_or_else(|| error("COVERAGE_RECOVERY_INVALID"))?;
    if entries.len() != plan.dates.len() {
        return Err(error("COVERAGE_RECOVERY_INVALID"));
    }
    let mut events = events::Events::open(dir, &plan.job_id, hash)?;
    for (date, entry) in entries {
        if !plan.dates.iter().any(|d| &d.date == date) {
            return Err(error("COVERAGE_RECOVERY_INVALID"));
        }
        let id = entry["run_id"]
            .as_str()
            .ok_or_else(|| error("COVERAGE_RECOVERY_INVALID"))?;
        let existing = match app.store.get(id) {
            Ok(run) => Some(run),
            Err(e) if e.code == "RUN_NOT_FOUND" => None,
            Err(e) => return Err(e),
        };
        if existing
            .as_ref()
            .is_none_or(|run| run.snapshot_path.is_none())
        {
            let reference = SnapshotRef {
                path: entry["snapshot_path"]
                    .as_str()
                    .ok_or_else(|| error("COVERAGE_PREPARED_CHANGED"))?
                    .into(),
                sha256: entry["snapshot_sha256"]
                    .as_str()
                    .ok_or_else(|| error("COVERAGE_PREPARED_CHANGED"))?
                    .into(),
            };
            let input = app.snapshots.read(&reference)?;
            if input["context"]["run_id"] != id
                || input["input_fingerprint"] != entry["input_fingerprint"]
            {
                return Err(error("COVERAGE_PREPARED_CHANGED"));
            }
            run::prepare_snapshot(
                app,
                &input,
                false,
                &format!("coverage_candidate:{}", plan.job_id),
            )?;
        }
        let run = app.store.get(id)?;
        if json!(run.snapshot_sha256) != entry["snapshot_sha256"]
            || json!(run.input_fingerprint) != entry["input_fingerprint"]
            || run.target_date != *date
            || run.run_kind != "preview"
        {
            return Err(error("COVERAGE_PREPARED_CHANGED"));
        }
        if events.state["dates"][date]["run_id"] != id {
            events.append(
                "execution_restored",
                Some(date),
                json!({"run_id":id,"status":"prepared","input_path":null}),
                json!({}),
            )?;
        }
    }
    Ok(())
}
pub(super) fn validate_current(
    app: &App,
    plan: &RepairPlan,
    hash: &str,
    dir: &Path,
) -> Result<(), ReportError> {
    let amendment = record(
        app,
        plan,
        hash,
        &dir.join("execution-amendment.json"),
        "amendment",
    )?;
    let chain = recoveries(app, plan, hash, dir)?;
    let recovery = chain.last().map(|(v, s, _)| (v.clone(), s.clone()));
    if let Some((value, _)) = &recovery {
        if value["previous_amendment_sha256"] != json!(amendment.as_ref().map(|(_, hash)| hash)) {
            return Err(error("COVERAGE_RECOVERY_INVALID"));
        }
    }
    let mut runtime = record(
        app,
        plan,
        hash,
        &dir.join("execution-runtime-revision.json"),
        "revision",
    )?;
    if let Some((revision, _)) = &runtime {
        let (base, base_hash, _) = chain
            .iter()
            .find(|(_, sha, _)| revision["base_recovery_sha256"] == *sha)
            .ok_or_else(|| error("COVERAGE_RUNTIME_REVISION_INVALID"))?;
        if revision["base_recovery_sha256"] != *base_hash
            || revision["previous_algorithm_fingerprint"] != base["algorithm_fingerprint"]
            || revision["config_fingerprint"] != base["config_fingerprint"]
            || revision["prepared_runs_sha256"] != fingerprint(&base["prepared_runs"])?
            || revision["retained_call_ids"] != base["retained_call_ids"]
            || revision["retained_calls_sha256"] != base["retained_calls_sha256"]
        {
            return Err(error("COVERAGE_RUNTIME_REVISION_INVALID"));
        }
        if recovery.as_ref().is_some_and(|(_, sha)| sha != base_hash) {
            runtime = None;
        }
    }
    let effective = runtime
        .as_ref()
        .or(recovery.as_ref())
        .or(amendment.as_ref());
    let config = effective
        .map(|(v, _)| v["config_fingerprint"].as_str().unwrap_or(""))
        .unwrap_or(&plan.config_fingerprint);
    let algorithm = effective
        .map(|(v, _)| v["algorithm_fingerprint"].as_str().unwrap_or(""))
        .unwrap_or(&plan.algorithm_fingerprint);
    if config != app.fingerprint(false)? || algorithm != algorithm_fingerprint()? {
        return Err(error("COVERAGE_EXECUTION_CONFIG_CHANGED"));
    }
    if let Some((value, _)) = recovery.as_ref().or(amendment.as_ref()) {
        restore(app, plan, hash, dir, value)?;
    }
    Ok(())
}
fn check_reason(reason: &str, note: &str) -> Result<(), ReportError> {
    if reason.trim().is_empty()
        || reason.chars().count() > 400
        || note.trim().is_empty()
        || note.chars().count() > 1000
    {
        return Err(error("COVERAGE_RECOVERY_REASON_REQUIRED"));
    }
    Ok(())
}
pub fn amend(
    app: &App,
    path: &Path,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<PathBuf, ReportError> {
    check_reason(reason, reason)?;
    let (plan, hash, dir) = load(app, path)?;
    let _job = ReportLock::acquire(&dir.join("job"), &plan.job_id, Duration::from_secs(5))?;
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs(5),
    )?;
    let output = dir.join("execution-amendment.json");
    if output.exists() {
        validate_current(app, &plan, &hash, &dir)?;
        return Ok(output);
    }
    let calls = app.store.job_calls(&plan.job_id)?;
    if calls.is_empty() || calls.iter().any(|r| r["status"] != "failed") {
        return Err(error("COVERAGE_EXECUTION_NOT_FAILED_ONLY"));
    }
    let events = events::Events::open(&dir, &plan.job_id, &hash)?;
    let mut prepared = BTreeMap::new();
    for item in &plan.dates {
        current(app, item)?;
        if let Some(id) = events.state["dates"][&item.date]["run_id"].as_str() {
            let run = app.store.get(id)?;
            if !matches!(run.status, RunStatus::Failed) || run.run_kind != "preview" {
                return Err(error("COVERAGE_EXECUTION_NOT_FAILED_ONLY"));
            }
        }
        let input = prepare_input(app, &plan, item, &dir, None, now)?;
        prepared.insert(
            item.date.clone(),
            self::prepared(app, &serde_json::to_value(input)?)?,
        );
    }
    let value = json!({"version":3,"job_id":plan.job_id,"plan_sha256":hash,"created_at":now.to_rfc3339(),"reason":reason,"initial_config_fingerprint":plan.config_fingerprint,"initial_algorithm_fingerprint":plan.algorithm_fingerprint,"config_fingerprint":app.fingerprint(false)?,"algorithm_fingerprint":algorithm_fingerprint()?,"retained_call_ids":calls.iter().map(|r|r["call_id"].clone()).collect::<Vec<_>>(),"retained_calls_sha256":fingerprint(&json!(calls))?,"prepared_runs":prepared});
    write_envelope(&output, "amendment", &value)?;
    restore(app, &plan, &hash, &dir, &value)?;
    Ok(output)
}

pub fn recover(
    app: &App,
    path: &Path,
    reason: &str,
    note: &str,
    calls_limit: Option<u64>,
    revise_runtime: bool,
    now: DateTime<Utc>,
) -> Result<PathBuf, ReportError> {
    check_reason(reason, note)?;
    let (plan, hash, dir) = load(app, path)?;
    let _job = ReportLock::acquire(&dir.join("job"), &plan.job_id, Duration::from_secs(5))?;
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs(5),
    )?;
    let chain = recoveries(app, &plan, &hash, &dir)?;
    let base = chain.last().map(|(v, s, _)| (v.clone(), s.clone()));
    let calls = app.store.job_calls(&plan.job_id)?;
    if calls.is_empty()
        || calls
            .iter()
            .any(|r| !matches!(r["status"].as_str(), Some("succeeded" | "failed")))
    {
        return Err(error("COVERAGE_RECOVERY_UNKNOWN_CALLS"));
    }
    if revise_runtime {
        let (base, base_hash) = base
            .clone()
            .ok_or_else(|| error("COVERAGE_RECOVERY_REQUIRED"))?;
        let output = dir.join("execution-runtime-revision.json");
        if output.exists() {
            validate_current(app, &plan, &hash, &dir)?;
            return Ok(output);
        }
        if base["config_fingerprint"] != app.fingerprint(false)?
            || base["retained_calls_sha256"] != fingerprint(&json!(calls))?
            || base["algorithm_fingerprint"] == algorithm_fingerprint()?
            || calls_limit.is_some_and(|n| base["calls_limit"] != n)
        {
            return Err(error("COVERAGE_RUNTIME_REVISION_INVALID"));
        }
        let value = json!({"version":3,"job_id":plan.job_id,"plan_sha256":hash,"base_recovery_sha256":base_hash,"previous_algorithm_fingerprint":base["algorithm_fingerprint"],"algorithm_fingerprint":algorithm_fingerprint()?,"config_fingerprint":app.fingerprint(false)?,"prepared_runs_sha256":fingerprint(&base["prepared_runs"])?,"retained_call_ids":base["retained_call_ids"],"retained_calls_sha256":base["retained_calls_sha256"],"created_at":now.to_rfc3339(),"reason":reason,"authorization_note":note});
        write_envelope(&output, "revision", &value)?;
        validate_current(app, &plan, &hash, &dir)?;
        return Ok(output);
    }
    if let Some((existing, _)) = &base {
        if existing["version"] == 3 && calls_limit.is_none_or(|n| existing["calls_limit"] == n) {
            validate_current(app, &plan, &hash, &dir)?;
            return Ok(chain.last().unwrap().2.clone());
        }
    }
    if chain.len() > 32 {
        return Err(error("COVERAGE_RECOVERY_LIMIT"));
    }
    let mut job_budget = budget(app, &plan)?;
    let mut pending_revision = None;
    let limit = calls_limit.unwrap_or(job_budget.calls_limit);
    if limit < job_budget.calls_limit || limit > 10000 {
        return Err(error("COVERAGE_BUDGET_INVALID"));
    }
    if limit > job_budget.calls_limit {
        let old = app.store.job_budget_revisions(&plan.job_id)?;
        let revision = JobBudgetRevision {
            version: if old.is_empty() { 1 } else { 2 },
            job_id: plan.job_id.clone(),
            plan_sha256: hash.clone(),
            previous_calls_limit: job_budget.calls_limit,
            calls_limit: limit,
            tokens_limit: job_budget.tokens_limit,
            cost_limit_microunits: job_budget.cost_limit_microunits,
            pricing: plan.pricing.clone(),
            retained_call_ids: calls
                .iter()
                .map(|r| r["call_id"].as_i64().unwrap())
                .collect(),
            retained_calls_sha256: fingerprint(&json!(calls))?,
            authorization_note: note.into(),
            previous_revision_sha256: old.last().map(|(_, sha)| sha.clone()),
            previous_tokens_limit: old.last().map(|(r, _)| r.tokens_limit),
        };
        pending_revision = Some(revision);
        job_budget.calls_limit = limit;
    }
    let events = events::Events::open(&dir, &plan.job_id, &hash)?;
    let mut inputs = BTreeMap::new();
    let mut successes = 0;
    for item in &plan.dates {
        current(app, item)?;
        for pin in &item.files {
            verify_pin(&plan, pin)?;
        }
        let input = if let Some(parent_id) = events.state["dates"][&item.date]["run_id"].as_str() {
            let parent = app.store.get(parent_id)?;
            if parent.run_kind != "preview"
                || !(matches!(parent.status, RunStatus::Failed)
                    || base.as_ref().is_some_and(|(value, _)| {
                        value["version"] == 1
                            && value["prepared_runs"][&item.date]["run_id"] == parent_id
                            && parent.phase == RunPhase::Prepared
                    }))
                || !matches!(parent.phase, RunPhase::Prepared | RunPhase::Generating)
            {
                return Err(error("COVERAGE_RECOVERY_FAILED_PREVIEW_REQUIRED"));
            }
            let snapshot = SnapshotRef {
                path: parent
                    .snapshot_path
                    .clone()
                    .ok_or_else(|| error("SNAPSHOT_MISSING"))?,
                sha256: parent
                    .snapshot_sha256
                    .clone()
                    .ok_or_else(|| error("SNAPSHOT_MISSING"))?,
            };
            let value = app.snapshots.read(&snapshot)?;
            if matches!(value["version"].as_u64(), Some(1 | 2)) {
                legacy_input(app, &plan, item, &parent, value, &mut successes, now)?
            } else {
                if value["version"] != 3 {
                    return Err(error("SNAPSHOT_VERSION_UNSUPPORTED"));
                }
                let mut input: FrozenInput = serde_json::from_value(value)?;
                // Recovery keeps the same selected input and prompt. A model/prompt change needs a failed-only amendment.
                for batch in &input.plan.batches {
                    batch.validate_identity(&app.config.llm)?;
                }
                let mut reused = input
                    .recovery_inputs
                    .as_ref()
                    .map(|r| r.reused_calls.clone())
                    .unwrap_or_default();
                let mut prior = input
                    .recovery_inputs
                    .as_ref()
                    .map(|r| r.prior_calls.clone())
                    .unwrap_or_default();
                successes += reused.len();
                for row in app.store.calls(parent_id)? {
                    let pin = FrozenCall::freeze(&parent, &row)?;
                    if row["status"] == "succeeded" {
                        successes += 1;
                        reused.insert(pin.logical_step.clone(), pin.clone());
                    }
                    prior.push(pin);
                }
                input.context = RunContext::new(
                    types::new_run_id(),
                    now,
                    types::parse_date(&item.date)?,
                    app.fingerprint(false)?,
                )?;
                input.recovery_fingerprint = app.fingerprint(true)?;
                input.recovery_inputs = Some(RecoveryInputs {
                    job_budget: Some(job_budget.clone()),
                    reused_calls: reused,
                    prior_calls: prior,
                });
                input.input_fingerprint = fingerprint(
                    &json!({"parent":input.input_fingerprint,"recovery":input.recovery_inputs}),
                )?;
                serde_json::to_value(input)?
            }
        } else {
            serde_json::to_value(prepare_input(
                app,
                &plan,
                item,
                &dir,
                Some(RecoveryInputs {
                    job_budget: Some(job_budget.clone()),
                    ..Default::default()
                }),
                now,
            )?)?
        };
        inputs.insert(item.date.clone(), input);
    }
    if successes == 0 {
        return Err(error("COVERAGE_RECOVERY_SUCCESS_REQUIRED"));
    }
    if let Some(revision) = pending_revision {
        job_budget.revision_sha256 = Some(app.store.register_job_budget_revision(&revision)?);
    }
    let mut prepared = BTreeMap::new();
    for (date, mut input) in inputs {
        if input["version"] == 3 {
            input["recovery_inputs"]["job_budget"] = json!(job_budget);
        } else {
            input["job_budget"] = json!(job_budget);
        }
        input["input_fingerprint"] = json!(fingerprint(
            &json!({"input":input["input_fingerprint"],"budget":job_budget})
        )?);
        prepared.insert(date, self::prepared(app, &input)?);
    }
    let amendment = record(
        app,
        &plan,
        &hash,
        &dir.join("execution-amendment.json"),
        "amendment",
    )?;
    let value = json!({"version":3,"job_id":plan.job_id,"plan_sha256":hash,"previous_recovery_sha256":base.as_ref().map(|(_,sha)|sha),"previous_amendment_sha256":amendment.as_ref().map(|(_,sha)|sha),"created_at":now.to_rfc3339(),"reason":reason,"authorization_note":note,"previous_config_fingerprint":plan.config_fingerprint,"previous_algorithm_fingerprint":plan.algorithm_fingerprint,"config_fingerprint":app.fingerprint(false)?,"algorithm_fingerprint":algorithm_fingerprint()?,"calls_limit":limit,"retained_call_ids":calls.iter().map(|r|r["call_id"].clone()).collect::<Vec<_>>(),"retained_calls_sha256":fingerprint(&json!(calls))?,"budget_revision_sha256":job_budget.revision_sha256,"prepared_runs":prepared});
    let output = dir.join(if chain.is_empty() {
        "execution-recovery.json".into()
    } else {
        format!("execution-recovery-{:03}.json", chain.len())
    });
    write_envelope(&output, "recovery", &value)?;
    restore(app, &plan, &hash, &dir, &value)?;
    Ok(output)
}

fn legacy_input(
    app: &App,
    plan: &RepairPlan,
    item: &RepairDate,
    parent: &crate::store::RunRecord,
    mut input: Value,
    successes: &mut usize,
    now: DateTime<Utc>,
) -> Result<Value, ReportError> {
    use crate::model::legacy::{self, LegacyBatch};
    let batches: Vec<LegacyBatch> = serde_json::from_value(input["batches"].clone())?;
    let mut reused: BTreeMap<String, FrozenCall> = if input["reused_calls"].is_null() {
        BTreeMap::new()
    } else {
        serde_json::from_value(input["reused_calls"].clone())?
    };
    let mut prior: Vec<FrozenCall> = if input["prior_calls"].is_null() {
        vec![]
    } else {
        serde_json::from_value(input["prior_calls"].clone())?
    };
    for row in app.store.calls(&parent.run_id)? {
        let pin = FrozenCall::freeze(parent, &row)?;
        if pin.status == "succeeded" {
            reused.insert(pin.logical_step.clone(), pin.clone());
        }
        prior.push(pin);
    }
    for batch in &batches {
        batch.validate()?;
        if let Some(pin) = reused.get(&batch.logical_step) {
            legacy::cached(
                &app.store,
                &app.snapshots,
                parent,
                batch,
                Some(pin),
                Some(&plan.job_id),
            )?;
            *successes += 1;
        } else {
            crate::model::plan::PlannedStep::legacy(batch, parent, &app.config.llm)?;
        }
    }
    input["context"] = serde_json::to_value(RunContext::new(
        types::new_run_id(),
        now,
        types::parse_date(&item.date)?,
        app.fingerprint(false)?,
    )?)?;
    input["rust_recovery_version"] = json!(3);
    input["reused_calls"] = json!(reused);
    input["prior_calls"] = json!(prior);
    input["recovery_fingerprint"] = json!(app.fingerprint(true)?);
    input["input_fingerprint"] = json!(fingerprint(
        &json!({"input":input["input_fingerprint"],"context":input["context"],"reused_calls":input["reused_calls"],"prior_calls":input["prior_calls"]})
    )?);
    Ok(input)
}
