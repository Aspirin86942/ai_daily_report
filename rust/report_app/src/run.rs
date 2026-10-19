mod daily;
mod input;
mod legacy;
mod period;
pub use input::FrozenInput;
pub fn prepare_daily_input(
    app: &App,
    request: &ReportRequest,
    scan: &ContextResult,
) -> Result<FrozenInput, ReportError> {
    input::prepare(app, request, scan)
}
pub use period::run_period;
pub fn prepare_archive_input(
    app: &App,
    request: &ReportRequest,
) -> Result<FrozenInput, ReportError> {
    input::prepare_archive(app, request)
}

use crate::{
    config::AppConfig,
    model::{
        transport::{HttpTransport, ModelTransport},
        ModelRunner,
    },
    publish::Publisher,
    reports::{self, data::DailyReport},
    snapshots::{canonical_json, digest, SnapshotRef, SnapshotStore},
    store::{locking::ReportLock, StagedReport, Store},
    types::{ReportOutcome, ReportRequest, RunPhase},
    ReportError,
};
use ai_daily_scanner_contract::{AdapterPaths, ReportMode as ScanMode};
use ai_daily_scanner_core::{ContextResult, ScanRequest, Scanner, ScannerConfig};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    fs,
    path::Path,
    sync::OnceLock,
    time::{Duration, Instant},
};

pub struct App {
    pub config: AppConfig,
    pub store: Store,
    pub snapshots: SnapshotStore,
    pub transport: Box<dyn ModelTransport>,
    pub model_workers: u8,
    pub ocr_manifest: crate::maintenance::ocr::OcrManifest,
    scanner: OnceLock<Scanner>,
}

impl App {
    pub fn open(config: AppConfig, now: DateTime<Utc>) -> Result<Self, ReportError> {
        let path = config.paths.db_dir.join("reports.sqlite3");
        let lock = ReportLock::acquire(
            &path,
            &config.report.scope_id,
            Duration::from_secs_f64(config.recovery.lock_wait_seconds),
        )?;
        let store = Store::open_locked(&path, now, &lock)?;
        let snapshots = SnapshotStore::new(
            config.paths.data_dir.join("run_snapshots"),
            config
                .retention
                .snapshot_max_mib
                .saturating_mul(1024 * 1024),
        )?;
        let transport = Box::new(HttpTransport::new(&config));
        let model_workers = config.recovery.model_workers;
        Ok(Self {
            config,
            store,
            snapshots,
            transport,
            model_workers,
            ocr_manifest: Default::default(),
            scanner: OnceLock::new(),
        })
    }
    pub fn fingerprint(&self, recovery: bool) -> Result<String, ReportError> {
        self.config.fingerprint(recovery)
    }
    pub fn scan(&self, request: &ReportRequest) -> Result<ContextResult, ReportError> {
        if self.scanner.get().is_none() {
            fs::create_dir_all(self.config.index_db_path.parent().unwrap())?;
            let scanner = Scanner::open(ScannerConfig {
                work_dir: path_text(&self.config.paths.work_dir)?,
                scan_db_path: path_text(&self.config.index_db_path)?,
                scanner_settings: self.config.scanner.clone(),
                adapters: AdapterPaths {
                    office_worker_path: path_text(&self.config.office_worker_path)?,
                    pdf_worker_path: path_text(&self.config.pdf_worker_path)?,
                },
            })
            .map_err(|_| ReportError::new("SCANNER_UNAVAILABLE", "原生扫描器初始化失败"))?;
            let _ = self.scanner.set(scanner);
        }
        let (start, _) = crate::types::utc_window(request.period.start)?;
        let (_, end) = crate::types::utc_window(request.period.end)?;
        let mode = match request.period.mode {
            crate::types::ReportMode::Daily => ScanMode::Daily,
            crate::types::ReportMode::Weekly => ScanMode::Weekly,
            crate::types::ReportMode::Monthly => ScanMode::Monthly,
        };
        let mut result = self
            .scanner
            .get()
            .unwrap()
            .build_context(&ScanRequest {
                report_mode: mode,
                start_date: request.period.start.to_string(),
                end_date: request.period.end.to_string(),
                window_start_utc: Some(start.to_rfc3339()),
                window_end_utc: Some(end.to_rfc3339()),
                compression_profile: None,
            })
            .map_err(|_| ReportError::new("SCANNER_UNAVAILABLE", "原生扫描操作失败"))?
            .value;
        for source in &mut result.sources {
            if let Some(document) = self.ocr_manifest.get(&source.source_id) {
                crate::maintenance::ocr::overlay(
                    source,
                    document,
                    &self.config,
                    &Default::default(),
                )?;
            }
        }
        Ok(result)
    }
}

