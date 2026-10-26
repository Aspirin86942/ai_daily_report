use super::App;
use crate::{
    archive::model_input::permitted,
    model::{
        plan::PlannedStep,
        transport::{HttpResponse, ModelTransport, TransportFailure},
        ModelRunner, StepOutput,
    },
    processing_cache::check_deadline,
    publish::Publisher,
    reports::{
        data::CoverageGap,
        personal::{PersonalReport, Selection, MAX_REPORT_BYTES, TEMPLATE},
    },
    snapshots::{fingerprint, SnapshotRef, SnapshotStore},
    store::{locking::ReportLock, StagedReport},
    types::{ReportOutcome, ReportRequest, ReportSource, RunContext, RunPhase},
    ReportError,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalInput {
    pub version: u32,
    pub context: RunContext,
    pub selection: Selection,
    pub plan: Option<PlannedStep>,
    pub template: String,
    pub input_fingerprint: String,
    pub recovery_fingerprint: String,
}

pub(crate) fn snapshots(app: &App) -> Result<SnapshotStore, ReportError> {
    let session = app.processing.as_ref().ok_or_else(|| {
        ReportError::new("RECOVERY_CONFIG_CHANGED", "个人日报恢复需要 personal 配置")
    })?;
    SnapshotStore::with_quota(
        session.quota.root.join("snapshots"),
        crate::processing_cache::MAX_BYTES,
        Arc::clone(&session.quota),
    )
}

fn ensure_snapshot_space(
    app: &App,
    end: &str,
    bytes: u64,
    deadline: Instant,
) -> Result<(), ReportError> {
    let session = app
        .processing
        .as_ref()
        .ok_or_else(|| ReportError::new("PROCESSING_LOCK_REQUIRED", "缺少处理会话"))?;
    if session.quota.available_bytes()? < bytes {
        let mut store = session.open_archive(&app.config)?;
        let now = session.quota.now();
        let protected =
            crate::archive::demand::protected(&app.config, &store, Some(end), now, deadline)?;
        crate::archive::retention::reclaim(&mut store, bytes, &protected, now, deadline)?;
    }
    // Exercise the same admission check as the writer; the processing lock
    // prevents another collector from consuming this space before its write.
    drop(session.quota.reserve(bytes)?);
    Ok(())
}

pub fn run(
    mut request: ReportRequest,
    app: &App,
    started: Instant,
) -> Result<ReportOutcome, ReportError> {
    if request.period.source != ReportSource::Archive {
        return Err(ReportError::new(
            "PERSONAL_SOURCE_INVALID",
            "个人日报需要 archive 采集来源",
        ));
    }
    let deadline = started + Duration::from_secs(270);
    check_deadline(deadline)?;
    let lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs(5).min(deadline.saturating_duration_since(Instant::now())),
    )?;
    app.store
        .recover_interrupted(request.context.started_at, &lock)?;
    request.context.config_fingerprint = app.fingerprint(false)?;
    let date = request.context.target_date.to_string();
    if request.regeneration_reason.is_some() && request.input.is_empty() {
        if let Some(current) = app.store.current(&date)? {
            if let Some(report) = current.report_json.as_deref() {
                let report: serde_json::Value = serde_json::from_str(report)?;
                request.input = report["user_notes"].as_str().unwrap_or("").to_owned();
            }
        }
    }
    if request.save && !request.dry_run && request.regeneration_reason.is_none() {
        if let Some(current) = app.store.current(&date)? {
            let mut result = super::repair_run(app, &current.run_id, Utc::now())?;
            result.action = "existing".into();
            return Ok(result);
        }
        for pending in app.store.pending(Some(&date))? {
            if !app.store.is_personal_run(&pending.run_id)? {
                continue;
            }
            if pending.phase == RunPhase::DbCommitted {
                return super::repair_run(app, &pending.run_id, Utc::now());
            }
            app.store.resume(&pending.run_id, Utc::now())?;
            let result = finish(app, &pending.run_id, started, deadline);
            if let Err(e) = &result {
                app.store.fail(&pending.run_id, e.code, false, Utc::now())?;
            }
            return result;
        }
    }
    if request.scheduled {
        app.store.activate(request.context.target_date)?;
    }
    let id = &request.context.run_id;
    app.store.create(
        &request.context,
        if request.save && !request.dry_run {
            "report"
        } else {
            "preview"
        },
        id,
        request.regeneration_reason.as_deref(),
        request.context.started_at,
    )?;
    app.store.register_personal_run(id)?;
    let result = (|| {
        app.store.set_scanning(id, Utc::now())?;
        let preparation = crate::archive::queue::prepare_personal(
            &app.config,
            app.processing
                .as_ref()
                .ok_or_else(|| ReportError::new("PROCESSING_LOCK_REQUIRED", "缺少处理会话"))?,
            request.context.target_date,
            Utc::now(),
            deadline,
        );
        if request
            .regeneration_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("automatic_personal_recovery:"))
        {
            let ready = preparation
                .as_ref()
                .map_err(|e| ReportError::new(e.code, e.message.clone()))?;
            if ready["queued"].as_u64().unwrap_or(0) > 0
                || ready["unavailable"].as_u64().unwrap_or(0) > 0
            {
                return Err(ReportError::new(
                    "PERSONAL_RECOVERY_WAITING",
                    "历史日报材料仍在准备，保留原报告并等待后续重试",
                ));
            }
        }
        let mut selection =
            crate::archive::selection::prepare_at(&app.config, &request, deadline, Utc::now())?;
        if let Err(e) = preparation {
            selection.gaps.push(CoverageGap::new(
                e.code,
                "parse",
                None,
                "补解析未完成，仅使用当前可用材料。",
            ));
        }
        let plan = if selection.materials.is_empty() || request.dry_run {
            None
        } else {
            match crate::model::personal::prepare(&app.config.llm, &request.context, &mut selection)
            {
                Ok(plan) => Some(plan),
                Err(e) => {
                    selection.gaps.push(CoverageGap::new(
                        e.code,
                        "model",
                        None,
                        "模型输入未能准备，本次保留有限材料说明。",
                    ));
                    None
                }
            }
        };
        let input_fingerprint = fingerprint(
            &json!({"context":request.context,"selection":selection,"plan":plan,"template":TEMPLATE}),
        )?;
        let input = PersonalInput {
            version: 5,
            context: request.context.clone(),
            selection,
            plan,
            template: TEMPLATE.into(),
            input_fingerprint,
            recovery_fingerprint: app.fingerprint(true)?,
        };
        let snapshots = snapshots(app)?;
        let bytes = crate::snapshots::canonical_json(&serde_json::to_value(&input)?)?;
        ensure_snapshot_space(
            app,
            &input.context.window_end_utc.to_rfc3339(),
            2 * bytes.len() as u64,
            deadline,
        )?;
        let reference = snapshots.put_bytes(id, "personal-input", &bytes)?;
        app.store.prepare(
            id,
            &reference,
            &input.input_fingerprint,
            &input
                .selection
                .materials
                .iter()
                .map(|m| serde_json::to_value(&m.source))
                .collect::<Result<Vec<_>, _>>()?,
            Utc::now(),
        )?;
        if request.dry_run {
            app.store.finish_scan_preview(id, "partial", Utc::now())?;
            return Ok(ReportOutcome {
                run_id: id.clone(),
                target_date: date,
                quality: "scan_only".into(),
                markdown: serde_json::to_string_pretty(&input.selection)?,
                output_path: None,
                action: "dry_run".into(),
                model_calls: 0,
            });
        }
        finish(app, id, started, deadline)
    })();
    if let Err(e) = &result {
        app.store.fail(id, e.code, false, Utc::now())?;
    }
    result
}

