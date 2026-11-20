use super::{App, PERSONAL_APP_BUDGET, PERSONAL_WORK_BUDGET};
use crate::{
    evidence::{denied_sources, permission_denied},
    model::{
        plan::PlannedStep,
        transport::{HttpResponse, ModelTransport, TransportFailure},
        ModelRunner,
    },
    processing_cache::check_deadline,
    publish::Publisher,
    reports::{
        data::CoverageGap,
        personal::{PersonalReport, Selection, MAX_REPORT_BYTES},
    },
    snapshots::{fingerprint, SnapshotRef, SnapshotStore},
    store::{locking::ReportLock, RunStatus, StagedReport},
    types::{ReportOutcome, ReportRequest, ReportSource, RunContext, RunPhase},
    ReportError,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
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

impl PersonalInput {
    fn fingerprint_fields(&self) -> serde_json::Value {
        let mut value = json!({
            "context": self.context, "selection": self.selection,
            "plan": self.plan, "template": self.template,
        });
        // Historical v5 snapshots used these four fields without a version tag.
        if self.version == 6 {
            value["version"] = json!(6);
        }
        value
    }

    fn save(&mut self, snapshots: &SnapshotStore, kind: &str) -> Result<SnapshotRef, ReportError> {
        let mut value = self.fingerprint_fields();
        self.input_fingerprint = fingerprint(&value)?;
        value["version"] = json!(self.version);
        value["input_fingerprint"] = json!(self.input_fingerprint);
        value["recovery_fingerprint"] = json!(self.recovery_fingerprint);
        snapshots.put(&self.context.run_id, kind, &value)
    }

    /// Rebuild the published report shape from a frozen input and its model outcome.
    pub fn report(
        &self,
        root_alias: &str,
        generated: Option<Result<crate::model::personal::PersonalOutput, ReportError>>,
    ) -> PersonalReport {
        let mut report = PersonalReport::from_selection(&self.context, root_alias, &self.selection);
        report.schema_version = self.version;
        if let Some(plan) = self.plan.as_ref().filter(|_| self.version == 6) {
            for source in &mut report.sources {
                let sent = plan.source_map.get(&source.source_id).is_some();
                source.model_chars = if sent { source.excerpt_chars } else { 0 };
                source.model_truncated = !sent;
            }
        }
        if let Some(result) = generated {
            match result {
                Ok(output) => {
                    report.items = output.items;
                    if let Some(code) = output.compaction_error {
                        report.gaps.push(CoverageGap::new(
                            code,
                            "model",
                            None,
                            "摘要精简未完成，已按完整事项保底减量并保留下一步计划。",
                        ));
                    }
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
        if report.items.is_empty() && !self.selection.materials.is_empty() {
            report.quality = "partial".into();
            report.gaps.push(CoverageGap::new(
                "PERSONAL_SUMMARY_UNAVAILABLE",
                "model",
                None,
                "尚无可用事项总结，不能据此确认工作成果。",
            ));
        }
        if self.version == 5 {
            report.retain_cited();
        }
        report
    }

    pub fn render_output(&self, report: &mut PersonalReport) -> Result<String, ReportError> {
        if self.version == 6 {
            report.render_detailed_output(&self.template)
        } else {
            render_legacy_output(report, self)
        }
    }

    fn prepare_plan(&mut self, llm: &crate::config::Llm) {
        let selection = &mut self.selection;
        match crate::model::personal::prepare_detailed_scan(llm, &self.context, selection) {
            Ok(plan) => {
                let sent = plan
                    .source_map
                    .as_object()
                    .map_or(0, |sources| sources.len());
                if sent < selection.materials.len() {
                    selection.gaps.push(CoverageGap::new(
                        "REQUEST_MATERIAL_BOUNDED",
                        "selection",
                        None,
                        &format!(
                            "已冻结 {} 个材料块，其中 {sent} 个进入模型请求，其余作为补充依据。",
                            selection.materials.len()
                        ),
                    ));
                }
                self.plan = Some(plan);
            }
            Err(e) => selection.gaps.push(CoverageGap::new(
                e.code,
                "model",
                None,
                "模型输入未能准备，本次保留有限材料说明。",
            )),
        }
    }
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

fn ensure_snapshot_space(app: &App, bytes: u64) -> Result<(), ReportError> {
    let session = app
        .processing
        .as_ref()
        .ok_or_else(|| ReportError::new("PROCESSING_LOCK_REQUIRED", "缺少处理会话"))?;
    // Admission is shared with the writer under the processing lock.
    drop(session.quota.reserve(bytes)?);
    Ok(())
}

pub fn run(
    mut request: ReportRequest,
    app: &App,
    started: Instant,
) -> Result<ReportOutcome, ReportError> {
    if request.period.source != ReportSource::Scan {
        return Err(ReportError::new(
            "PERSONAL_SOURCE_INVALID",
            "个人日报需要 scan 来源",
        ));
    }
    let deadline = started + PERSONAL_WORK_BUDGET;
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
            let result = snapshots(app).and_then(|store| {
                finish_frozen(app, &pending.run_id, started, deadline, &store, None)
            });
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
    app.store.create_personal(
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
    let result = (|| {
        app.store.set_scanning(id, Utc::now())?;
        let selection = super::personal_scan::prepare(app, &request)?;
        let kind = if request.dry_run || selection.materials.is_empty() {
            "personal-input"
        } else {
            "personal-materials"
        };
        let mut input = PersonalInput {
            version: 6,
            context: request.context.clone(),
            selection,
            plan: None,
            template: crate::reports::personal::DETAILED_SCAN_TEMPLATE.into(),
            input_fingerprint: String::new(),
            recovery_fingerprint: app.fingerprint(true)?,
        };
        let snapshots = snapshots(app)?;
        let reference = input.save(&snapshots, kind)?;
        // Commit all approved blocks and their source rows before selecting any model input.
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
        check_deadline(deadline)?;
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
        finish_frozen(
            app,
            id,
            started,
            deadline,
            &snapshots,
            Some((reference, input)),
        )
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
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs(5).min(deadline.saturating_duration_since(Instant::now())),
    )?;
    finish_frozen(app, id, started, deadline, &snapshots(app)?, None)
}

fn finish_frozen(
    app: &App,
    id: &str,
    started: Instant,
    deadline: Instant,
    snapshots: &SnapshotStore,
    just_saved: Option<(SnapshotRef, PersonalInput)>,
) -> Result<ReportOutcome, ReportError> {
    let run = app.store.get(id)?;
    let reference =
        SnapshotRef {
            path: run.snapshot_path.clone().ok_or_else(|| {
                ReportError::new("PERSONAL_INPUT_EXPIRED", "个人日报临时输入已过期")
            })?,
            sha256: run.snapshot_sha256.clone().ok_or_else(|| {
                ReportError::new("PERSONAL_INPUT_EXPIRED", "个人日报临时输入已过期")
            })?,
        };
    let mut input = match just_saved {
        Some((saved, input)) if saved == reference => input,
        _ => {
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
            if input.input_fingerprint != fingerprint(&input.fingerprint_fields())? {
                return Err(ReportError::new(
                    "SNAPSHOT_CONTEXT_MISMATCH",
                    "个人日报冻结输入指纹不符",
                ));
            }
            input
        }
    };
    if !matches!(input.version, 5 | 6)
        || input
            .plan
            .as_ref()
            .is_some_and(|plan| plan.version != input.version + 1)
        || input.context.run_id != id
        || run.input_fingerprint.as_deref() != Some(&input.input_fingerprint)
        || !app
            .config
            .matches_personal_fingerprint(&input.recovery_fingerprint, true)?
    {
        return Err(ReportError::new(
            "SNAPSHOT_CONTEXT_MISMATCH",
            "个人日报冻结输入或配置已改变",
        ));
    }
    if reference.path == format!("{id}/personal-materials-{}.json", reference.sha256) {
        if input.version != 6
            || input.plan.is_some()
            || run.phase != RunPhase::Prepared
            || !matches!(run.status, RunStatus::Running)
        {
            return Err(ReportError::new(
                "RUN_PHASE_INVALID",
                "当前状态不能为冻结材料选材",
            ));
        }
        if input.context.config_fingerprint != run.config_fingerprint
            || !app
                .config
                .matches_personal_fingerprint(&run.config_fingerprint, false)?
        {
            return Err(ReportError::new(
                "RECOVERY_CONFIG_CHANGED",
                "选材需要原运行配置",
            ));
        }
        check_deadline(deadline)?;
        input.prepare_plan(&app.config.llm);
        check_deadline(deadline)?;
        let planned = input.save(snapshots, "personal-input")?;
        app.store
            .freeze_personal_plan(&run, &planned, &input.input_fingerprint, Utc::now())?;
        // The database now owns the self-contained final input. An interrupted cleanup
        // leaves only a quota-tracked file with its original seven-day expiry.
        if let Some(session) = &app.processing {
            session.quota.remove(&snapshots.root.join(reference.path))?;
        }
    }
    let generated = if let Some(plan) = &input.plan {
        let transport = PersonalTransport {
            app,
            selection: &input.selection,
            plan,
            snapshot_bytes: 2 * plan.bytes()?.len() as u64
                + crate::snapshots::RESPONSE_RESERVATION_BYTES,
            deadline,
        };
        let runner = ModelRunner {
            store: &app.store,
            snapshots,
            transport: &transport,
            llm: &app.config.llm,
            allow_new_calls: app
                .config
                .matches_personal_fingerprint(&run.config_fingerprint, false)?,
            run_id: id,
            started_at: Utc::now()
                - chrono::Duration::from_std(started.elapsed()).unwrap_or_default(),
            started,
            deadline,
        };
        Some(crate::model::personal::generate(&runner, plan))
    } else {
        None
    };
    let mut report = input.report(&app.config.report.root_alias, generated);
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
    if input.version == 6 {
        let denied = denied_sources(&app.config, report.sources.iter())?;
        if !denied.is_empty() {
            if report
                .items
                .iter()
                .flat_map(|item| &item.source_ids)
                .any(|id| denied.contains(id))
            {
                return Err(permission_denied());
            }
            report
                .sources
                .retain(|source| !denied.contains(&source.source_id));
            report.evidence.retain(|id, _| !denied.contains(id));
            report.quality = "partial".into();
            report.gaps.push(CoverageGap::new(
                "PUBLISH_PERMISSION_CHANGED",
                "publish",
                None,
                &format!(
                    "{} 个补充材料块的使用权限已变更，发布时已省略。",
                    denied.len()
                ),
            ));
        }
    }
    let markdown = input.render_output(&mut report)?;
    check_deadline(started + PERSONAL_APP_BUDGET)?;
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
        (
            Some(publisher.publish_with_config(id, now, &app.config)?),
            "published",
        )
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
        (started + PERSONAL_APP_BUDGET).min(Instant::now() + Duration::from_secs(2)),
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

fn render_legacy_output(
    report: &mut PersonalReport,
    input: &PersonalInput,
) -> Result<String, ReportError> {
    loop {
        report.evidence_attachment = None;
        report.evidence_attachment = report.render_attachment(&input.template)?;
        if report.output_bytes("") <= MAX_REPORT_BYTES {
            let body = report.render(&input.template)?;
            if report.output_bytes(&body) <= MAX_REPORT_BYTES {
                return Ok(body);
            }
        }
        if report.items.len() <= 1 {
            return Err(ReportError::new(
                "PERSONAL_REPORT_TOO_LARGE",
                "完整事项及其依据超过报告上限，未发布不完整摘要",
            ));
        }
        if input.plan.as_ref().is_some_and(|plan| {
            matches!(plan.target["summary_prompt_version"].as_u64(), Some(4 | 6))
        }) {
            if !crate::model::personal::omit_lowest_priority(&mut report.items, &input.selection) {
                return Err(ReportError::new(
                    "PERSONAL_REPORT_TOO_LARGE",
                    "下一步计划及其完整依据超过报告上限，未删除计划或截断依据",
                ));
            }
        } else {
            report.items.pop();
        }
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
    }
}

struct PersonalTransport<'a> {
    app: &'a App,
    selection: &'a Selection,
    plan: &'a PlannedStep,
    snapshot_bytes: u64,
    deadline: Instant,
}
impl PersonalTransport<'_> {
    fn check(&self) -> Result<(), ReportError> {
        check_deadline(self.deadline)?;
        let sent = self
            .selection
            .materials
            .iter()
            .map(|m| &m.source)
            .filter(|s| self.plan.source_map.get(&s.source_id).is_some());
        if denied_sources(&self.app.config, sent)?.is_empty() {
            Ok(())
        } else {
            Err(permission_denied())
        }
    }
}

impl ModelTransport for PersonalTransport<'_> {
    fn prepare(&self) -> Result<(), ReportError> {
        self.check()?;
        ensure_snapshot_space(self.app, self.snapshot_bytes)?;
        self.app.transport.prepare()
    }
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        self.check()
            .map_err(|_| TransportFailure::PermissionDenied)?;
        self.app.transport.send(request, timeout)
    }
}