fn path_text(path: &Path) -> Result<String, ReportError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| ReportError::new("PATH_INVALID", "路径不是有效文字"))
}
pub(crate) fn hash(value: &Value) -> Result<String, ReportError> {
    Ok(digest(&canonical_json(value)?))
}
pub fn run_report(request: ReportRequest, app: &App) -> Result<ReportOutcome, ReportError> {
    match request.period.mode {
        crate::types::ReportMode::Daily => run_daily(request, app),
        _ => run_period(request, app),
    }
}

/// Maintenance freezes the complete input before it executes any model call.
pub fn prepare_frozen(
    app: &App,
    input: &FrozenInput,
    save: bool,
    reason: &str,
) -> Result<String, ReportError> {
    prepare_snapshot(app, &serde_json::to_value(input)?, save, reason)
}
pub fn prepare_snapshot(
    app: &App,
    input: &Value,
    save: bool,
    reason: &str,
) -> Result<String, ReportError> {
    let context: crate::types::RunContext = serde_json::from_value(input["context"].clone())?;
    context.validate()?;
    let input_fingerprint = input["input_fingerprint"]
        .as_str()
        .filter(|s| crate::snapshots::valid_hash(s))
        .ok_or_else(|| ReportError::new("SNAPSHOT_CONTEXT_MISMATCH", "冻结输入缺少有效指纹"))?;
    if !(matches!(input["version"].as_u64(), Some(3 | 4))
        || matches!(input["version"].as_u64(), Some(1 | 2)) && input["rust_recovery_version"] == 3)
        || context.config_fingerprint != app.fingerprint(false)?
    {
        return Err(ReportError::new(
            "SNAPSHOT_CONTEXT_MISMATCH",
            "冻结输入与当前配置不一致",
        ));
    }
    let id = &context.run_id;
    let kind = if save { "report" } else { "preview" };
    let existing = match app.store.get(id) {
        Ok(run) => Some(run),
        Err(error) if error.code == "RUN_NOT_FOUND" => None,
        Err(error) => return Err(error),
    };
    if let Some(run) = &existing {
        if run.run_kind != kind
            || run.generation_id != *id
            || run.config_fingerprint != context.config_fingerprint
            || run.target_date != context.target_date.to_string()
            || run.regeneration_reason.as_deref() != Some(reason)
        {
            return Err(ReportError::new(
                "SNAPSHOT_CONTEXT_MISMATCH",
                "维护运行与冻结输入不一致",
            ));
        }
    } else {
        app.store
            .create(&context, kind, id, Some(reason), context.started_at)?;
    }
    if input["version"] == 4 {
        crate::archive::model_input::pin(&app.config, &serde_json::from_value(input.clone())?)?;
    }
    let reference = app.snapshots.put(id, "input", input)?;
    if let Some(run) = existing {
        if run.snapshot_path.is_some() {
            if run.snapshot_path.as_deref() != Some(reference.path.as_str())
                || run.snapshot_sha256.as_deref() != Some(reference.sha256.as_str())
                || run.input_fingerprint.as_deref() != Some(input_fingerprint)
            {
                return Err(ReportError::new(
                    "SNAPSHOT_CONTEXT_MISMATCH",
                    "已准备的运行输入发生变化",
                ));
            }
            return Ok(id.clone());
        }
        if !matches!(run.phase, RunPhase::Created | RunPhase::Scanning) {
            return Err(ReportError::new(
                "RUN_NOT_RECOVERABLE",
                "运行未处于输入准备阶段",
            ));
        }
        if !matches!(run.status, crate::store::RunStatus::Running) {
            app.store.connection()?.execute("UPDATE report_runs SET status='running',error_code=NULL,finished_at=NULL WHERE run_id=? AND snapshot_path IS NULL",[id])?;
        }
    }
    let sources = input["sources"]
        .as_array()
        .ok_or_else(|| ReportError::new("SNAPSHOT_CONTEXT_MISMATCH", "冻结来源缺失"))?;
    app.store.prepare(
        id,
        &reference,
        input_fingerprint,
        sources,
        context.started_at,
    )?;
    Ok(id.clone())
}

