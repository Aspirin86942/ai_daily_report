//! 只读健康巡检：任务、采集、日报发布、磁盘与数据库状态。
//!
//! 该模块不调用模型、不扫描原目录、不修改数据库；`health` 命令与健康任务共用这里的检查结果。
use crate::{
    config::AppConfig,
    infra::{
        fs::{display_path, optional_file_sha256, reject_redirects, write_atomic},
        json::read_frozen_report_json,
        sqlite::{open_read_only, quick_check, quick_check_yielding, QuickCheckOutcome},
    },
    types, ReportError,
};
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs,
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
    fn new(
        level: &'static str,
        code: &'static str,
        message: impl Into<String>,
        evidence: Value,
    ) -> Self {
        Self {
            code,
            level,
            message: message.into(),
            evidence,
        }
    }

    fn ok(code: &'static str, message: impl Into<String>) -> Self {
        Self::new("ok", code, message, json!({}))
    }

    fn warning(code: &'static str, message: impl Into<String>, evidence: Value) -> Self {
        Self::new("warning", code, message, evidence)
    }

    fn error(code: &'static str, message: impl Into<String>, evidence: Value) -> Self {
        Self::new("error", code, message, evidence)
    }
}

fn open_read(path: &Path) -> Result<Connection, ReportError> {
    open_read_only(
        path,
        std::time::Duration::from_secs(5),
        "HEALTH_PATH_REDIRECTED",
        "HEALTH_DB_MISSING",
    )
}

enum PublishedQuery {
    Resolved(HealthCheck),
    VerifyFiles {
        target: NaiveDate,
        run_id: String,
        quality: Option<String>,
        output_path: Option<String>,
        expected_sha256: Option<String>,
        report_json: Option<String>,
    },
}

fn fetch_published_report(
    connection: &Connection,
    config: &AppConfig,
    target: NaiveDate,
    now: DateTime<Utc>,
    active: &[(String, String)],
) -> Result<PublishedQuery, ReportError> {
    let row = connection
        .query_row(
            "SELECT r.run_id,r.quality,r.output_path,r.expected_sha256,r.report_json
             FROM report_runs r LEFT JOIN daily_reports d ON d.date=r.target_date
             WHERE r.target_date=?1 AND r.run_kind='report' AND r.status='succeeded' AND r.phase='published'
               AND (d.current_run_id IS NULL OR d.current_run_id=r.run_id)
             ORDER BY r.created_at DESC LIMIT 1",
            [target.to_string()],
            |row| <(String, Option<String>, Option<String>, Option<String>, Option<String>)>::try_from(row),
        )
        .optional()?;
    let Some((run_id, quality, output_path, expected_sha256, report_json)) = row else {
        let date = target.to_string();
        if active.iter().any(|(_, target_date)| target_date == &date) {
            return Ok(PublishedQuery::Resolved(HealthCheck::ok(
                "REPORT_IN_PROGRESS",
                format!("{target} 日报正在生成，仍在运行预算内"),
            )));
        }
        let scheduled = types::utc_window(target)?.1
            + Duration::seconds(i64::from(
                config.schedule.due_time()?.num_seconds_from_midnight(),
            ));
        if now < scheduled
            || (now - scheduled)
                .to_std()
                .is_ok_and(|elapsed| elapsed.as_secs() < config.recovery.run_budget_seconds)
        {
            return Ok(PublishedQuery::Resolved(HealthCheck::ok(
                "REPORT_NOT_DUE",
                format!(
                    "{target} 日报尚未到应完成时间（计划 {}，运行预算 {} 秒）",
                    scheduled
                        .with_timezone(&types::shanghai())
                        .format("%Y-%m-%d %H:%M"),
                    config.recovery.run_budget_seconds
                ),
            )));
        }
        return Ok(PublishedQuery::Resolved(HealthCheck::error(
            "REPORT_NOT_PUBLISHED",
            format!("{target} 没有已发布的正式日报"),
            json!({"target_date": target, "reports_dir": display_path(&config.paths.reports_dir)}),
        )));
    };
    Ok(PublishedQuery::VerifyFiles {
        target,
        run_id,
        quality,
        output_path,
        expected_sha256,
        report_json,
    })
}

