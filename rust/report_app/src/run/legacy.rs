use super::{hash, App};
use crate::{
    model::{
        legacy::{cached, LegacyBatch, LegacyItem},
        recovery::FrozenCall,
        slicing::{union_chars, TextRange},
    },
    publish::Publisher,
    reports::{
        self,
        data::{CoverageGap, ReportSource},
    },
    store::{RunRecord, StagedReport},
    types::{ReportOutcome, RunContext},
    ReportError,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn finish(
    app: &App,
    run: &RunRecord,
    payload: Value,
    now: DateTime<Utc>,
    started: std::time::Instant,
    deadline: std::time::Instant,
) -> Result<ReportOutcome, ReportError> {
    let context: RunContext = serde_json::from_value(payload["context"].clone())?;
    context.validate()?;
    let stored: RunContext = serde_json::from_str(&run.context_json)?;
    if serde_json::to_value(&context)? != serde_json::to_value(stored)?
        || payload["input_fingerprint"].as_str() != run.input_fingerprint.as_deref()
    {
        return Err(ReportError::new(
            "SNAPSHOT_CONTEXT_MISMATCH",
            "旧冻结输入与运行记录不一致",
        ));
    }
    let mut sources: Vec<ReportSource> = serde_json::from_value(payload["sources"].clone())?;
    for source in &sources {
        source.validate()?;
    }
    let known: BTreeSet<_> = sources.iter().map(|s| s.source_id.clone()).collect();
    let unverified: BTreeSet<_> = sources
        .iter()
        .filter(|s| s.unverified())
        .map(|s| s.source_id.clone())
        .collect();
    let batches: Vec<LegacyBatch> = serde_json::from_value(payload["batches"].clone())?;
    if batches.len() > 8 {
        return Err(ReportError::new("MODEL_PLAN_INVALID", "旧冻结批次数超限"));
    }
    let pins: BTreeMap<String, FrozenCall> = if payload["reused_calls"].is_null() {
        BTreeMap::new()
    } else {
        serde_json::from_value(payload["reused_calls"].clone())?
    };
    let job_id = payload["job_budget"]["job_id"].as_str();
    let mut calls = app.store.calls(&run.run_id)?;
    if let Some(prior) = payload["prior_calls"].as_array() {
        for pin in prior {
            let pin: FrozenCall = serde_json::from_value(pin.clone())?;
            calls.push(pin.read(&app.store, run, job_id)?.1);
        }
    }
    let mut summary: Vec<LegacyItem> = Vec::new();
    let mut pending = Vec::new();
    let mut ranges: BTreeMap<String, Vec<TextRange>> = BTreeMap::new();
    let mut effective = Vec::new();
    let before_calls = app.store.calls(&run.run_id)?.len();
    let bounded = batches.iter().all(|b| b.response_schema_version == 2);
    for batch in &batches {
        if batch.source_ids.iter().any(|id| !known.contains(id)) {
            return Err(ReportError::new(
                "MODEL_SOURCE_REFERENCE_INVALID",
                "旧请求引用了不存在的来源",
            ));
        }
        let cached_result = cached(
            &app.store,
            &app.snapshots,
            run,
            batch,
            pins.get(&batch.logical_step),
            job_id,
        );
        let (result, batch, row) = match cached_result {
            Ok(cached) => cached,
            Err(error)
                if error.code == "RECOVERY_CONFIG_CHANGED"
                    && payload["rust_recovery_version"] == 3 =>
            {
                if run.config_fingerprint != app.fingerprint(false)? {
                    return Err(error);
                }
                let recovery = crate::model::recovery::RecoveryInputs {
                    job_budget: if payload["job_budget"].is_null()
                        || payload["job_budget"] == json!({})
                    {
                        None
                    } else {
                        Some(serde_json::from_value(payload["job_budget"].clone())?)
                    },
                    reused_calls: BTreeMap::new(),
                    prior_calls: vec![],
                };
                let step = crate::model::plan::PlannedStep::legacy(batch, run, &app.config.llm)?;
                let runner = crate::model::ModelRunner {
                    store: &app.store,
                    snapshots: &app.snapshots,
                    transport: app.transport.as_ref(),
                    llm: &app.config.llm,
                    recovery: &app.config.recovery,
                    recovery_inputs: Some(&recovery),
                    allow_new_calls: true,
                    run_id: &run.run_id,
                    started_at: now,
                    started,
                    deadline,
                };
                let generated = runner.execute(&step)?;
                let crate::model::StepOutput::Legacy(result) = generated.output else {
                    unreachable!()
                };
                let batch: LegacyBatch =
                    serde_json::from_value(generated.effective.target["legacy_batch"].clone())?;
                let row = app
                    .store
                    .calls(&run.run_id)?
                    .into_iter()
                    .find(|r| r["call_id"] == generated.call_id)
                    .ok_or_else(|| ReportError::new("MODEL_CALL_MISSING", "模型调用记录缺失"))?;
                (result, batch, row)
            }
            Err(error) => return Err(error),
        };
        calls.push(row);
        if bounded {
            for entry in batch.entries()? {
                let id = entry["source_id"].as_str().unwrap().to_owned();
                if let Some(values) = entry["text_ranges"].as_array() {
                    ranges
                        .entry(id)
                        .or_default()
                        .extend(serde_json::from_value::<Vec<TextRange>>(json!(values))?);
                } else {
                    ranges.entry(id.clone()).or_default().push(TextRange {
                        start: 0,
                        end: *batch.source_chars.get(&id).unwrap(),
                    });
                }
            }
        }
        for item in result.summary_items {
            if item.source_ids.iter().any(|id| unverified.contains(id)) {
                pending.push(item);
            } else {
                summary.push(item);
            }
        }
        pending.extend(result.verification_items);
        effective.push(batch);
    }
    for items in [&mut summary, &mut pending] {
        let mut seen = BTreeSet::new();
        items.retain(|item| {
            let mut ids = item.source_ids.clone();
            ids.sort();
            seen.insert((item.text.clone(), ids))
        });
    }
    let mut gaps: Vec<CoverageGap> = serde_json::from_value(payload["gaps"].clone())?;
    let mut quality = payload["quality"]
        .as_str()
        .ok_or_else(|| ReportError::new("REPORT_DATA_INVALID", "旧报告质量缺失"))?
        .to_owned();
    if bounded && !batches.is_empty() {
        let identity = hash(&serde_json::to_value(&effective)?)?;
        for source in &mut sources {
            let successful = ranges.remove(&source.source_id).unwrap_or_default();
            let mut attempted = Vec::new();
            let mut unknown = false;
            for row in &calls {
                let usage: Value =
                    serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
                let entries =
                    &usage["reserved_budget"]["request_audit"]["source_ranges"][&source.source_id];
                if !entries.is_null() {
                    attempted.extend(serde_json::from_value::<Vec<TextRange>>(entries.clone())?);
                    unknown |= matches!(row["status"].as_str(), Some("unknown" | "reserved"));
                }
            }
            if successful
                .iter()
                .chain(&attempted)
                .any(|r| r.end > source.excerpt_chars || r.end <= r.start)
            {
                return Err(ReportError::new(
                    "MODEL_SOURCE_RANGE_MISMATCH",
                    "旧调用范围超过冻结来源",
                ));
            }
            let audit = &mut source.coverage_limits["evidence_audit"];
            if !audit.is_object() {
                *audit = json!({"version":1,"secondary_tags":[]});
            }
            audit["planned_model_chars"] = json!(source.model_chars);
            audit["attempted_model_chars"] = json!(union_chars(&attempted));
            audit["sent_unknown"] = json!(unknown);
            audit["effective_plan_sha256"] = json!(identity);
            let detail: Vec<_> = effective
                .iter()
                .flat_map(|batch| {
                    let entries = batch.entries().unwrap_or_default();
                    entries
                        .into_iter()
                        .filter(|e| e["source_id"] == source.source_id)
                        .flat_map(|e| {
                            e["text_ranges"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .map(|mut range| {
                                    range["logical_step"] = json!(batch.logical_step);
                                    range["revision"] = json!(batch.revision);
                                    range
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            audit["model_ranges"] = json!(detail);
            source.model_chars = union_chars(&successful);
            source.model_truncated = source.model_chars < source.excerpt_chars;
            if source.model_truncated
                && !gaps.iter().any(|g| {
                    g.code == "MODEL_INPUT_BOUNDED"
                        && g.source_id.as_deref() == Some(&source.source_id)
                })
            {
                gaps.push(CoverageGap::new(
                    "MODEL_INPUT_BOUNDED",
                    "model",
                    Some(&source.source_id),
                    "实际成功批次只覆盖部分摘录。",
                ));
            }
        }
        if !gaps.is_empty() {
            quality = "partial".into();
        }
    }
    let report = json!({"schema_version":2,"report_kind":"file_evidence","date":context.target_date,"scope_id":context.scope_id,"timezone":context.timezone,"window_start_utc":context.window_start_utc.to_rfc3339(),"window_end_utc":context.window_end_utc.to_rfc3339(),"quality":quality,"root_alias":payload["root_alias"],"sources":sources,"gaps":gaps,"summary_items":summary,"verification_items":pending,"generated_at":now.to_rfc3339(),"user_notes":payload["notes"],"missed_dates":payload["missed_dates"]});
    let template = payload["template"]
        .as_str()
        .ok_or_else(|| ReportError::new("TEMPLATE_INVALID", "旧冻结模板缺失"))?;
    let markdown = reports::legacy::render(&report, template)?;
    let output_path = if run.run_kind == "preview" {
        app.store
            .finish_preview(&run.run_id, &report, &markdown, now)?;
        None
    } else {
        let publisher = Publisher::new(&app.config.paths.reports_dir, &app.store)?;
        let path = publisher.output_path(&run.target_date)?;
        let publication = hash(
            &json!({"input_fingerprint":payload["input_fingerprint"],"template":template,"notes":payload["notes"],"missed_dates":payload["missed_dates"],"root_alias":payload["root_alias"]}),
        )?;
        app.store.stage_report(
            &run.run_id,
            &StagedReport {
                report: &report,
                markdown: &markdown,
                output_path: path.to_str().unwrap(),
                publication_fingerprint: &publication,
            },
            now,
        )?;
        Some(publisher.publish(&run.run_id, now)?)
    };
    Ok(ReportOutcome {
        run_id: run.run_id.clone(),
        target_date: run.target_date.clone(),
        quality,
        markdown,
        output_path,
        action: if run.run_kind == "preview" {
            "preview"
        } else {
            "published"
        }
        .into(),
        model_calls: (app.store.calls(&run.run_id)?.len() - before_calls) as u64,
    })
}
