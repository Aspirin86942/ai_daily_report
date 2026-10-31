//! 只读健康巡检：任务、采集、日报发布、磁盘与数据库状态。
//!
//! 该模块不调用模型、不扫描原目录、不修改数据库；`health` 命令与健康任务共用这里的检查结果。
use crate::{config::AppConfig, operations::display_path, snapshots::digest, types, ReportError};
use chrono::{DateTime, Duration, NaiveDate, Utc};
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
    let report_json: Option<String> = connection.query_row(
        "SELECT report_json FROM report_runs WHERE run_id=?",
        [&run_id],
        |row| row.get(0),
    )?;
    if let Err(error) =
        crate::publish::verify_attachment(&path, report_json.as_deref().unwrap_or("{}"))
    {
        return Ok(HealthCheck::error(
            "REPORT_EVIDENCE_MISMATCH",
            format!("{target} 日报依据附件未通过核对"),
            json!({"run_id":run_id,"error_code":error.code}),
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
        "SELECT count(*) FROM report_runs r WHERE run_kind='report' AND phase IN ('prepared','generating','db_committed') AND status!='succeeded' AND COALESCE(error_code,'')!='RUN_SUPERSEDED' AND COALESCE(regeneration_reason,'') NOT LIKE 'coverage_publish:%'
         AND NOT EXISTS(SELECT 1 FROM daily_reports d JOIN report_runs p ON p.run_id=d.current_run_id
             WHERE p.scope_id=r.scope_id AND p.target_date=r.target_date AND p.created_at>r.created_at
             AND p.status='succeeded' AND p.phase='published'
             AND (COALESCE(json_extract(p.report_json,'$.schema_version'),0)!=5
                 OR json_array_length(p.report_json,'$.items')>0 OR p.quality='no_changes'))",
        [],
        |row| row.get(0),
    )?;
    let cutoff = now - Duration::hours(24);
    // 正式发布时间的上界：同一 scope/目标日、run_kind=report 且已完成 published 的运行。
    // 只有比失败更晚的发布才能证明该失败已恢复。
    let mut published_statement = connection.prepare(
        "SELECT scope_id,target_date,MAX(COALESCE(finished_at,created_at)) FROM report_runs \
         WHERE run_kind='report' AND status='succeeded' AND phase='published' \
         AND (COALESCE(json_extract(report_json,'$.schema_version'),0)!=5 \
             OR json_array_length(report_json,'$.items')>0 OR quality='no_changes') \
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
            if config.report.profile == crate::config::ReportProfile::Personal {
                match crate::personal_recovery::issues(connection, &config.report.scope_id, now) {
                    Ok(issues) => {
                        let missing = issues.iter().filter(|issue| issue.missing_summary).count();
                        checks.push(HealthCheck {
                            code: "HISTORICAL_REPORT_ATTENTION",
                            level: if missing > 0 { "error" } else if issues.is_empty() { "ok" } else { "warning" },
                            message: if issues.is_empty() { "没有未恢复的个人日报异常".into() }
                                else { format!("{} 个历史日报仍需恢复，其中 {missing} 个缺少有效总结；告警保留至有效发布完成", issues.len()) },
                            evidence: json!({"reports":issues}),
                        });
                    }
                    Err(_) => checks.push(HealthCheck::error(
                        "DATABASE_INTEGRITY",
                        "历史日报异常无法核对",
                        json!({}),
                    )),
                }
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

    checks.push(match open_read(&config.index_db_path) {
        Ok(connection) if quick_check(&connection) => {
            HealthCheck::ok("SCAN_INDEX_INTEGRITY", "每日扫描索引完整性正常")
        }
        _ => HealthCheck::error(
            "SCAN_INDEX_INTEGRITY",
            "每日扫描索引不存在或完整性检查未通过",
            json!({"path":display_path(&config.index_db_path)}),
        ),
    });
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