fn verify_published_files(
    config: &AppConfig,
    query: PublishedQuery,
) -> Result<HealthCheck, ReportError> {
    let PublishedQuery::VerifyFiles {
        target,
        run_id,
        quality,
        output_path,
        expected_sha256,
        report_json,
    } = query
    else {
        let PublishedQuery::Resolved(check) = query else {
            unreachable!()
        };
        return Ok(check);
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
    if optional_file_sha256(&path, "OUTPUT_PATH_REDIRECTED")?.as_deref() != Some(&expected) {
        return Ok(HealthCheck::error(
            "REPORT_HASH_MISMATCH",
            format!("{target} 日报正文哈希与数据库不一致"),
            json!({"run_id": run_id, "path": display_path(&path)}),
        ));
    }
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

fn active_runs(
    connection: &Connection,
    config: &AppConfig,
    now: DateTime<Utc>,
) -> Result<Vec<(String, String)>, ReportError> {
    let mut statement = connection.prepare(
        "SELECT run_id,target_date,created_at FROM report_runs WHERE scope_id=?
         AND run_kind='report' AND status='running' AND finished_at IS NULL AND error_code IS NULL
         AND phase IN ('created','scanning','prepared','generating','db_committed')",
    )?;
    let rows = statement.query_map([&config.report.scope_id], |row| row.try_into())?;
    let mut active = Vec::new();
    for row in rows {
        let (id, date, created_at): (String, String, String) = row?;
        if DateTime::parse_from_rfc3339(&created_at).is_ok_and(|started| {
            (now - started.with_timezone(&Utc))
                .to_std()
                .is_ok_and(|elapsed| elapsed.as_secs() < config.recovery.run_budget_seconds)
        }) {
            active.push((id, date));
        }
    }
    Ok(active)
}

fn historical_attention(
    connection: &Connection,
    config: &AppConfig,
    now: DateTime<Utc>,
    active: &[(String, String)],
) -> Result<HealthCheck, ReportError> {
    let mut issues = crate::personal_recovery::issues(connection, &config.report.scope_id, now)?;
    issues.retain(|issue| {
        issue.reason_codes.as_slice() != ["REPORT_NOT_PUBLISHED"]
            || !active.iter().any(|(id, _)| id == &issue.run_id)
    });
    let missing = issues.iter().filter(|issue| issue.missing_summary).count();
    let (level, message) = if issues.is_empty() {
        ("ok", "没有未恢复的个人日报异常".into())
    } else {
        let level = if missing > 0 { "error" } else { "warning" };
        let count = issues.len();
        (
            level,
            format!(
                "{count} 个历史日报仍需恢复，其中 {missing} 个缺少有效总结；告警保留至有效发布完成"
            ),
        )
    };
    Ok(HealthCheck {
        code: "HISTORICAL_REPORT_ATTENTION",
        level,
        message,
        evidence: json!({"reports":issues}),
    })
}

fn run_attention(
    connection: &Connection,
    now: DateTime<Utc>,
    active: &[(String, String)],
) -> Result<HealthCheck, ReportError> {
    let mut pending_statement = connection.prepare(
        "SELECT run_id FROM report_runs r WHERE run_kind='report' AND phase IN ('prepared','generating','db_committed') AND status!='succeeded' AND COALESCE(error_code,'')!='RUN_SUPERSEDED' AND COALESCE(regeneration_reason,'') NOT LIKE 'coverage_publish:%'
         AND NOT EXISTS(SELECT 1 FROM daily_reports d JOIN report_runs p ON p.run_id=d.current_run_id
             WHERE p.scope_id=r.scope_id AND p.target_date=r.target_date AND p.created_at>r.created_at
             AND p.status='succeeded' AND p.phase='published'
             AND (COALESCE(json_extract(p.report_json,'$.schema_version'),0) NOT IN (5,6)
                 OR json_array_length(p.report_json,'$.items')>0 OR p.quality='no_changes'))",
    )?;
    let pending = pending_statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|id| !active.iter().any(|(active_id, _)| active_id == id))
        .count();
    let cutoff = now - Duration::hours(24);
    // A later publication for the same scope and date resolves an earlier failure.
    let mut statement = connection.prepare(
        "SELECT r.created_at, (
             SELECT MAX(COALESCE(p.finished_at,p.created_at)) FROM report_runs p
             WHERE p.scope_id=r.scope_id AND p.target_date=r.target_date
               AND p.run_kind='report' AND p.status='succeeded' AND p.phase='published'
               AND (COALESCE(json_extract(p.report_json,'$.schema_version'),0) NOT IN (5,6)
                   OR json_array_length(p.report_json,'$.items')>0 OR p.quality='no_changes')
         ) FROM report_runs r WHERE r.status='failed'",
    )?;
    let rows = statement.query_map([], |row| row.try_into())?;
    let mut recent_failures = 0usize;
    let mut resolved_failures = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for row in rows {
        let (text, published): (String, Option<String>) = row?;
        let Ok(at) = DateTime::parse_from_rfc3339(&text) else {
            continue;
        };
        let at = at.with_timezone(&Utc);
        if at < cutoff {
            continue;
        }
        let resolved = published
            .and_then(|published_at| DateTime::parse_from_rfc3339(&published_at).ok())
            .is_some_and(|published_at| published_at > at);
        if resolved {
            resolved_failures += 1;
            continue;
        }
        recent_failures += 1;
        if failures.len() < 5 {
            failures.push(text);
        }
    }
    let mut evidence = json!({
        "pending": pending,
        "recent_failures": recent_failures,
        "resolved_failures": resolved_failures,
        "failures": failures,
    });
    if pending == 0 && recent_failures == 0 {
        evidence["active"] = json!(active.len());
        let message = if resolved_failures == 0 {
            format!(
                "运行预算内 {} 个，没有待续运行，近 24 小时没有失败运行",
                active.len()
            )
        } else {
            format!("没有待续运行；近 24 小时的 {resolved_failures} 个失败已由更晚的正式发布恢复")
        };
        return Ok(HealthCheck::new("ok", "RUN_ATTENTION", message, evidence));
    }
    let mut message = format!("待续运行 {pending} 个，近 24 小时失败运行 {recent_failures} 个");
    if resolved_failures > 0 {
        message.push_str(&format!(
            "（另有 {resolved_failures} 个失败已由更晚的正式发布恢复）"
        ));
    }
    Ok(HealthCheck::warning("RUN_ATTENTION", message, evidence))
}

