//! 只读健康巡检：任务、采集、日报发布、磁盘与数据库状态。
//!
//! 该模块不调用模型、不扫描原目录、不修改数据库；`health` 命令与健康任务共用这里的检查结果。
use crate::{
    archive::store::ArchiveStore, config::AppConfig, operations::display_path, snapshots::digest,
    types, ReportError,
};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const DISK_WARN_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const DISK_ERROR_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct HealthCheck {
    pub code: &'static str,
    pub level: &'static str,
    pub message: String,
    pub evidence: Value,
}

#[derive(Debug, Serialize)]
pub struct TaskNames {
    pub daily: String,
    pub collect: String,
    pub parse: String,
    pub health: String,
}

#[derive(Debug, Serialize)]
pub struct HealthReport {
    pub status: &'static str,
    pub target_date: String,
    pub checks: Vec<HealthCheck>,
    pub task_names: TaskNames,
    pub log_dir: String,
    pub generated_at: String,
}

impl HealthCheck {
    fn ok(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            level: "ok",
            message: message.into(),
            evidence: json!({}),
        }
    }
    fn warning(code: &'static str, message: impl Into<String>, evidence: Value) -> Self {
        Self {
            code,
            level: "warning",
            message: message.into(),
            evidence,
        }
    }
    fn error(code: &'static str, message: impl Into<String>, evidence: Value) -> Self {
        Self {
            code,
            level: "error",
            message: message.into(),
            evidence,
        }
    }
}

fn open_read(path: &std::path::Path) -> Result<Connection, ReportError> {
    if !path.is_file() {
        return Err(ReportError::new("HEALTH_DB_MISSING", "数据库文件不存在"));
    }
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

fn quick_check(connection: &Connection) -> bool {
    matches!(
        connection.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)),
        Ok(value) if value == "ok"
    )
}

fn shanghai_day_start(target: NaiveDate) -> Option<DateTime<Utc>> {
    let day = target.and_hms_opt(0, 0, 0)?;
    let local = types::shanghai().from_local_datetime(&day).single()?;
    Some(local.with_timezone(&Utc))
}

fn baseline(store: &ArchiveStore) -> Result<Option<DateTime<Utc>>, ReportError> {
    let value: Option<String> = store
        .conn
        .query_row(
            "SELECT value FROM metadata WHERE key='baseline'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value
        .and_then(|text| DateTime::parse_from_rfc3339(&text).ok())
        .map(|value| value.with_timezone(&Utc)))
}

fn collection_continuity(
    store: &ArchiveStore,
    target: NaiveDate,
    now: DateTime<Utc>,
) -> Result<HealthCheck, ReportError> {
    let Some(window_start) = shanghai_day_start(target) else {
        return Ok(HealthCheck::warning(
            "COLLECTION_GAP",
            "目标日期无法换算为上海时间",
            json!({"target_date": target}),
        ));
    };
    let window_end = window_start + Duration::days(1);
    let start = baseline(store)?.map_or(window_start, |value| window_start.max(value));
    let end = window_end.min(now);
    if start >= end {
        return Ok(HealthCheck::ok(
            "COLLECTION_GAP",
            "目标时段尚未结束或归档尚未建立基线，无需核对采集连续性",
        ));
    }
    let coverage = crate::archive::schedule::coverage(store, start, end, now)?;
    if coverage.planned_idle {
        return Ok(HealthCheck::ok(
            "COLLECTION_GAP",
            "按当时生效的工作日采集规则，目标范围没有计划采集点",
        ));
    }
    let completed = coverage.completed;
    let gaps = coverage.gaps;
    let ranges = coverage.ranges;
    if !completed {
        return Ok(HealthCheck::error(
            "COLLECTION_GAP",
            "目标范围尚无可核对的已完成采集记录，无法判断是否有变化",
            json!({"target_date": target}),
        ));
    }
    if gaps == 0 {
        return Ok(HealthCheck::ok(
            "COLLECTION_GAP",
            "目标范围采集连续，未发现缺少已完成采集记录的时间",
        ));
    }
    Ok(HealthCheck::error(
        "COLLECTION_GAP",
        format!(
            "按当时生效的采集间隔检查（允许 1 分钟调度偏差），发现 {gaps} 段缺少已完成采集记录，前 {} 段（Asia/Shanghai）：{}",
            ranges.len(),
            ranges.join("；")
        ),
        json!({"target_date": target, "gaps": gaps, "ranges": ranges}),
    ))
}