pub fn resume_frozen(
    app: &App,
    id: &str,
    now: DateTime<Utc>,
) -> Result<ReportOutcome, ReportError> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(app.config.recovery.run_budget_seconds);
    let lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs_f64(app.config.recovery.lock_wait_seconds),
    )?;
    app.store.recover_interrupted(now, &lock)?;
    let run = app.store.get(id)?;
    if let (crate::store::RunStatus::Succeeded, "preview", Some(_), Some(markdown)) = (
        &run.status,
        run.run_kind.as_str(),
        run.report_json.as_ref(),
        run.markdown.as_ref(),
    ) {
        return Ok(ReportOutcome {
            run_id: id.into(),
            target_date: run.target_date,
            quality: run.quality.unwrap(),
            markdown: markdown.clone(),
            output_path: None,
            action: "preview_reused".into(),
            model_calls: 0,
        });
    }
    if run.phase == RunPhase::Published {
        return repair_run(app, id, now);
    }
    app.store.resume(id, now)?;
    finish_guarded(app, id, now, started, deadline)
}
fn deadline_check(deadline: Instant) -> Result<(), ReportError> {
    if Instant::now() >= deadline {
        Err(ReportError::new(
            "MODEL_RUN_DEADLINE_EXCEEDED",
            "本次报告运行时间已用完",
        ))
    } else {
        Ok(())
    }
}

/// Prepare only new scheduled archive input; published and frozen runs stay immutable.
pub fn prepare_scheduled_archive(
    app: &App,
    request: &mut ReportRequest,
) -> Result<Option<Value>, ReportError> {
    if !request.scheduled || request.period.source != crate::types::ReportSource::Archive {
        return Ok(None);
    }
    let target = request.context.target_date.to_string();
    if app.store.current(&target)?.is_some() || !app.store.pending(Some(&target))?.is_empty() {
        return Ok(None);
    }
    let ready = crate::archive::queue::prepare_daily(
        &app.config,
        request.context.target_date,
        request.context.started_at,
    )?;
    if ready["action"] != "deferred" {
        // Newly completed parses carry their real ready_at, after the original CLI start.
        request.context.started_at = Utc::now();
    }
    Ok(Some(ready))
}

pub fn run_daily(mut request: ReportRequest, app: &App) -> Result<ReportOutcome, ReportError> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(app.config.recovery.run_budget_seconds);
    let now = request.context.started_at;
    let lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs_f64(app.config.recovery.lock_wait_seconds),
    )?;
    app.store.recover_interrupted(now, &lock)?;
    request.context.config_fingerprint = app.fingerprint(false)?;
    if request.scheduled {
        app.store.activate(request.context.target_date)?;
    }
    if let Some(recovered) = daily::resume_pending(app, &request, now, started, deadline)? {
        return Ok(recovered);
    }
    if let Some(existing) = daily::existing_archive(app, &request)? {
        return Ok(existing);
    }
    let run_id = &request.context.run_id;
    app.store.create(
        &request.context,
        if request.save && !request.dry_run {
            "report"
        } else {
            "preview"
        },
        run_id,
        request.regeneration_reason.as_deref(),
        now,
    )?;
    let result = daily::run_new(app, &request, now, started, deadline);
    if let Err(error) = &result {
        app.store.fail(run_id, error.code, false, now)?;
    }
    result
}

fn finish_guarded(
    app: &App,
    run_id: &str,
    now: DateTime<Utc>,
    started: Instant,
    deadline: Instant,
) -> Result<ReportOutcome, ReportError> {
    let result = finish(app, run_id, now, started, deadline);
    if let Err(error) = &result {
        app.store.fail(run_id, error.code, false, now)?;
    }
    result
}

