mod input;
pub mod personal;
mod personal_scan;
use crate::{
    config::AppConfig,
    model::transport::{HttpTransport, ModelTransport},
    publish::Publisher,
    snapshots::SnapshotStore,
    store::{locking::ReportLock, Store},
    types::{ReportOutcome, ReportRequest, RunPhase},
    ReportError,
};
use ai_daily_scanner_contract::{AdapterPaths, ReportMode as ScanMode};
use ai_daily_scanner_core::{ContextResult, ScanRequest, Scanner, ScannerConfig};
use chrono::{DateTime, Utc};
use std::{
    fs,
    path::Path,
    sync::OnceLock,
    time::{Duration, Instant},
};
pub struct App {
    pub processing: Option<crate::processing_cache::ProcessingSession>,
    pub started: Instant,
    pub config: AppConfig,
    pub store: Store,
    pub snapshots: SnapshotStore,
    pub transport: Box<dyn ModelTransport>,
    scanner: OnceLock<Scanner>,
}

impl App {
    pub fn open(config: AppConfig, now: DateTime<Utc>) -> Result<Self, ReportError> {
        Self::open_started(config, now, Instant::now())
    }
    pub fn open_started(
        config: AppConfig,
        now: DateTime<Utc>,
        started: Instant,
    ) -> Result<Self, ReportError> {
        Self::open_inner(config, now, started, true)
    }
    pub fn open_history(config: AppConfig, now: DateTime<Utc>) -> Result<Self, ReportError> {
        Self::open_inner(config, now, Instant::now(), false)
    }
    fn open_inner(
        config: AppConfig,
        now: DateTime<Utc>,
        started: Instant,
        maintain: bool,
    ) -> Result<Self, ReportError> {
        let processing =
            if maintain && config.report.profile == crate::config::ReportProfile::Personal {
                Some(crate::processing_cache::ProcessingSession::acquire(
                    &config,
                    now,
                    started + Duration::from_secs(300),
                )?)
            } else {
                None
            };
        let path = config.paths.db_dir.join("reports.sqlite3");
        let lock = ReportLock::acquire(
            &path,
            &config.report.scope_id,
            if processing.is_some() {
                Duration::from_secs(5).min(
                    (started + Duration::from_secs(270)).saturating_duration_since(Instant::now()),
                )
            } else {
                Duration::from_secs_f64(config.recovery.lock_wait_seconds)
            },
        )?;
        let store = Store::open_locked(&path, now, &lock)?;
        if let Some(session) = &processing {
            session.prune_snapshots()?;
            store.compact_personal_until(
                now,
                &session.quota.root.join("snapshots"),
                (started + Duration::from_secs(270)).min(Instant::now() + Duration::from_secs(2)),
            )?;
        }
        let snapshot_root = config.paths.data_dir.join("run_snapshots");
        let snapshots = if maintain {
            SnapshotStore::new(
                config.paths.data_dir.join("run_snapshots"),
                config
                    .retention
                    .snapshot_max_mib
                    .saturating_mul(1024 * 1024),
            )?
        } else {
            SnapshotStore::retained(
                snapshot_root,
                config
                    .retention
                    .snapshot_max_mib
                    .saturating_mul(1024 * 1024),
            )?
        };
        let transport = Box::new(HttpTransport::new(&config));
        Ok(Self {
            processing,
            started,
            config,
            store,
            snapshots,
            transport,
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
        let mode = ScanMode::Daily;
        let result = self
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
        Ok(result)
    }
}

fn path_text(path: &Path) -> Result<String, ReportError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| ReportError::new("PATH_INVALID", "路径不是有效文字"))
}
pub fn run_report(request: ReportRequest, app: &App) -> Result<ReportOutcome, ReportError> {
    if request.period.mode != crate::types::ReportMode::Daily
        || app.config.report.profile != crate::config::ReportProfile::Personal
    {
        return Err(ReportError::new(
            "FEATURE_RETIRED",
            "当前程序只生成个人日报",
        ));
    }
    personal::run(request, app, app.started)
}

pub fn run_daily(request: ReportRequest, app: &App) -> Result<ReportOutcome, ReportError> {
    run_report(request, app)
}
pub fn repair_run(app: &App, id: &str, now: DateTime<Utc>) -> Result<ReportOutcome, ReportError> {
    let path = Publisher::new(&app.config.paths.reports_dir, &app.store)?.publish(id, now)?;
    let run = app.store.get(id)?;
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
pub fn repair_by_id(app: &App, id: &str, now: DateTime<Utc>) -> Result<ReportOutcome, ReportError> {
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs_f64(app.config.recovery.lock_wait_seconds),
    )?;
    repair_run(app, id, now)
}