fn published_report(
    connection: &Connection,
    config: &AppConfig,
    target: NaiveDate,
) -> Result<HealthCheck, ReportError> {
    let row = connection
        .query_row(
            "SELECT run_id,quality,output_path,expected_sha256 FROM report_runs WHERE target_date=?1 AND run_kind='report' AND status='succeeded' AND phase='published' ORDER BY created_at DESC LIMIT 1",
            [target.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((run_id, quality, output_path, expected_sha256)) = row else {
        return Ok(HealthCheck::error(
            "REPORT_NOT_PUBLISHED",
            format!("{target} 没有已发布的正式日报"),
            json!({"target_date": target, "reports_dir": display_path(&config.paths.reports_dir)}),
        ));
    };
    let Some(expected) = expected_sha256 else {
        return Ok(HealthCheck::error(
            "REPORT_HASH_MISMATCH",
            format!("{target} 日报缺少发布哈希，无法核对正文"),
            json!({"run_id": run_id}),
        ));
    };
    let path = output_path.map(PathBuf::from).unwrap_or_else(|| {
        config
            .paths
            .reports_dir
            .join(target.format("%Y-%m").to_string())
            .join(format!("{target}.md"))
    });
    if !path.is_file() {
        return Ok(HealthCheck::error(
            "REPORT_HASH_MISMATCH",
            format!("{target} 日报正文文件缺失：{}", display_path(&path)),
            json!({"run_id": run_id}),
        ));
    }
    let actual = digest(&fs::read(&path)?);
    if actual != expected {
        return Ok(HealthCheck::error(
            "REPORT_HASH_MISMATCH",
            format!("{target} 日报正文哈希与数据库不一致"),
            json!({"run_id": run_id, "path": display_path(&path)}),
        ));
    }
    Ok(HealthCheck::ok(
        "REPORT_NOT_PUBLISHED",
        format!(
            "{target} 日报已发布（run {run_id}，质量 {}）",
            quality.unwrap_or_else(|| "unknown".into())
        ),
    ))
}

fn run_attention(connection: &Connection, now: DateTime<Utc>) -> Result<HealthCheck, ReportError> {
    let pending: i64 = connection.query_row(
        "SELECT count(*) FROM report_runs WHERE run_kind='report' AND phase IN ('prepared','generating','db_committed') AND status!='succeeded' AND COALESCE(error_code,'')!='RUN_SUPERSEDED' AND COALESCE(regeneration_reason,'') NOT LIKE 'coverage_publish:%'",
        [],
        |row| row.get(0),
    )?;
    let cutoff = now - Duration::hours(24);
    // 正式发布时间的上界：同一 scope/目标日、run_kind=report 且已完成 published 的运行。
    // 只有比失败更晚的发布才能证明该失败已恢复。
    let mut published_statement = connection.prepare(
        "SELECT scope_id,target_date,MAX(COALESCE(finished_at,created_at)) FROM report_runs \
         WHERE run_kind='report' AND status='succeeded' AND phase='published' \
         GROUP BY scope_id,target_date",
    )?;
    let published: Vec<(String, String, String)> = published_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut statement = connection
        .prepare("SELECT scope_id,target_date,created_at FROM report_runs WHERE status='failed'")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut recent_failures = 0usize;
    let mut resolved_failures = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for row in rows {
        let (scope, target, text) = row?;
        let Ok(at) = DateTime::parse_from_rfc3339(&text) else {
            continue;
        };
        let at = at.with_timezone(&Utc);
        if at < cutoff {
            continue;
        }
        let resolved = published
            .iter()
            .find(|(published_scope, published_target, _)| {
                *published_scope == scope && *published_target == target
            })
            .and_then(|(_, _, published_at)| {
                DateTime::parse_from_rfc3339(published_at)
                    .ok()
                    .map(|value| value.with_timezone(&Utc) > at)
            })
            .unwrap_or(false);
        if resolved {
            resolved_failures += 1;
            continue;
        }
        recent_failures += 1;
        if failures.len() < 5 {
            failures.push(text);
        }
    }
    if pending == 0 && recent_failures == 0 {
        // 即使当前没有告警，证据里仍保留已恢复失败的数量，便于核对历史失败未丢失。
        return Ok(HealthCheck {
            code: "RUN_ATTENTION",
            level: "ok",
            message: if resolved_failures == 0 {
                "没有待续运行，近 24 小时没有失败运行".to_string()
            } else {
                format!(
                    "没有待续运行；近 24 小时的 {resolved_failures} 个失败已由更晚的正式发布恢复"
                )
            },
            evidence: json!({
                "pending": 0,
                "recent_failures": 0,
                "resolved_failures": resolved_failures,
                "failures": []
            }),
        });
    }
    let mut message = format!("待续运行 {pending} 个，近 24 小时失败运行 {recent_failures} 个");
    if resolved_failures > 0 {
        message.push_str(&format!(
            "（另有 {resolved_failures} 个失败已由更晚的正式发布恢复）"
        ));
    }
    Ok(HealthCheck::warning(
        "RUN_ATTENTION",
        message,
        json!({
            "pending": pending,
            "recent_failures": recent_failures,
            "resolved_failures": resolved_failures,
            "failures": failures
        }),
    ))
}

fn parse_queue(
    store: &ArchiveStore,
    config: &AppConfig,
    now: DateTime<Utc>,
) -> Result<HealthCheck, ReportError> {
    let queued: i64 = store.conn.query_row(
        "SELECT count(*) FROM extractions WHERE status IN ('queued','running') OR id IN (SELECT value FROM metadata WHERE key LIKE 'retry:%')",
        [], |row| row.get(0),
    )?;
    let oldest: Option<String> = store.conn.query_row(
        "SELECT min(v.observed_at) FROM versions v JOIN version_extractions ve ON ve.version_id=v.id JOIN extractions e ON e.id=ve.extraction_id WHERE v.pruned_at IS NULL AND (e.status IN ('queued','running') OR e.id IN (SELECT value FROM metadata WHERE key LIKE 'retry:%'))",
        [], |row| row.get(0),
    )?;
    let age = oldest
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| (now - at.with_timezone(&Utc)).num_seconds().max(0));
    let last: Option<String> = store.conn.query_row(
        "SELECT metrics_json FROM collections WHERE status LIKE 'parse_%' ORDER BY julianday(started_at) DESC LIMIT 1", [], |row| row.get(0)
    ).optional()?;
    let changes = if config.report.profile == crate::config::ReportProfile::Personal {
        0
    } else {
        crate::archive::queue::pending_changes(
            store,
            &crate::archive::parse::profile(config)?,
            None,
        )?
    };
    let expired: i64 = store.conn.query_row(
        "SELECT count(*) FROM extractions WHERE status='expired'",
        [],
        |r| r.get(0),
    )?;
    let gaps: i64 = store.conn.query_row(
        "SELECT count(*) FROM gaps WHERE resolved_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    let evidence = json!({"queued":queued,"expired":expired,"read_gaps":gaps,"pending_changes":changes,"oldest_observed_at":oldest,"oldest_age_seconds":age,
        "last_parse_run":last.map(|value|serde_json::from_str::<Value>(&value)).transpose()?});
    Ok(HealthCheck {
        code: "PARSE_QUEUE",
        level: if queued == 0 && changes == 0 {
            "ok"
        } else {
            "warning"
        },
        message: if queued == 0 && changes == 0 {
            "归档解析队列为空".into()
        } else {
            format!("归档仍有 {queued} 个解析任务、{changes} 个变化依据未准备完成")
        },
        evidence,
    })
}

