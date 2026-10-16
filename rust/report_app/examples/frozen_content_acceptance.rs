//! Compare model coverage on an existing frozen excerpt, without rescanning files.
//! `prepare` makes no model calls. `send` requires the operator's cost authorization.
use ai_daily_report::{
    config::AppConfig,
    model::{
        plan,
        recovery::{FrozenCall, RecoveryInputs},
        slicing::union_chars,
    },
    reports::DAILY_TEMPLATE_V3,
    run::{prepare_frozen, resume_frozen, App, FrozenInput},
    snapshots::{canonical_json, digest, fingerprint},
    types::{new_run_id, RunContext},
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{fs, path::Path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 || !matches!(args[1].as_str(), "prepare" | "recompose" | "send") {
        return Err("usage: frozen_content_acceptance prepare|recompose|send PROGRAM_DIR CONFIG_DIR FROZEN_INPUT OUTPUT_DIR".into());
    }
    let root = Path::new(&args[5]);
    fs::create_dir_all(root)?;
    let root = fs::canonicalize(root)?;
    let mut config = AppConfig::load(Path::new(&args[2]), Some(Path::new(&args[3])))?;
    config.paths.data_dir = root.join("data");
    config.paths.reports_dir = root.join("reports");
    config.paths.db_dir = root.join("data/db");
    config.paths.log_dir = root.join("logs");
    config.report.notes_dir = root.join("notes");
    config.index_db_path = root.join("data/db/scan.sqlite3");
    config.llm.facts_max_tokens = 16384;
    config.llm.max_batches = 32;
    let now = Utc::now();
    let mut app = App::open(config, now)?;
    app.model_workers = 2;
    let current = root.join("prepared-input-current.json");
    let prepared = if args[1] == "recompose" || current.exists() {
        current
    } else {
        root.join("prepared-input.json")
    };
    if matches!(args[1].as_str(), "prepare" | "recompose") {
        if prepared.exists() {
            return Err("prepared input already exists; use send to resume".into());
        }
        let bytes = fs::read(&args[4])?;
        let envelope: Value = serde_json::from_slice(&bytes)?;
        let mut input: FrozenInput =
            serde_json::from_value(envelope.get("input").unwrap_or(&envelope).clone())?;
        let recovery = if args[1] == "recompose" {
            let parent = app.store.get(&input.context.run_id)?;
            let mut recovery = RecoveryInputs::default();
            for row in app.store.calls(&parent.run_id)? {
                let pin = FrozenCall::freeze(&parent, &row)?;
                if pin.status == "succeeded" && pin.logical_step.starts_with("batch_") {
                    recovery
                        .reused_calls
                        .insert(pin.logical_step.clone(), pin.clone());
                }
                recovery.prior_calls.push(pin);
            }
            Some(recovery)
        } else {
            None
        };
        let parent = digest(&bytes);
        let mut sources = Vec::new();
        for source in &input.sources {
            if !source.usable() {
                continue;
            }
            let mut chars = vec![None; source.excerpt_chars];
            for entry in input
                .plan
                .batches
                .iter()
                .flat_map(|batch| &batch.entries)
                .filter(|entry| entry.source_id == source.source_id)
            {
                let text: Vec<_> = entry.text.chars().collect();
                let mut offset = 0;
                for range in &entry.text_ranges {
                    for (slot, ch) in chars
                        .get_mut(range.start..range.end)
                        .ok_or("invalid source range")?
                        .iter_mut()
                        .zip(
                            text.get(offset..offset + range.end - range.start)
                                .ok_or("invalid entry range")?,
                        )
                    {
                        if slot.is_some_and(|prior| prior != *ch) {
                            return Err("overlapping frozen excerpts differ".into());
                        }
                        *slot = Some(*ch);
                    }
                    offset += range.end - range.start + 1;
                }
            }
            let text: String = chars
                .into_iter()
                .collect::<Option<String>>()
                .ok_or("original frozen plan does not contain the entire excerpt")?;
            sources.push(source.input(&text)?.ok_or("unusable source")?);
        }
        input.plan = plan::prepare(
            &app.config.llm,
            input.plan.target.clone(),
            &sources,
            input.plan.coverage["scan"].clone(),
        )?;
        input.gaps.retain(|gap| gap.stage != "model");
        for source in &mut input.sources {
            let ranges: Vec<_> = input
                .plan
                .batches
                .iter()
                .flat_map(|batch| &batch.entries)
                .filter(|entry| entry.source_id == source.source_id)
                .flat_map(|entry| entry.text_ranges.clone())
                .collect();
            source.model_chars = union_chars(&ranges);
            source.model_truncated = source.model_chars < source.excerpt_chars;
            if source.usable() && source.model_truncated {
                return Err("new plan omitted part of an excerpt".into());
            }
        }
        input.context = RunContext::new(
            new_run_id(),
            now,
            input.context.target_date,
            app.fingerprint(false)?,
        )?;
        input.recovery_fingerprint = app.fingerprint(true)?;
        input.recovery_inputs = recovery;
        input.template = DAILY_TEMPLATE_V3.into();
        input.input_fingerprint = fingerprint(
            &json!({"parent_sha256":parent,"plan":input.plan,"context":input.context}),
        )?;
        fs::write(&prepared, canonical_json(&serde_json::to_value(&input)?)?)?;
        let summary = json!({"status":"prepared","model_calls":0,"run_id":input.context.run_id,"parent_sha256":parent,"plan_version":input.plan.version,"sources":sources.len(),"excerpt_chars":sources.iter().map(|s|s.text.chars().count()).sum::<usize>(),"batches":input.plan.batches.len(),"requests":input.plan.batches.iter().map(|batch|Ok(json!({"step":batch.logical_step,"bytes":batch.bytes()?.len(),"max_tokens":batch.request["max_tokens"],"entries":batch.entries.len()}))).collect::<Result<Vec<Value>,ai_daily_report::ReportError>>()?});
        fs::write(root.join("preflight.json"), canonical_json(&summary)?)?;
        println!("{summary}");
    } else {
        let input: FrozenInput = serde_json::from_slice(&fs::read(prepared)?)?;
        let id = prepare_frozen(&app, &input, false, "授权验收：完整事实提取与分组成稿")?;
        let result = resume_frozen(&app, &id, now);
        let run = app.store.get(&id)?;
        let report: Value = run
            .report_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or(Value::Null);
        let calls = app.store.calls(&id)?;
        let summary = json!({"run_id":id,"status":run.status,"error_code":result.as_ref().err().map(|e|e.code),"quality":run.quality,"calls":calls.len(),"succeeded_calls":calls.iter().filter(|call|call["status"]=="succeeded").count(),"facts":report["facts"].as_array().map(Vec::len),"sources":report["sources"].as_array().map(|sources|sources.iter().map(|s|json!({"source_id":s["source_id"],"excerpt_chars":s["excerpt_chars"],"model_chars":s["model_chars"],"model_truncated":s["model_truncated"]})).collect::<Vec<_>>()),"gap_codes":report["gaps"].as_array().map(|gaps|gaps.iter().map(|gap|gap["code"].clone()).collect::<Vec<_>>())});
        fs::write(root.join("result.json"), canonical_json(&summary)?)?;
        println!("{summary}");
        let outcome = result?;
        fs::write(root.join("candidate.md"), outcome.markdown)?;
    }
    Ok(())
}
