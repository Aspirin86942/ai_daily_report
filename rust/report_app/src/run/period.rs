use super::{deadline_check, hash, repair_run, scan_stable, App};
use crate::{
    config::ModelPricing,
    model::{
        plan::{PlannedStep, StepKind},
        ModelRunner, StepOutput,
    },
    publish::Publisher,
    reports::{self, period},
    snapshots::{canonical_json, SnapshotRef},
    store::{locking::ReportLock, StagedReport},
    types::{Period, ReportOutcome, ReportRequest, ReportSource, RunContext, RunPhase},
    ReportError,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeriodInput {
    version: u32,
    context: RunContext,
    period: Period,
    quality: String,
    save: bool,
    step: PlannedStep,
    template: String,
    input_fingerprint: String,
    recovery_fingerprint: String,
    model_pricing: Option<ModelPricing>,
}

fn daily_context(reports: Vec<Value>) -> Vec<Value> {
    reports.into_iter().map(|report|{
        let mut data=json!({"date":report["date"]});
        for field in ["schema_version","report_kind","quality","completed_work","work_summary","next_plan","summary_items","verification_items","content","user_notes"] {
            if !report[field].is_null(){data[field]=report[field].clone();}
        }
        if let Some(gaps)=report["gaps"].as_array(){data["gaps"]=json!(gaps.iter().map(|gap|&gap["message"]).collect::<Vec<_>>());}
        if let Some(sources)=report["sources"].as_array(){data["sources"]=json!(sources.iter().map(|s|json!({"source_id":s["source_id"],"relative_path":s["relative_path"],"source_kind":s["source_kind"]})).collect::<Vec<_>>());}
        data
    }).collect()
}

pub fn run_period(mut request: ReportRequest, app: &App) -> Result<ReportOutcome, ReportError> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(app.config.recovery.run_budget_seconds);
    let now = request.context.started_at;
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs_f64(app.config.recovery.lock_wait_seconds),
    )?;
    let step_kind = period::kind(&request.period);
    let kind = if step_kind == StepKind::Weekly {
        "weekly"
    } else {
        "monthly"
    };
    request.context.config_fingerprint = app.fingerprint(false)?;
    app.store.recover_interrupted(now, &_lock)?;
    if request.save {
        for pending in app
            .store
            .pending_period(kind, &request.period.start.to_string())?
        {
            app.store.resume(&pending.run_id, now)?;
            let result = finish(app, &pending.run_id, now, started, deadline);
            if let Err(error) = &result {
                app.store.fail(&pending.run_id, error.code, false, now)?;
            }
            let recovered = result?;
            if request.input.trim().is_empty() {
                return Ok(recovered);
            }
        }
    }
    let id = &request.context.run_id;
    app.store.create(&request.context, kind, id, None, now)?;
    let result = (|| {
        app.store.set_scanning(id, now)?;
        deadline_check(deadline)?;
        let (reports, missing, file_context, sources, quality) =
            if request.period.source == ReportSource::Db {
                let (reports, missing) = app
                    .store
                    .reports_in_range(request.period.start, request.period.end)?;
                let partial = !missing.is_empty()
                    || reports
                        .iter()
                        .any(|r| r["quality"] == "partial" || r["quality"] == "unavailable");
                (
                    daily_context(reports),
                    missing,
                    String::new(),
                    vec![],
                    if partial { "partial" } else { "complete" },
                )
            } else {
                let scan = scan_stable(app, &request, deadline)?;
                if scan.envelope.status == ai_daily_scanner_contract::EngineStatus::Error {
                    return Err(ReportError::new("SCAN_UNAVAILABLE", "周月报扫描未能完成"));
                }
                let quality =
                    if scan.envelope.status == ai_daily_scanner_contract::EngineStatus::Partial {
                        "partial"
                    } else {
                        "complete"
                    };
                let sources = scan
                    .sources
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()?;
                (vec![], vec![], scan.envelope.file_context, sources, quality)
            };
        let target = json!({"week_label":request.period.label,"year_month":request.period.label,"date_range":format!("{} ~ {}",request.period.start,request.period.end),"data_source":if request.period.source==ReportSource::Db{"已保存日报"}else{"文件扫描"},"reports_summary":String::from_utf8(canonical_json(&json!({"reports":reports,"user_statement":request.input}))?).unwrap(),"missing_days":missing.join("、"),"file_context":file_context});
        let step = PlannedStep::period(&app.config.llm, step_kind, target)?;
        let fingerprint = hash(
            &json!({"version":3,"step":step,"source":request.period.source,"config":request.context.config_fingerprint}),
        )?;
        let payload = PeriodInput {
            version: 3,
            context: request.context.clone(),
            period: request.period.clone(),
            quality: quality.into(),
            save: request.save,
            step,
            template: if step_kind == StepKind::Weekly {
                period::WEEK_TEMPLATE
            } else {
                period::MONTH_TEMPLATE
            }
            .into(),
            input_fingerprint: fingerprint,
            recovery_fingerprint: app.fingerprint(true)?,
            model_pricing: app.config.recovery.model_pricing.clone(),
        };
        let reference = app
            .snapshots
            .put(id, "period-input", &serde_json::to_value(&payload)?)?;
        app.store
            .prepare(id, &reference, &payload.input_fingerprint, &sources, now)?;
        if request.save {
            if let Some(current) = app
                .store
                .current_period(kind, &request.period.label)?
                .filter(|r| r.input_fingerprint.as_deref() == Some(&payload.input_fingerprint))
            {
                let path = Publisher::new(&app.config.paths.reports_dir, &app.store)?
                    .publish(&current.run_id, now)?;
                app.store.finish_observation(id, &current, now)?;
                return Ok(ReportOutcome {
                    run_id: id.clone(),
                    target_date: current.target_date,
                    quality: current.quality.unwrap(),
                    markdown: current.markdown.unwrap(),
                    output_path: Some(path),
                    action: "reused".into(),
                    model_calls: 0,
                });
            }
        }
        finish(app, id, now, started, deadline)
    })();
    if let Err(error) = &result {
        app.store.fail(id, error.code, false, now)?;
    }
    result
}