fn disk_space(config: &AppConfig) -> HealthCheck {
    match free_bytes(&config.paths.data_dir) {
        Some(bytes) if bytes < DISK_ERROR_BYTES => HealthCheck::error(
            "DISK_SPACE",
            format!(
                "数据目录所在磁盘剩余 {:.1} GiB，低于 5 GiB",
                bytes as f64 / 1024.0 / 1024.0 / 1024.0
            ),
            json!({"free_bytes": bytes}),
        ),
        Some(bytes) if bytes < DISK_WARN_BYTES => HealthCheck::warning(
            "DISK_SPACE",
            format!(
                "数据目录所在磁盘剩余 {:.1} GiB，低于 20 GiB",
                bytes as f64 / 1024.0 / 1024.0 / 1024.0
            ),
            json!({"free_bytes": bytes}),
        ),
        Some(bytes) => HealthCheck::ok(
            "DISK_SPACE",
            format!(
                "数据目录所在磁盘剩余 {:.1} GiB",
                bytes as f64 / 1024.0 / 1024.0 / 1024.0
            ),
        ),
        None => HealthCheck::warning("DISK_SPACE", "无法读取数据目录所在磁盘剩余空间", json!({})),
    }
}

#[cfg(windows)]
fn free_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut root = path.to_path_buf();
    loop {
        if root.exists() {
            break;
        }
        if !root.pop() {
            return None;
        }
    }
    let wide: Vec<u16> = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };
    (ok != 0).then_some(available)
}