pub fn finish(
    app: &App,
    id: &str,
    started: Instant,
    deadline: Instant,
) -> Result<ReportOutcome, ReportError> {
    let run = app.store.get(id)?;
    let snapshots = snapshots(app)?;
    let reference =
        SnapshotRef {
            path: run.snapshot_path.clone().ok_or_else(|| {
                ReportError::new("PERSONAL_INPUT_EXPIRED", "个人日报临时输入已过期")
            })?,
            sha256: run.snapshot_sha256.clone().ok_or_else(|| {
                ReportError::new("PERSONAL_INPUT_EXPIRED", "个人日报临时输入已过期")
            })?,
        };
    let value = snapshots.read(&reference).map_err(|e| {
        if e.code == "SNAPSHOT_MISSING" {
            ReportError::new("PERSONAL_INPUT_EXPIRED", "个人日报临时输入已过期")
        } else {
            e
        }
    })?;
    let input: PersonalInput = serde_json::from_value(value)?;
    input.context.validate()?;
    input.selection.validate()?;
    if input.version != 5
        || input.context.run_id != id
        || run.input_fingerprint.as_deref() != Some(&input.input_fingerprint)
        || input.input_fingerprint
            != fingerprint(
                &json!({"context":input.context,"selection":input.selection,"plan":input.plan,"template":input.template}),
            )?
        || input.recovery_fingerprint != app.fingerprint(true)?
    {
        return Err(ReportError::new(
            "SNAPSHOT_CONTEXT_MISMATCH",
            "个人日报冻结输入或配置已改变",
        ));
    }
    let mut report = PersonalReport::from_selection(
        &input.context,
        &app.config.report.root_alias,
        &input.selection,
    );
    if let Some(plan) = &input.plan {
        let transport = PersonalTransport {
            app,
            selection: &input.selection,
            end: input.context.window_end_utc.to_rfc3339(),
            snapshot_bytes: 2 * plan.bytes()?.len() as u64
                + crate::snapshots::RESPONSE_RESERVATION_BYTES,
            deadline,
        };
        let runner = ModelRunner {
            store: &app.store,
            snapshots: &snapshots,
            transport: &transport,
            llm: &app.config.llm,
            recovery_inputs: None,
            allow_new_calls: run.config_fingerprint == app.fingerprint(false)?,
            run_id: id,
            started_at: Utc::now()
                - chrono::Duration::from_std(started.elapsed()).unwrap_or_default(),
            started,
            deadline,
        };
        match runner.execute(plan) {
            Ok(result) => {
                let StepOutput::Personal(output) = result.output else {
                    return Err(ReportError::new(
                        "MODEL_RESPONSE_INVALID",
                        "个人日报响应类型不符",
                    ));
                };
                report.items = output.items;
                if output.omitted > 0 {
                    report.gaps.push(CoverageGap::new(
                        "PERSONAL_OUTPUT_REDUCED",
                        "model",
                        None,
                        "部分完整事项因篇幅限制未展开。",
                    ));
                }
                report.quality = if report.gaps.is_empty() && !report.selection.partial() {
                    "complete"
                } else {
                    "partial"
                }
                .into();
            }
            Err(e) => {
                report.quality = "partial".into();
                report.gaps.push(CoverageGap::new(
                    e.code,
                    "model",
                    None,
                    "模型未形成有效总结，用户原文及材料范围已保留。",
                ));
            }
        }
    }
    if report.items.is_empty() && !input.selection.materials.is_empty() {
        report.quality = "partial".into();
        report.gaps.push(CoverageGap::new(
            "PERSONAL_SUMMARY_UNAVAILABLE",
            "model",
            None,
            "尚无可用事项总结，不能据此确认工作成果。",
        ));
    }
    report.retain_cited();
    if report.items.is_empty() {
        if let Some(current) = app.store.current(&run.target_date)? {
            if current.run_id != id {
                let previous: serde_json::Value =
                    serde_json::from_str(current.report_json.as_deref().unwrap_or("{}"))?;
                if previous["items"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    return Err(ReportError::new(
                        "PERSONAL_RECOVERY_INCOMPLETE",
                        "恢复未形成有效总结，保留已发布的有效内容",
                    ));
                }
            }
        }
    }
    let markdown = loop {
        report.evidence_attachment = None;
        report.evidence_attachment = report.render_attachment(&input.template)?;
        if report.output_bytes("") <= MAX_REPORT_BYTES {
            let body = report.render(&input.template)?;
            if report.output_bytes(&body) <= MAX_REPORT_BYTES {
                break body;
            }
        }
        if report.items.len() <= 1 {
            return Err(ReportError::new(
                "PERSONAL_REPORT_TOO_LARGE",
                "完整事项及其依据超过报告上限，未发布不完整摘要",
            ));
        }
        report.items.pop();
        report.quality = "partial".into();
        if !report
            .gaps
            .iter()
            .any(|g| g.code == "PERSONAL_OUTPUT_REDUCED")
        {
            report.gaps.push(CoverageGap::new(
                "PERSONAL_OUTPUT_REDUCED",
                "publish",
                None,
                "部分完整事项及依据因报告大小限制未展开。",
            ));
        }
        report.retain_cited();
    };
    check_deadline(started + Duration::from_secs(300))?;
    let value = serde_json::to_value(&report)?;
    let now = Utc::now();
    let (output_path, action) = if run.run_kind == "preview" {
        app.store.finish_preview(id, &value, &markdown, now)?;
        (None, "preview")
    } else {
        let publisher = Publisher::new(&app.config.paths.reports_dir, &app.store)?;
        let path = publisher.output_path(&report.date)?;
        app.store.stage_report(id,&StagedReport {
            report:&value,markdown:&markdown,output_path:&super::path_text(&path)?,
            publication_fingerprint:&fingerprint(&json!({"input":input.input_fingerprint,"report":value,"template":input.template}))?,
        },now)?;
        (Some(publisher.publish(id, now)?), "published")
    };
    let markdown = if run.run_kind == "preview" {
        report.render_preview(&input.template)?
    } else {
        markdown
    };
    let model_calls = app.store.calls(id)?.len() as u64;
    app.store.compact_personal_until(
        now,
        &snapshots.root,
        (started + Duration::from_secs(300)).min(Instant::now() + Duration::from_secs(2)),
    )?;
    Ok(ReportOutcome {
        run_id: id.into(),
        target_date: report.date,
        quality: report.quality,
        markdown,
        output_path,
        action: action.into(),
        model_calls,
    })
}

