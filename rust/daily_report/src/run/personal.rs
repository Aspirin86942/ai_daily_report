use super::{App, PERSONAL_APP_BUDGET, PERSONAL_WORK_BUDGET};
use crate::{
    evidence::{denied_sources, permission_denied},
    infra::{
        fs::reject_redirects,
        json::{fingerprint, read_frozen_report_json},
        locking::ReportLock,
    },
    model::{
        personal::{
            self as model, PersonalPrompts, PublicationRules, COMPACT_PROMPT_V8, SCAN_PROMPT_V9,
        },
        plan::PlannedStep,
        transport::{HttpResponse, ModelTransport, TransportFailure},
        ModelRunner,
    },
    processing_cache::check_deadline,
    publish::Publisher,
    reports::{
        data::CoverageGap,
        personal::{
            material_labels, EvidenceAttachment, PersonalReport, Selection, MAX_BODY_CHARS,
            MAX_REPORT_BYTES, SCAN_TEMPLATE_V7,
        },
    },
    snapshots::{SnapshotRef, SnapshotStore},
    store::{RunStatus, StagedReport},
    types::{ReportOutcome, ReportRequest, ReportSource, RunContext, RunPhase},
    ReportError,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::ErrorKind,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

const BOUNDED_INPUT_VERSION: u32 = 7;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalInput {
    pub version: u32,
    pub context: RunContext,
    pub selection: Selection,
    pub plan: Option<PlannedStep>,
    pub template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompts: Option<PersonalPrompts>,
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
        if self.version >= 6 {
            value["version"] = json!(self.version);
        }
        if let Some(prompts) = &self.prompts {
            value["prompts"] = json!(prompts);
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
        report.schema_version = self.version.min(6);
        if let Some(plan) = self.plan.as_ref().filter(|_| self.version >= 6) {
            for source in &mut report.sources {
                let sent = plan.source_map.get(&source.source_id).is_some();
                source.model_chars = if sent { source.excerpt_chars } else { 0 };
                source.model_truncated = !sent;
            }
        }
        let summarized = matches!(generated, Some(Ok(_)));
        let mut gap = |code, message| {
            report
                .gaps
                .push(CoverageGap::new(code, "model", None, message))
        };
        match generated {
            Some(Ok(output)) => {
                report.items = output.items;
                if let Some(code) = output.compaction_error {
                    gap(
                        code,
                        "摘要精简未完成，已按完整事项保底减量并保留下一步计划。",
                    );
                }
                if output.omitted > 0 {
                    gap("PERSONAL_OUTPUT_REDUCED", "部分完整事项因篇幅限制未展开。");
                }
            }
            Some(Err(e)) => gap(e.code, "模型未形成有效总结，用户原文及材料范围已保留。"),
            None => {}
        }
        if report.items.is_empty() && !self.selection.materials.is_empty() {
            gap(
                "PERSONAL_SUMMARY_UNAVAILABLE",
                "尚无可用事项总结，不能据此确认工作成果。",
            );
        }
        if self.version == BOUNDED_INPUT_VERSION && self.plan.is_some() {
            let attaching = report
                .sources
                .iter()
                .filter(|source| source.model_truncated)
                .count();
            if attaching > 0 {
                report.gaps.push(CoverageGap::new(
                    "REQUEST_MATERIAL_BOUNDED",
                    "selection",
                    None,
                    &format!("{attaching} 个已采纳材料块因模型请求容量限额仅写入完整依据。"),
                ));
            }
        }
        if !report.gaps.is_empty() {
            report.quality = "partial".into();
        } else if summarized && !report.selection.partial() {
            report.quality = "complete".into();
        }
        if self.version == 5 {
            report.retain_cited();
        }
        report
    }

    pub fn render_output(&self, report: &mut PersonalReport) -> Result<String, ReportError> {
        self.render_with_attachment(report, None)
    }

    fn render_with_attachment(
        &self,
        report: &mut PersonalReport,
        attachment: Option<EvidenceAttachment>,
    ) -> Result<String, ReportError> {
        match self.version {
            BOUNDED_INPUT_VERSION => {
                report.evidence_attachment = Some(match attachment {
                    Some(attachment) => attachment,
                    None => self.bounded_attachment(&report.root_alias)?,
                });
                report.render_bounded_output(&self.template, false)
            }
            6 => report.render_detailed_output(&self.template),
            _ => render_legacy_output(report, self),
        }
    }

    fn bounded_attachment(&self, root_alias: &str) -> Result<EvidenceAttachment, ReportError> {
        PersonalReport::from_selection(&self.context, root_alias, &self.selection)
            .prepare_bounded_attachment(&self.template)
    }

    fn publication_rules(
        &self,
        attachment: &EvidenceAttachment,
    ) -> Result<PublicationRules, ReportError> {
        Ok(PublicationRules {
            prompts: self
                .prompts
                .clone()
                .ok_or_else(|| snapshot_mismatch("冻结输入缺少活动提示词"))?,
            template: self.template.clone(),
            evidence_link: attachment.link(&self.context.target_date.to_string()),
            labels: material_labels(self.selection.sources()),
            max_chars: MAX_BODY_CHARS,
        })
    }

    fn prepare_plan(
        &mut self,
        llm: &crate::config::Llm,
        publication: Option<&PublicationRules>,
    ) -> Result<(), ReportError> {
        match model::prepare_scan(llm, &self.context, &self.selection, publication) {
            Ok(plan) => {
                let total = self.selection.materials.len();
                let sent = plan
                    .source_map
                    .as_object()
                    .map_or(0, |sources| sources.len());
                if publication.is_none() && sent < total {
                    self.selection.gaps.push(CoverageGap::new(
                        "REQUEST_MATERIAL_BOUNDED",
                        "selection",
                        None,
                        &format!("已冻结 {total} 个材料块，其中 {sent} 个进入模型请求，其余作为补充依据。"),
                    ));
                }
                self.plan = Some(plan);
            }
            Err(e) if publication.is_none() => self.selection.gaps.push(CoverageGap::new(
                e.code,
                "model",
                None,
                "模型输入未能准备，本次保留有限材料说明。",
            )),
            Err(e) => return Err(e),
        }
        Ok(())
    }
}

fn snapshot_mismatch(message: &str) -> ReportError {
    ReportError::new("SNAPSHOT_CONTEXT_MISMATCH", message)
}

fn read_template(config_dir: &Path, name: &str, default: &str) -> Result<String, ReportError> {
    let path = config_dir.join("templates").join(name);
    reject_redirects(&path, "TEMPLATE_PATH_REDIRECTED")?;
    match fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => Ok(text),
        Ok(_) => Err(ReportError::new(
            "TEMPLATE_OVERRIDE_INVALID",
            format!("{name} 为空"),
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(default.into()),
        Err(error) => Err(ReportError::new(
            "TEMPLATE_OVERRIDE_INVALID",
            format!("{name}：{error}"),
        )),
    }
}

pub(super) fn new_input(
    app: &App,
    context: RunContext,
    selection: Selection,
) -> Result<PersonalInput, ReportError> {
    let config_dir = &app.config.config_dir;
    Ok(PersonalInput {
        version: BOUNDED_INPUT_VERSION,
        context,
        selection,
        plan: None,
        template: read_template(config_dir, "daily_render_template.md", SCAN_TEMPLATE_V7)?,
        prompts: Some(PersonalPrompts {
            summary: read_template(config_dir, "daily_prompt.md", SCAN_PROMPT_V9)?,
            compaction: read_template(config_dir, "daily_compact_prompt.md", COMPACT_PROMPT_V8)?,
        }),
        input_fingerprint: String::new(),
        recovery_fingerprint: app.fingerprint(true)?,
    })
}

pub(crate) fn prepare_model_check(
    app: &App,
    context: &RunContext,
    selection: Selection,
) -> Result<PlannedStep, ReportError> {
    let input = new_input(app, context.clone(), selection)?;
    let attachment = input.bounded_attachment(&app.config.report.root_alias)?;
    model::prepare_scan(
        &app.config.llm,
        &input.context,
        &input.selection,
        Some(&input.publication_rules(&attachment)?),
    )
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
    let lock = lock_until(app, deadline)?;
    app.store
        .recover_interrupted(request.context.started_at, &lock)?;
    request.context.config_fingerprint = app.fingerprint(false)?;
    let date = request.context.target_date.to_string();
    if request.regeneration_reason.is_some() && request.input.is_empty() {
        if let Some(report) = app.store.current(&date)?.and_then(|r| r.report_json) {
            let report = read_frozen_report_json(&report)?;
            request.input = report["user_notes"].as_str().unwrap_or("").to_owned();
        }
    }
    let save = request.save && !request.dry_run;
    if save && request.regeneration_reason.is_none() {
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
        if save { "report" } else { "preview" },
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
        let mut input = new_input(app, request.context.clone(), selection)?;
        if request.dry_run {
            check_deadline(deadline)?;
            if let Err(error) = input.bounded_attachment(&app.config.report.root_alias) {
                if error.code != "PERSONAL_REPORT_TOO_LARGE" {
                    return Err(error);
                }
                input.selection.gaps.push(CoverageGap::new(
                    error.code,
                    "publish",
                    None,
                    &error.message,
                ));
            }
        }
        let snapshots = snapshots(app)?;
        let reference = input.save(&snapshots, kind)?;
        // Commit all approved blocks and their source rows before selecting any model input.
        app.store.prepare(
            id,
            &reference,
            &input.input_fingerprint,
            &input
                .selection
                .sources()
                .map(serde_json::to_value)
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
    let _lock = lock_until(app, deadline)?;
    finish_frozen(app, id, started, deadline, &snapshots(app)?, None)
}

fn lock_until(app: &App, deadline: Instant) -> Result<ReportLock, ReportError> {
    ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs(5).min(deadline.saturating_duration_since(Instant::now())),
    )
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
    let expired = || ReportError::new("PERSONAL_INPUT_EXPIRED", "个人日报临时输入已过期");
    let reference = SnapshotRef {
        path: run.snapshot_path.clone().ok_or_else(expired)?,
        sha256: run.snapshot_sha256.clone().ok_or_else(expired)?,
    };
    let mut input = match just_saved {
        Some((saved, input)) if saved == reference => input,
        _ => {
            let value = snapshots.read(&reference).map_err(|e| {
                if e.code == "SNAPSHOT_MISSING" {
                    expired()
                } else {
                    e
                }
            })?;
            let input: PersonalInput = serde_json::from_value(value)?;
            input.context.validate()?;
            input.selection.validate()?;
            if input.input_fingerprint != fingerprint(&input.fingerprint_fields())? {
                return Err(snapshot_mismatch("个人日报冻结输入指纹不符"));
            }
            input
        }
    };
    let bounded = input.version == BOUNDED_INPUT_VERSION;
    if !matches!(input.version, 5 | 6 | BOUNDED_INPUT_VERSION)
        || bounded != input.prompts.is_some()
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
        return Err(snapshot_mismatch("个人日报冻结输入或配置已改变"));
    }
    let root_alias = &app.config.report.root_alias;
    let (attachment, publication) = if bounded {
        check_deadline(deadline)?;
        let attachment = input.bounded_attachment(root_alias)?;
        let publication = input.publication_rules(&attachment)?;
        (Some(attachment), Some(publication))
    } else {
        (None, None)
    };
    if reference.path == format!("{id}/personal-materials-{}.json", reference.sha256) {
        if input.version < 6
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
        input.prepare_plan(&app.config.llm, publication.as_ref())?;
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
    if let Some(publication) = &publication {
        match &input.plan {
            Some(plan) if plan.target["publication"] != serde_json::to_value(publication)? => {
                return Err(snapshot_mismatch("冻结计划与完整依据或渲染模板不一致"));
            }
            None if !input.selection.materials.is_empty() => {
                return Err(snapshot_mismatch("冻结输入缺少模型计划"));
            }
            _ => {}
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
        Some(match model::generate(&runner, plan) {
            Err(error) if bounded => return Err(error),
            Ok(output) if bounded && output.items.is_empty() => {
                return Err(ReportError::new(
                    "PERSONAL_SUMMARY_UNAVAILABLE",
                    "模型未形成有效事项总结",
                ))
            }
            result => result,
        })
    } else {
        None
    };
    let mut report = input.report(root_alias, generated);
    if report.items.is_empty() {
        if let Some(current) = app
            .store
            .current(&run.target_date)?
            .filter(|r| r.run_id != id)
        {
            let previous = read_frozen_report_json(current.report_json.as_deref().unwrap_or("{}"))?;
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
    if input.version >= 6 {
        let denied = denied_sources(&app.config, report.sources.iter())?;
        if !denied.is_empty() {
            if bounded
                || report
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
    let markdown = input.render_with_attachment(&mut report, attachment)?;
    check_deadline(started + PERSONAL_APP_BUDGET)?;
    let value = serde_json::to_value(&report)?;
    let now = Utc::now();
    let (output_path, action, markdown) = if run.run_kind == "preview" {
        let preview = if bounded {
            report.render_bounded_output(&input.template, true)?
        } else {
            report.render_preview(&input.template)?
        };
        app.store.finish_preview(id, &value, &markdown, now)?;
        (None, "preview", preview)
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
            markdown,
        )
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
    let priorities = model::source_priorities(&input.selection);
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
        if input
            .plan
            .as_ref()
            .and_then(model::legacy_compaction_prompt)
            .is_some()
        {
            if !model::omit_lowest_priority(&mut report.items, &priorities) {
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
        let sent = self
            .selection
            .sources()
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
        check_deadline(self.deadline)?;
        self.check()?;
        ensure_snapshot_space(self.app, self.snapshot_bytes)?;
        self.app.transport.prepare()
    }
    fn before_send(&self) -> Result<(), ReportError> {
        self.app.transport.before_send()?;
        self.check()
    }
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        self.app.transport.send(request, timeout)
    }
}