#[cfg(not(windows))]
fn free_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

/// 只读检查；数据库缺失或损坏以 unhealthy 结果返回，不向调用方抛错。
pub fn check(
    config: &AppConfig,
    target: NaiveDate,
    now: DateTime<Utc>,
) -> Result<HealthReport, ReportError> {
    let mut checks = Vec::new();
    let reports_path = config.paths.db_dir.join("reports.sqlite3");
    let reports = match open_read(&reports_path) {
        Ok(connection) => {
            if quick_check(&connection) {
                Some(connection)
            } else {
                checks.push(HealthCheck::error(
                    "DATABASE_INTEGRITY",
                    "报告库完整性检查未通过",
                    json!({"path": display_path(&reports_path)}),
                ));
                None
            }
        }
        Err(_) => {
            checks.push(HealthCheck::error(
                "DATABASE_INTEGRITY",
                "报告库不存在或无法只读打开",
                json!({"path": display_path(&reports_path)}),
            ));
            None
        }
    };
    match &reports {
        Some(connection) => {
            match published_report(connection, config, target) {
                Ok(check) => checks.push(check),
                Err(_) => checks.push(HealthCheck::error(
                    "DATABASE_INTEGRITY",
                    format!("{target} 日报无法核对：报告库结构不可用"),
                    json!({"target_date": target}),
                )),
            }
            match run_attention(connection, now) {
                Ok(check) => checks.push(check),
                Err(_) => checks.push(HealthCheck::error(
                    "DATABASE_INTEGRITY",
                    "运行状态无法核对：报告库结构不可用",
                    json!({}),
                )),
            }
            let partial: Option<String> = connection.query_row(
                "SELECT r.report_json FROM report_runs r JOIN daily_reports d ON d.current_run_id=r.run_id WHERE d.date=? AND r.quality='partial'",
                [target.to_string()], |r|r.get(0)).optional().ok().flatten();
            if let Some(report) = partial {
                let report: Value = serde_json::from_str(&report).unwrap_or_default();
                checks.push(HealthCheck::warning("REPORT_CONTENT_PARTIAL", "日报已发布，内容仅部分可用",
                    json!({"quality":"partial","selection":report["selection"],"gaps":report["gaps"]})));
            }
        }
        None => checks.push(HealthCheck::error(
            "REPORT_NOT_PUBLISHED",
            format!("{target} 日报无法核对：报告库不可用"),
            json!({"target_date": target}),
        )),
    }

    match ArchiveStore::open_collection(config, false) {
        Ok(store) => {
            if quick_check(&store.conn) {
                match collection_continuity(&store, target, now) {
                    Ok(check) => checks.push(check),
                    Err(_) => checks.push(HealthCheck::error(
                        "DATABASE_INTEGRITY",
                        "归档索引结构不可用，无法核对采集连续性",
                        json!({"target_date": target}),
                    )),
                }
                match parse_queue(&store, config, now) {
                    Ok(check) => checks.push(check),
                    Err(_) => checks.push(HealthCheck::error(
                        "DATABASE_INTEGRITY",
                        "归档索引结构不可用，无法核对解析队列",
                        json!({}),
                    )),
                }
            } else {
                checks.push(HealthCheck::error(
                    "DATABASE_INTEGRITY",
                    "归档索引完整性检查未通过",
                    json!({"path": display_path(&store.root.join("index.sqlite3"))}),
                ));
                checks.push(HealthCheck::error(
                    "COLLECTION_GAP",
                    "归档索引不可用，无法核对目标日采集连续性",
                    json!({"target_date": target}),
                ));
            }
        }
        Err(_) => {
            checks.push(HealthCheck::error(
                "COLLECTION_GAP",
                "归档索引不存在或无法只读打开，无法核对采集连续性",
                json!({"target_date": target, "data_dir": display_path(&config.paths.data_dir)}),
            ));
        }
    }
    checks.push(disk_space(config));
    if config.report.profile == crate::config::ReportProfile::Personal {
        let root = config.paths.data_dir.join("daily_cache");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut bytes = 0u64;
        let mut complete = true;
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            if std::time::Instant::now() >= deadline {
                complete = false;
                break;
            }
            match entry {
                Ok(entry) if entry.file_type().is_file() => match entry.metadata() {
                    Ok(metadata) => bytes = bytes.saturating_add(metadata.len()),
                    Err(_) => complete = false,
                },
                Ok(entry) if entry.path_is_symlink() => complete = false,
                Err(_) => complete = false,
                _ => {}
            }
        }
        checks.push(HealthCheck {
            code:"PROCESSING_CACHE",level:if complete && bytes <= crate::processing_cache::MAX_BYTES {"ok"} else {"warning"},
            message:format!("个人处理缓存已核对 {bytes} 字节"),
            evidence:json!({"path":display_path(&root),"logical_bytes":bytes,"complete":complete,"max_bytes":crate::processing_cache::MAX_BYTES,"retention_days":7}),
        });
    }

    let status = if checks.iter().any(|check| check.level == "error") {
        "unhealthy"
    } else if checks.iter().any(|check| check.level == "warning") {
        "warning"
    } else {
        "healthy"
    };
    Ok(HealthReport {
        status,
        target_date: target.to_string(),
        checks,
        task_names: TaskNames {
            daily: config.schedule.task_name.clone(),
            collect: format!("{} Collect", config.schedule.task_name),
            parse: format!("{} Parse", config.schedule.task_name),
            health: format!("{} Health", config.schedule.task_name),
        },
        log_dir: display_path(&config.paths.log_dir),
        generated_at: now.to_rfc3339(),
    })
}