fn scan_stable(
    app: &App,
    request: &ReportRequest,
    deadline: Instant,
) -> Result<ContextResult, ReportError> {
    deadline_check(deadline)?;
    let first = app.scan(request)?;
    deadline_check(deadline)?;
    let changed = first.sources.iter().any(|source| {
        source.reason.as_ref().is_some_and(|reason| {
            let reason = reason.to_lowercase();
            reason.contains("unstable")
                || reason.contains("source_changed")
                || matches!(
                    reason.as_str(),
                    "source_guard_unavailable" | "source_version_changed"
                )
        })
    });
    if !changed {
        return Ok(first);
    }
    let mut second = app.scan(request)?;
    deadline_check(deadline)?;
    let ids: std::collections::BTreeSet<_> =
        second.sources.iter().map(|s| s.source_id.clone()).collect();
    for mut source in first
        .sources
        .into_iter()
        .filter(|s| !ids.contains(&s.source_id))
    {
        source.parse_status = ai_daily_scanner_contract::ParseStatus::Error;
        source.excerpt.clear();
        source.excerpt_sha256 = digest(b"");
        source.content_sha256 = None;
        source.reason = Some("source_unstable_disappeared_on_retry".into());
        second.sources.push(source);
    }
    second.sources.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    Ok(second)
}

fn finish(
    app: &App,
    run_id: &str,
    now: DateTime<Utc>,
    started: Instant,
    deadline: Instant,
) -> Result<ReportOutcome, ReportError> {
    deadline_check(deadline)?;
    let run = app.store.get(run_id)?;
    if run.phase == RunPhase::DbCommitted {
        return repair_run(app, run_id, now);
    }
    let reference = SnapshotRef {
        path: run
            .snapshot_path
            .clone()
            .ok_or_else(|| ReportError::new("SNAPSHOT_MISSING", "冻结输入缺失"))?,
        sha256: run
            .snapshot_sha256
            .clone()
            .ok_or_else(|| ReportError::new("SNAPSHOT_MISSING", "冻结输入哈希缺失"))?,
    };
    let value = app.snapshots.read(&reference).or_else(|original| {
        crate::archive::model_input::read_frozen(&app.config, run_id, &reference.sha256)
            .map_err(|_| original)
    })?;
    if matches!(value["version"].as_u64(), Some(1 | 2)) {
        return legacy::finish(app, &run, value, now, started, deadline);
    }
    if !matches!(value["version"].as_u64(), Some(3 | 4)) {
        return Err(ReportError::new(
            "SNAPSHOT_VERSION_UNSUPPORTED",
            "冻结输入版本不支持",
        ));
    }
    let mut payload: FrozenInput = serde_json::from_value(value)?;
    payload.context.validate()?;
    if payload.context.run_id != run_id
        || run.input_fingerprint.as_deref() != Some(&payload.input_fingerprint)
    {
        return Err(ReportError::new(
            "SNAPSHOT_CONTEXT_MISMATCH",
            "冻结输入与运行记录不一致",
        ));
    }
    let same_config = run.config_fingerprint == app.fingerprint(false)?;
    if !same_config && payload.recovery_fingerprint != app.fingerprint(true)? {
        return Err(ReportError::new(
            "RECOVERY_CONFIG_CHANGED",
            "冻结输入对应的业务配置已经改变",
        ));
    }
    let mut limits = app.config.recovery.clone();
    limits.model_pricing = payload.model_pricing.clone();
    let transport = crate::archive::model_input::ProtectedTransport {
        config: &app.config,
        input: &payload,
        inner: app.transport.as_ref(),
    };
    let runner = ModelRunner {
        store: &app.store,
        snapshots: &app.snapshots,
        transport: &transport,
        llm: &app.config.llm,
        recovery: &limits,
        recovery_inputs: payload.recovery_inputs.as_ref(),
        allow_new_calls: same_config,
        run_id,
        started_at: now,
        started,
        deadline,
    };
    let generated = runner.generate_with_workers(&payload.plan, app.model_workers)?;
    if payload.version == 4 {
        for (stage, elapsed) in [
            ("fact_extraction", generated.fact_wall_ms),
            ("composition", generated.composition_wall_ms),
        ] {
            crate::archive::stats::record_stage(
                &app.config,
                run_id,
                &payload.context.target_date.to_string(),
                stage,
                elapsed,
            )?;
        }
    }
    let calls = runner.audited_calls()?;
    if payload.version == 4 {
        crate::archive::model_input::validate_evidence(&app.config, &generated.facts)?;
    }
    input::apply_coverage(&mut payload, &generated.steps, &calls)?;
    if !payload.sources.is_empty() && generated.facts.is_empty() {
        payload.gaps.push(reports::data::CoverageGap::new(
            "MODEL_NO_FACTS",
            "model",
            None,
            "现有材料未形成可确认的具体事项。",
        ));
        payload.quality = "partial".into();
    }
    let report = DailyReport {
        schema_version: payload.version,
        report_kind: "file_evidence".into(),
        date: payload.context.target_date.to_string(),
        scope_id: payload.context.scope_id.clone(),
        root_alias: payload.root_alias.clone(),
        timezone: payload.context.timezone.clone(),
        window_start_utc: payload.context.window_start_utc.to_rfc3339(),
        window_end_utc: payload.context.window_end_utc.to_rfc3339(),
        quality: payload.quality.clone(),
        sources: payload.sources.clone(),
        gaps: payload.gaps.clone(),
        missed_dates: payload.missed_dates.clone(),
        generated_at: (now + chrono::Duration::from_std(started.elapsed()).unwrap_or_default())
            .to_rfc3339(),
        user_notes: payload.notes.clone(),
        facts: generated.facts,
        content: generated.report,
    };
    deadline_check(deadline)?;
    let outcome = stage(app, run_id, &payload, &report, now)?;
    if payload.version == 4 {
        crate::archive::stats::record_stage(
            &app.config,
            run_id,
            &payload.context.target_date.to_string(),
            "run_completion",
            started.elapsed().as_millis() as u64,
        )?;
    }
    Ok(outcome)
}