struct PersonalTransport<'a> {
    app: &'a App,
    selection: &'a Selection,
    end: String,
    snapshot_bytes: u64,
    deadline: Instant,
}
impl PersonalTransport<'_> {
    fn check(&self) -> Result<(), ReportError> {
        let current = crate::config::AppConfig::load(
            &self.app.config.program_dir,
            Some(&self.app.config.config_dir),
        )?;
        for material in &self.selection.materials {
            if material.source.source_kind == "file" {
                let path = material.source.coverage_limits["personal"]["absolute_path"]
                    .as_str()
                    .ok_or_else(|| {
                        ReportError::new("ARCHIVE_MODEL_PERMISSION_DENIED", "缺少来源权限信息")
                    })?;
                if !permitted(&self.app.config, Path::new(path))
                    || !permitted(&current, Path::new(path))
                {
                    return Err(ReportError::new(
                        "ARCHIVE_MODEL_PERMISSION_DENIED",
                        "来源当前未获发送许可",
                    ));
                }
            }
        }
        Ok(())
    }
}
impl ModelTransport for PersonalTransport<'_> {
    fn prepare(&self) -> Result<(), ReportError> {
        self.check()?;
        ensure_snapshot_space(self.app, &self.end, self.snapshot_bytes, self.deadline)?;
        self.app.transport.prepare()
    }
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        self.check()
            .map_err(|_| TransportFailure::PermissionDenied)?;
        self.app.transport.send(request, timeout)
    }
}