fn shanghai_date_of(generated_at: &str) -> Result<NaiveDate, ReportError> {
    DateTime::parse_from_rfc3339(generated_at)
        .map(|value| value.with_timezone(&types::shanghai()).date_naive())
        .map_err(|_| ReportError::new("HEALTH_TIMESTAMP_INVALID", "健康结果生成时间无效"))
}

/// 原子写入健康 JSON：临时文件与目标同目录，显式 UTF-8，刷新后替换。
pub fn write_report(report: &HealthReport, path: &Path) -> Result<(), ReportError> {
    if path.file_name().is_none() {
        return Err(ReportError::new(
            "HEALTH_PATH_INVALID",
            "健康文件路径缺少文件名",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ReportError::new("HEALTH_PATH_INVALID", "健康文件路径缺少目录"))?;
    crate::snapshots::reject_redirects(path, "HEALTH_PATH_REDIRECTED")?;
    fs::create_dir_all(parent)?;
    crate::snapshots::reject_redirects(path, "HEALTH_PATH_REDIRECTED")?;
    let mut bytes = serde_json::to_vec_pretty(report)?;
    bytes.push(b'\n');
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|error| ReportError::from(error.error))?;
    Ok(())
}

/// 按 generated_at 的上海日期写入 `log_dir/health-YYYY-MM-DD.json`。
pub fn persist(report: &HealthReport) -> Result<PathBuf, ReportError> {
    let day = shanghai_date_of(&report.generated_at)?;
    let path = PathBuf::from(&report.log_dir).join(format!("health-{day}.json"));
    write_report(report, &path)?;
    Ok(path)
}

pub fn exit_code(report: &HealthReport) -> i32 {
    match report.status {
        "healthy" => 0,
        "warning" => 1,
        _ => 2,
    }
}

pub fn render(report: &HealthReport, format: crate::archive::cli::Format) -> String {
    match format {
        crate::archive::cli::Format::Json => {
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into())
        }
        crate::archive::cli::Format::Text => {
            let mut lines = vec![format!("{} {}", report.status, report.target_date)];
            for check in &report.checks {
                lines.push(format!(
                    "[{}] {}: {}",
                    check.level, check.code, check.message
                ));
            }
            lines.join("\n")
        }
    }
}