fn finish(
    app: &App,
    id: &str,
    now: DateTime<Utc>,
    started: Instant,
    deadline: Instant,
) -> Result<ReportOutcome, ReportError> {
    let run = app.store.get(id)?;
    if run.phase == RunPhase::DbCommitted {
        return repair_run(app, id, now);
    }
    let reference = SnapshotRef {
        path: run
            .snapshot_path
            .ok_or_else(|| ReportError::new("SNAPSHOT_MISSING", "周月报输入缺失"))?,
        sha256: run
            .snapshot_sha256
            .ok_or_else(|| ReportError::new("SNAPSHOT_MISSING", "周月报输入哈希缺失"))?,
    };
    let payload: PeriodInput = serde_json::from_value(app.snapshots.read(&reference)?)?;
    payload.context.validate()?;
    if payload.version != 3
        || payload.context.run_id != id
        || run.context_json
            != String::from_utf8(canonical_json(&serde_json::to_value(&payload.context)?)?).unwrap()
        || run.input_fingerprint.as_deref() != Some(&payload.input_fingerprint)
    {
        return Err(ReportError::new(
            "SNAPSHOT_CONTEXT_MISMATCH",
            "周月报输入与运行记录不一致",
        ));
    }
    let same = run.config_fingerprint == app.fingerprint(false)?;
    if !same && payload.recovery_fingerprint != app.fingerprint(true)? {
        return Err(ReportError::new(
            "RECOVERY_CONFIG_CHANGED",
            "周月报业务配置已经改变",
        ));
    }
    let mut limits = app.config.recovery.clone();
    limits.model_pricing = payload.model_pricing.clone();
    let runner = ModelRunner {
        store: &app.store,
        snapshots: &app.snapshots,
        transport: app.transport.as_ref(),
        llm: &app.config.llm,
        recovery: &limits,
        recovery_inputs: None,
        allow_new_calls: same,
        run_id: id,
        started_at: now,
        started,
        deadline,
    };
    let StepOutput::Period(report) = runner.execute(&payload.step)?.output else {
        unreachable!()
    };
    let markdown = reports::render(
        &payload.template,
        &report,
        &payload
            .context
            .started_at
            .with_timezone(&crate::types::shanghai())
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
    )?;
    let publisher = Publisher::new(&app.config.paths.reports_dir, &app.store)?;
    let path = publisher.period_path(&run.run_kind, &report)?;
    let publication =
        hash(&json!({"input":payload.input_fingerprint,"template":payload.template}))?;
    app.store.stage_period(
        id,
        &StagedReport {
            report: &report,
            markdown: &markdown,
            output_path: path.to_str().unwrap(),
            publication_fingerprint: &publication,
        },
        &payload.quality,
        payload.save,
        now,
    )?;
    let output_path = if payload.save {
        Some(publisher.publish(id, now)?)
    } else {
        None
    };
    Ok(ReportOutcome {
        run_id: id.into(),
        target_date: run.target_date,
        quality: payload.quality,
        markdown,
        output_path,
        action: if payload.save { "published" } else { "preview" }.into(),
        model_calls: app.store.calls(id)?.len() as u64,
    })
}