fn disk_space(config: &AppConfig) -> HealthCheck {
    let Some(bytes) = free_bytes(&config.paths.data_dir) else {
        return HealthCheck::warning("DISK_SPACE", "无法读取数据目录所在磁盘剩余空间", json!({}));
    };
    let message = format!(
        "数据目录所在磁盘剩余 {:.1} GiB",
        bytes as f64 / 1024_f64.powi(3)
    );
    let evidence = json!({"free_bytes": bytes});
    if bytes < DISK_ERROR_BYTES {
        HealthCheck::error("DISK_SPACE", message + "，低于 5 GiB", evidence)
    } else if bytes < DISK_WARN_BYTES {
        HealthCheck::warning("DISK_SPACE", message + "，低于 20 GiB", evidence)
    } else {
        HealthCheck::ok("DISK_SPACE", message)
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
    let reports_path = config.reports_db_path();
    let reports = match open_read(&reports_path) {
        Ok(connection) => {
            match quick_check_yielding(&connection, &reports_path, &config.report.scope_id) {
                QuickCheckOutcome::Ok => Some(connection),
                QuickCheckOutcome::Deferred => {
                    if connection
                        .query_row("SELECT COUNT(*) FROM report_schema_migrations", [], |row| {
                            row.get::<_, i64>(0)
                        })
                        .is_ok()
                    {
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
                QuickCheckOutcome::Failed => {
                    checks.push(HealthCheck::error(
                        "DATABASE_INTEGRITY",
                        "报告库完整性检查未通过",
                        json!({"path": display_path(&reports_path)}),
                    ));
                    None
                }
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
    if let Some(connection) = reports {
        let mut pending_published = None;
        let mut published_err = None;
        let mut after_published = Vec::new();
        let observe = (|| -> Result<(), ReportError> {
            let tx = connection.unchecked_transaction()?;
            let active = active_runs(&tx, config, now)?;
            match fetch_published_report(&tx, config, target, now, &active) {
                Ok(query) => pending_published = Some(query),
                Err(_) => {
                    published_err = Some(HealthCheck::error(
                        "DATABASE_INTEGRITY",
                        format!("{target} 日报无法核对：报告库结构不可用"),
                        json!({"target_date": target}),
                    ))
                }
            }
            match run_attention(&tx, now, &active) {
                Ok(check) => after_published.push(check),
                Err(_) => after_published.push(HealthCheck::error(
                    "DATABASE_INTEGRITY",
                    "运行状态无法核对：报告库结构不可用",
                    json!({}),
                )),
            }
            if config.report.profile == crate::config::ReportProfile::Personal {
                match historical_attention(&tx, config, now, &active) {
                    Ok(check) => after_published.push(check),
                    Err(_) => after_published.push(HealthCheck::error(
                        "DATABASE_INTEGRITY",
                        "历史日报异常无法核对",
                        json!({}),
                    )),
                }
            }
            let partial: Option<String> = tx.query_row(
                "SELECT r.report_json FROM report_runs r JOIN daily_reports d ON d.current_run_id=r.run_id WHERE d.date=? AND r.quality='partial'",
                [target.to_string()], |r|r.get(0)).optional().ok().flatten();
            if let Some(report) = partial {
                let report = read_frozen_report_json(&report).unwrap_or_default();
                after_published.push(HealthCheck::warning("REPORT_CONTENT_PARTIAL", "日报已发布，内容仅部分可用",
                    json!({"quality":"partial","selection":report["selection"],"gaps":report["gaps"]})));
            }
            Ok(())
        })();
        drop(connection);
        if observe.is_err() {
            checks.push(HealthCheck::error(
                "DATABASE_INTEGRITY",
                "运行状态无法核对：报告库结构不可用",
                json!({}),
            ));
        } else {
            if let Some(err_check) = published_err {
                checks.push(err_check);
            } else if let Some(query) = pending_published {
                checks.push(verify_published_files(config, query)?);
            }
            checks.extend(after_published);
        }
    } else {
        checks.push(HealthCheck::error(
            "REPORT_NOT_PUBLISHED",
            format!("{target} 日报无法核对：报告库不可用"),
            json!({"target_date": target}),
        ));
    }

    checks.push(match open_read(&config.index_db_path) {
        Ok(connection)
            if quick_check(&connection)
                && crate::scan::cache::verify_schema(&connection).is_ok() =>
        {
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
        let root = config.daily_cache_dir();
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
            evidence:json!({"path":display_path(&root),"logical_bytes":bytes,"complete":complete,"max_bytes":crate::processing_cache::MAX_BYTES,"retention_days":crate::processing_cache::RETENTION_DAYS}),
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

/// 原子写入健康 JSON：临时文件与目标同目录，显式 UTF-8，刷新后替换。
pub fn write_report(report: &HealthReport, path: &Path) -> Result<(), ReportError> {
    let mut bytes = serde_json::to_vec_pretty(report)?;
    bytes.push(b'\n');
    write_atomic(
        path,
        &bytes,
        "HEALTH_PATH_REDIRECTED",
        "HEALTH_PATH_INVALID",
        "健康文件路径无效",
    )
}

/// 按 generated_at 的上海日期写入 `log_dir/health-YYYY-MM-DD.json`。
pub fn persist(report: &HealthReport) -> Result<PathBuf, ReportError> {
    let generated = DateTime::parse_from_rfc3339(&report.generated_at)
        .map_err(|_| ReportError::new("HEALTH_TIMESTAMP_INVALID", "健康结果生成时间无效"))?;
    let day = generated.with_timezone(&types::shanghai()).date_naive();
    let directory = Path::new(&report.log_dir);
    let path = directory.join(format!("health-{day}.json"));
    reject_redirects(&path, "HEALTH_PATH_REDIRECTED")?;
    fs::create_dir_all(directory)?;
    // health_notify.ps1 takes the same OS file lock while enriching this observation.
    let lock_path = directory.join(".health-publish.lock");
    reject_redirects(&lock_path, "HEALTH_PATH_REDIRECTED")?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    let wait = std::time::Instant::now();
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if wait.elapsed().as_secs() < 15 => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(ReportError::new(
                    "HEALTH_PUBLISH_BUSY",
                    "另一项健康检查正在写入结果",
                ));
            }
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    match fs::read(&path) {
        Ok(bytes) => {
            let previous = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    DateTime::parse_from_rfc3339(value["generated_at"].as_str()?).ok()
                });
            if previous.is_some_and(|previous| previous >= generated) {
                return Ok(path);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
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

pub fn render(report: &HealthReport, format: crate::cli::Format) -> String {
    match format {
        crate::cli::Format::Json => {
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into())
        }
        crate::cli::Format::Text => {
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
