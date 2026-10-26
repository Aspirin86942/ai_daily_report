use super::{
    deadline_check, finish, finish_guarded, input, reuse_report, scan_stable, stage, App,
    FrozenInput,
};
use crate::{
    reports::data::DailyReport,
    types::{ReportOutcome, ReportRequest},
    ReportError,
};
use chrono::{DateTime, Utc};
use std::time::Instant;

pub(super) fn resume_pending(
    app: &App,
    request: &ReportRequest,
    now: DateTime<Utc>,
    started: Instant,
    deadline: Instant,
) -> Result<Option<ReportOutcome>, ReportError> {
    if request.save && !request.dry_run && request.regeneration_reason.is_none() {
        let target = request.context.target_date.to_string();
        let pending = app.store.pending(if request.scheduled {
            None
        } else {
            Some(&target)
        })?;
        for pending in crate::archive::model_input::recovery_runs(
            &app.config,
            pending,
            request.period.source == crate::types::ReportSource::Archive,
        )? {
            app.store.resume(&pending.run_id, now)?;
            let recovered = finish_guarded(app, &pending.run_id, now, started, deadline)?;
            if pending.target_date == target && request.input.trim().is_empty() {
                return Ok(Some(recovered));
            }
        }
    }
    Ok(None)
}

pub(super) fn existing_archive(
    app: &App,
    request: &ReportRequest,
) -> Result<Option<ReportOutcome>, ReportError> {
    if request.period.source == crate::types::ReportSource::Archive
        && request.save
        && !request.dry_run
        && request.regeneration_reason.is_none()
    {
        if let Some(current) = app
            .store
            .current(&request.context.target_date.to_string())?
        {
            crate::archive::model_input::published(
                &app.config,
                &current.run_id,
                current.markdown.as_deref().unwrap_or(""),
            )?;
            return Ok(Some(ReportOutcome {
                run_id: current.run_id,
                target_date: current.target_date,
                quality: current.quality.unwrap_or_else(|| "complete".into()),
                markdown: current.markdown.unwrap_or_default(),
                output_path: current.output_path.map(Into::into),
                action: "existing".into(),
                model_calls: 0,
            }));
        }
    }
    Ok(None)
}

enum PreparedDaily {
    Input(Box<FrozenInput>),
    Preview(ReportOutcome),
}

fn prepare(
    app: &App,
    request: &ReportRequest,
    now: DateTime<Utc>,
    deadline: Instant,
) -> Result<PreparedDaily, ReportError> {
    let run_id = &request.context.run_id;
    let payload = if request.period.source == crate::types::ReportSource::Archive {
        let payload = input::prepare_archive(app, request)?;
        if request.dry_run {
            let reference =
                app.snapshots
                    .put(run_id, "archive-preview", &serde_json::to_value(&payload)?)?;
            app.store.prepare(
                run_id,
                &reference,
                &payload.input_fingerprint,
                &payload
                    .sources
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>, _>>()?,
                now,
            )?;
            app.store
                .finish_scan_preview(run_id, &payload.quality, now)?;
            return Ok(PreparedDaily::Preview(ReportOutcome {
                run_id: run_id.clone(),
                target_date: request.context.target_date.to_string(),
                quality: "scan_only".into(),
                markdown: serde_json::to_string_pretty(&payload.sources)?,
                output_path: None,
                action: "dry_run".into(),
                model_calls: 0,
            }));
        }
        payload
    } else {
        let scan = scan_stable(app, request, deadline)?;
        if request.dry_run {
            let metadata = serde_json::to_value(&scan)?;
            let reference = app.snapshots.put(run_id, "scan-preview", &metadata)?;
            let sources = scan
                .sources
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            app.store
                .prepare(run_id, &reference, &reference.sha256, &sources, now)?;
            app.store.finish_scan_preview(
                run_id,
                if scan.envelope.status == ai_daily_scanner_contract::EngineStatus::Ok {
                    "complete"
                } else {
                    "partial"
                },
                now,
            )?;
            return Ok(PreparedDaily::Preview(ReportOutcome {
                run_id: run_id.clone(),
                target_date: request.context.target_date.to_string(),
                quality: "scan_only".into(),
                markdown: scan.envelope.file_context,
                output_path: None,
                action: "dry_run".into(),
                model_calls: 0,
            }));
        }
        input::prepare(app, request, &scan)?
    };
    Ok(PreparedDaily::Input(Box::new(payload)))
}

pub(super) fn run_new(
    app: &App,
    request: &ReportRequest,
    now: DateTime<Utc>,
    started: Instant,
    deadline: Instant,
) -> Result<ReportOutcome, ReportError> {
    let run_id = &request.context.run_id;
    app.store.set_scanning(run_id, now)?;
    deadline_check(deadline)?;
    let payload = match prepare(app, request, now, deadline)? {
        PreparedDaily::Input(input) => input,
        PreparedDaily::Preview(outcome) => return Ok(outcome),
    };
    crate::archive::model_input::pin(&app.config, &payload)?;
    if payload.version == 4 {
        crate::archive::stats::record_stage(
            &app.config,
            run_id,
            &payload.context.target_date.to_string(),
            "input_preparation",
            started.elapsed().as_millis() as u64,
        )?;
    }
    let reference = app
        .snapshots
        .put(run_id, "input", &serde_json::to_value(&payload)?)?;
    let sources = payload
        .sources
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    if payload.quality == "unavailable" {
        app.store.save_scan_failure(run_id, &reference, &sources)?;
        return Err(ReportError::new(
            "SCAN_UNAVAILABLE",
            "现有材料无法支持生成报告，覆盖诊断已保存",
        ));
    }
    app.store.prepare(
        run_id,
        &reference,
        &payload.input_fingerprint,
        &sources,
        now,
    )?;
    if request.save && request.regeneration_reason.is_none() {
        if let Some(current) = app
            .store
            .current(&request.context.target_date.to_string())?
            .filter(|r| r.input_fingerprint.as_deref() == Some(&payload.input_fingerprint))
        {
            if current.publication_fingerprint.as_deref()
                == Some(&payload.publication_fingerprint()?)
            {
                return reuse_report(app, run_id, current, now);
            }
            let mut report: DailyReport = serde_json::from_str(
                current
                    .report_json
                    .as_deref()
                    .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "正式报告正文缺失"))?,
            )?;
            report.user_notes = payload.notes.clone();
            report.root_alias = payload.root_alias.clone();
            report.missed_dates = payload.missed_dates.clone();
            return stage(app, run_id, &payload, &report, now);
        }
    }
    finish(app, run_id, now, started, deadline)
}
