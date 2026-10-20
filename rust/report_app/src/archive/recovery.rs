use super::error;
use crate::{
    config::AppConfig,
    model::recovery::{FrozenCall, RecoveryInputs},
    run::{self, App, FrozenInput},
    snapshots::{canonical_json, digest, SnapshotRef},
    store::{locking::ReportLock, RunStatus},
    types::{self, RunContext},
    ReportError,
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};

/// Keep the failed input and receipts immutable and prepare explicit recovery lineage.
pub fn prepare(config: &AppConfig, parent_id: &str, reason: &str) -> Result<Value, ReportError> {
    if reason.trim().is_empty() || reason.len() > 2048 {
        return Err(error("ARCHIVE_RECOVERY_REASON_REQUIRED"));
    }
    let now = Utc::now();
    let app = App::open(config.clone(), now)?;
    let _lock = ReportLock::acquire(
        &app.store.path,
        &config.report.scope_id,
        Duration::from_secs(5),
    )?;
    let parent = app.store.get(parent_id)?;
    if parent.run_kind != "report"
        || !matches!(&parent.status, RunStatus::Failed)
        || parent.scope_id != config.report.scope_id
    {
        return Err(error("ARCHIVE_RECOVERY_FAILED_REPORT_REQUIRED"));
    }
    let reference = SnapshotRef {
        path: parent
            .snapshot_path
            .clone()
            .ok_or_else(|| error("SNAPSHOT_MISSING"))?,
        sha256: parent
            .snapshot_sha256
            .clone()
            .ok_or_else(|| error("SNAPSHOT_MISSING"))?,
    };
    let value = app.snapshots.read(&reference).or_else(|original| {
        super::model_input::read_frozen(config, parent_id, &reference.sha256).map_err(|_| original)
    })?;
    let mut input: FrozenInput = serde_json::from_value(value)?;
    if input.version != 4
        || input.context.run_id != parent_id
        || input.input_fingerprint != parent.input_fingerprint.as_deref().unwrap_or("")
        || input.recovery_fingerprint != app.fingerprint(true)?
        || input
            .recovery_inputs
            .as_ref()
            .is_some_and(|inputs| inputs.job_budget.is_some())
    {
        return Err(error("ARCHIVE_RECOVERY_INPUT_MISMATCH"));
    }
    for batch in &input.plan.batches {
        batch.validate_identity(&config.llm)?;
    }
    let mut recovery = input
        .recovery_inputs
        .take()
        .unwrap_or_else(|| RecoveryInputs {
            reused_calls: BTreeMap::new(),
            ..Default::default()
        });
    // Reuse is checked against each exact planned request by ModelRunner.
    for row in app.store.calls(parent_id)? {
        if row["status"] == "reserved" {
            return Err(error("ARCHIVE_RECOVERY_CALL_ACTIVE"));
        }
        let pin = FrozenCall::freeze(&parent, &row)?;
        app.snapshots.read_bytes(&SnapshotRef {
            path: pin.request_path.clone(),
            sha256: pin.request_sha256.clone(),
        })?;
        if let (Some(path), Some(sha256)) = (&pin.response_path, &pin.response_sha256) {
            app.snapshots.read_bytes(&SnapshotRef {
                path: path.clone(),
                sha256: sha256.clone(),
            })?;
        }
        if pin.status == "succeeded"
            && (pin.logical_step.starts_with("batch_") || pin.logical_step.starts_with("compose_"))
        {
            recovery
                .reused_calls
                .insert(pin.logical_step.clone(), pin.clone());
        }
        if !recovery
            .prior_calls
            .iter()
            .any(|prior| prior.call_id == pin.call_id)
        {
            recovery.prior_calls.push(pin);
        }
    }
    let reused = recovery
        .reused_calls
        .keys()
        .filter(|step| step.starts_with("batch_"))
        .count();
    let reused_composition = recovery.reused_calls.len() - reused;
    for batch in &mut input.plan.batches {
        let prefix = format!("{}.", batch.logical_step);
        let has_saved_result = recovery
            .reused_calls
            .keys()
            .any(|step| *step == batch.logical_step || step.starts_with(&prefix));
        if !has_saved_result && batch.target.get("fact_source_aliases").is_none() {
            batch.enable_source_aliases(&config.llm)?;
        }
    }
    let parent_fingerprint = input.input_fingerprint.clone();
    let id = types::new_run_id();
    input.context = RunContext::new(
        id.clone(),
        now,
        types::parse_date(&parent.target_date)?,
        app.fingerprint(false)?,
    )?;
    input.recovery_fingerprint = app.fingerprint(true)?;
    input.recovery_inputs = Some(recovery);
    let steps = input
        .plan
        .batches
        .iter()
        .map(|step| step.identity())
        .collect::<Result<Vec<_>, _>>()?;
    input.input_fingerprint = digest(&canonical_json(&json!({"parent":parent_fingerprint,
        "context":input.context,"recovery":input.recovery_inputs,"steps":steps}))?);
    let note = format!("archive_recover:{parent_id}:{reason}");
    if let Err(failure) = run::prepare_frozen(&app, &input, true, &note) {
        let _ = app
            .store
            .fail(&id, "ARCHIVE_RECOVERY_PREPARE_FAILED", true, now);
        return Err(failure);
    }
    app.store.fail(parent_id, "RUN_SUPERSEDED", true, now)?;
    Ok(
        json!({"status":"prepared","parent_run_id":parent_id,"run_id":id,
        "target_date":parent.target_date,"reused_fact_steps":reused,
        "reused_composition_steps":reused_composition,"new_model_calls":0,
        "original_requests_and_responses_preserved":true}),
    )
}