fn stage(
    app: &App,
    run_id: &str,
    input: &FrozenInput,
    report: &DailyReport,
    now: DateTime<Utc>,
) -> Result<ReportOutcome, ReportError> {
    let stage_started = Instant::now();
    let markdown = reports::render_daily(report, &input.template)?;
    let value = serde_json::to_value(report)?;
    let run = app.store.get(run_id)?;
    let (output_path, action) = if run.run_kind == "preview" {
        app.store.finish_preview(run_id, &value, &markdown, now)?;
        (None, "preview")
    } else {
        let publisher = Publisher::new(&app.config.paths.reports_dir, &app.store)?;
        let path = publisher.output_path(&report.date)?;
        app.store.stage_report(
            run_id,
            &StagedReport {
                report: &value,
                markdown: &markdown,
                output_path: &path_text(&path)?,
                publication_fingerprint: &input.publication_fingerprint()?,
            },
            now,
        )?;
        (Some(publisher.publish(run_id, now)?), "published")
    };
    if action == "published" {
        crate::archive::model_input::published(&app.config, run_id, &markdown)?;
    }
    if input.version == 4 {
        crate::archive::stats::record_stage(
            &app.config,
            run_id,
            &report.date,
            "publication",
            stage_started.elapsed().as_millis() as u64,
        )?;
    }
    Ok(ReportOutcome {
        run_id: run_id.into(),
        target_date: report.date.clone(),
        quality: report.quality.clone(),
        markdown,
        output_path,
        action: action.into(),
        model_calls: app.store.calls(run_id)?.len() as u64,
    })
}

pub fn repair_run(app: &App, id: &str, now: DateTime<Utc>) -> Result<ReportOutcome, ReportError> {
    let path = Publisher::new(&app.config.paths.reports_dir, &app.store)?.publish(id, now)?;
    let run = app.store.get(id)?;
    crate::archive::model_input::published(&app.config, id, run.markdown.as_deref().unwrap_or(""))?;
    Ok(ReportOutcome {
        run_id: id.into(),
        target_date: run.target_date,
        quality: run.quality.unwrap(),
        markdown: run.markdown.unwrap(),
        output_path: Some(path),
        action: "repaired".into(),
        model_calls: 0,
    })
}

pub fn repair_output(
    app: &App,
    date: &str,
    now: DateTime<Utc>,
) -> Result<ReportOutcome, ReportError> {
    crate::types::parse_date(date)?;
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs_f64(app.config.recovery.lock_wait_seconds),
    )?;
    let run = app
        .store
        .pending(Some(date))?
        .into_iter()
        .find(|r| r.phase == RunPhase::DbCommitted)
        .or(app.store.current(date)?)
        .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "目标日期没有可恢复的正文"))?;
    repair_run(app, &run.run_id, now)
}
