//! Windows task definitions and explicit operational checks.
use crate::{config::AppConfig, run::App, snapshots::reject_redirects, types, ReportError};
use ai_daily_scanner_contract::AdapterPaths;
use ai_daily_scanner_core::{Scanner, ScannerConfig};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(&value).into()
    }
}

pub fn archive_scanner(
    source: &Path,
    archive: &Path,
    now: DateTime<Utc>,
) -> Result<Value, ReportError> {
    reject_redirects(source, "DATABASE_PATH_REDIRECTED")?;
    reject_redirects(archive, "BACKUP_PATH_REDIRECTED")?;
    let source = fs::canonicalize(source)?;
    let archive = fs::canonicalize(archive)?;
    if !source.is_file() || !archive.is_dir() {
        return Err(ReportError::new(
            "BACKUP_PATH_INVALID",
            "数据库文件和归档目录必须存在",
        ));
    }
    let before = crate::store::file_sha(&source)?;
    let conn = Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(ReportError::new(
            "DATABASE_INTEGRITY_FAILED",
            "原数据库完整性检查失败",
        ));
    }
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let name = format!(
        "{}-{}-{}.sqlite3",
        source.file_stem().unwrap().to_string_lossy(),
        now.format("%Y%m%dT%H%M%SZ"),
        types::new_run_id()
    );
    let target = archive.join(&name);
    // VACUUM INTO copies a consistent SQLite snapshot, including committed WAL data.
    conn.execute("VACUUM INTO ?", [target.to_string_lossy().as_ref()])?;
    let copy = Connection::open_with_flags(&target, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let check: String = copy.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" || crate::store::file_sha(&source)? != before {
        return Err(ReportError::new(
            "BACKUP_SOURCE_CHANGED",
            "归档检查失败，原库保持不变；请检查未登记的归档文件",
        ));
    }
    drop(copy);
    let mut permissions = fs::metadata(&target)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&target, permissions)?;
    let value = json!({"schema_version":"ai_daily_scanner_db_archive_v1","created_at_utc":now,"source_name":source.file_name().unwrap().to_string_lossy(),"source_sha256":before,"source_user_version":version,"archive_name":name,"archive_size":fs::metadata(&target)?.len(),"archive_sha256":crate::store::file_sha(&target)?,"sqlite_backup_api":false,"backup_method":"sqlite_vacuum_into"});
    crate::maintenance::write_immutable(
        &archive.join(format!("{name}.manifest.json")),
        &crate::snapshots::canonical_json(&value)?,
    )?;
    Ok(value)
}

fn trigger_times(local_time: &str) -> Result<[String; 3], ReportError> {
    let time = chrono::NaiveTime::parse_from_str(local_time, "%H:%M")
        .map_err(|_| ReportError::new("TASK_TIME_INVALID", "计划时间无效"))?;
    let at = |minutes: i64| {
        (time + chrono::Duration::minutes(minutes))
            .format("%H:%M")
            .to_string()
    };
    Ok([time.format("%H:%M").to_string(), at(30), at(90)])
}

fn calendar_trigger(id: &str, date: chrono::NaiveDate, time: &str) -> String {
    format!("<CalendarTrigger id=\"{id}\"><StartBoundary>{date}T{time}:00+08:00</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>")
}

fn logon_trigger(config: &AppConfig, sid: &str) -> String {
    if config.schedule.logon_catchup {
        format!("<LogonTrigger id=\"LogonCatchup\"><Enabled>true</Enabled><UserId>{}</UserId><Delay>PT1M</Delay></LogonTrigger>", xml(sid))
    } else {
        String::new()
    }
}

fn daily_triggers(config: &AppConfig, sid: &str, now: DateTime<Utc>) -> Result<String, ReportError> {
    let date = now.with_timezone(&types::shanghai()).date_naive();
    let times = trigger_times(&config.schedule.local_time)?;
    Ok(format!(
        "{}{}{}{}",
        calendar_trigger("DailyShanghai", date, &times[0]),
        calendar_trigger("DailyCatchup30", date, &times[1]),
        calendar_trigger("DailyCatchup90", date, &times[2]),
        logon_trigger(config, sid)
    ))
}

fn collect_triggers(config: &AppConfig, sid: &str, now: DateTime<Utc>) -> String {
    let date = now.with_timezone(&types::shanghai()).date_naive();
    format!(
        "<CalendarTrigger id=\"Collect30Minutes\"><StartBoundary>{date}T00:00:00+08:00</StartBoundary><Enabled>true</Enabled><Repetition><Interval>PT30M</Interval><Duration>P1D</Duration><StopAtDurationEnd>false</StopAtDurationEnd></Repetition><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>{}",
        logon_trigger(config, sid)
    )
}

fn task_plan_inner(
    config: &AppConfig,
    sid: &str,
    task_name: &str,
    arguments: &str,
    triggers: &str,
) -> Result<Value, ReportError> {
    let pieces: Vec<_> = sid.split('-').collect();
    if pieces.len() < 4
        || pieces[0] != "S"
        || pieces[1] != "1"
        || pieces[2..]
            .iter()
            .any(|s| s.is_empty() || s.parse::<u64>().is_err())
    {
        return Err(ReportError::new("TASK_SID_INVALID", "用户 SID 无效"));
    }
    let program = config.program_dir.join("ai-daily-report.exe");
    if !program.is_file() {
        return Err(ReportError::new(
            "TASK_RUNTIME_MISSING",
            "程序目录缺少 ai-daily-report.exe",
        ));
    }
    let root = display_path(&config.program_dir);
    let config_dir = display_path(&config.config_dir);
    if config_dir.contains(['"', '\r', '\n']) {
        return Err(ReportError::new("TASK_PATH_INVALID", "配置路径无效"));
    }
    // Ownership follows the persistent configuration, so updating a release keeps the same owner.
    let owner = format!(
        "AI Daily Report | checkout={}",
        display_path(config.config_dir.parent().unwrap())
    );
    let schedule = &config.schedule;
    let restart = if schedule.restart_count > 0 {
        format!(
            "<RestartOnFailure><Interval>PT{}M</Interval><Count>{}</Count></RestartOnFailure>",
            schedule.restart_interval_minutes, schedule.restart_count
        )
    } else {
        String::new()
    };
    let document = format!(
        r#"<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><RegistrationInfo><Description>{owner}</Description><Author>{sid}</Author></RegistrationInfo><Triggers>{triggers}</Triggers><Principals><Principal id="DailyReportUser"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>{battery}</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>{stop}</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><WakeToRun>{wake}</WakeToRun><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><ExecutionTimeLimit>PT{limit}M</ExecutionTimeLimit>{restart}</Settings><Actions Context="DailyReportUser"><Exec><Command>{program}</Command><Arguments>{arguments}</Arguments><WorkingDirectory>{root}</WorkingDirectory></Exec></Actions></Task>"#,
        owner = xml(&owner),
        sid = xml(sid),
        triggers = triggers,
        battery = !schedule.allow_on_battery,
        stop = schedule.stop_on_battery,
        wake = schedule.wake_to_run,
        limit = schedule.execution_limit_minutes,
        program = xml(&display_path(&program)),
        arguments = xml(arguments),
        root = xml(&root)
    );
    let db = config.paths.db_dir.join("reports.sqlite3");
    let verified = if db.exists() {
        reject_redirects(&db, "DATABASE_PATH_REDIRECTED")?;
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.query_row("SELECT 1 FROM report_runs WHERE run_kind='model_check' AND status='succeeded' AND config_fingerprint=? LIMIT 1",[config.fingerprint(false)?],|row|row.get::<_,i64>(0)).optional()?.is_some()
    } else {
        false
    };
    Ok(
        json!({"project_root":root,"program":display_path(&program),"config_dir":config_dir,"arguments":arguments,"schedule":schedule,"task_name":task_name,"task_path":"\\","user_sid":sid,"owner_marker":owner,"data_dir":display_path(&config.paths.data_dir),"work_dir":display_path(&config.paths.work_dir),"reports_dir":display_path(&config.paths.reports_dir),"credential_present":!config.api_key().is_empty(),"capabilities_configured":config.llm.context_window_tokens.is_some()&&config.llm.verified_max_output_tokens.is_some(),"model_check_verified":verified,"xml":document}),
    )
}

pub fn task_plan(config: &AppConfig, sid: &str, now: DateTime<Utc>) -> Result<Value, ReportError> {
    let config_dir = display_path(&config.config_dir);
    let arguments = format!("--config-dir \"{config_dir}\" daily --scheduled");
    let triggers = daily_triggers(config, sid, now)?;
    task_plan_inner(config, sid, &config.schedule.task_name, &arguments, &triggers)
}

pub fn task_plan_kind(
    config: &AppConfig,
    sid: &str,
    now: DateTime<Utc>,
    kind: &str,
) -> Result<Value, ReportError> {
    match kind {
        "daily" => task_plan(config, sid, now),
        "collect" => task_plan_collect(config, sid, now),
        _ => Err(ReportError::new("TASK_KIND_INVALID", "任务类型无效")),
    }
}

fn task_plan_collect(
    config: &AppConfig,
    sid: &str,
    now: DateTime<Utc>,
) -> Result<Value, ReportError> {
    let mut collection = config.clone();
    collection.schedule.task_name = format!("{} Collect", config.schedule.task_name);
    collection.schedule.local_time = "00:00".into();
    collection.schedule.wake_to_run = false;
    collection.schedule.execution_limit_minutes = 20;
    let config_dir = display_path(&collection.config_dir);
    let arguments = format!("--config-dir \"{config_dir}\" collect --scheduled");
    let triggers = collect_triggers(&collection, sid, now);
    let mut value = task_plan_inner(
        &collection,
        sid,
        &collection.schedule.task_name,
        &arguments,
        &triggers,
    )?;
    value["kind"] = json!("collect");
    value["interval_minutes"] = json!(30);
    Ok(value)
}

pub const TEMPLATES: &[(&str, &str)] = &[
    (
        "evidence_facts_prompt_v4.md",
        include_str!("../../../templates/evidence_facts_prompt_v4.md"),
    ),
    (
        "evidence_compose_prompt_v4.md",
        include_str!("../../../templates/evidence_compose_prompt_v4.md"),
    ),
    (
        "evidence_report_template_v4.md",
        include_str!("../../../templates/evidence_report_template_v4.md"),
    ),
    (
        "evidence_facts_prompt_v3.md",
        include_str!("../../../templates/evidence_facts_prompt_v3.md"),
    ),
    (
        "evidence_compose_prompt_v3.md",
        include_str!("../../../templates/evidence_compose_prompt_v3.md"),
    ),
    (
        "evidence_report_template_v3.md",
        include_str!("../../../templates/evidence_report_template_v3.md"),
    ),
    (
        "evidence_report_template.md",
        include_str!("../../../templates/evidence_report_template.md"),
    ),
    (
        "weekly_prompt.md",
        include_str!("../../../templates/weekly_prompt.md"),
    ),
    (
        "weekly_template.md",
        include_str!("../../../templates/weekly_template.md"),
    ),
    (
        "monthly_prompt.md",
        include_str!("../../../templates/monthly_prompt.md"),
    ),
    (
        "monthly_template.md",
        include_str!("../../../templates/monthly_template.md"),
    ),
];

pub fn doctor(app: &App, check_model: bool, now: DateTime<Utc>) -> Result<Value, ReportError> {
    if !app.config.paths.work_dir.is_dir() {
        return Err(ReportError::new(
            "WORK_DIRECTORY_UNAVAILABLE",
            "工作目录不可访问",
        ));
    }
    crate::model::plan::request_limit(&app.config.llm, app.config.llm.max_tokens)?;
    crate::model::plan::request_limit(&app.config.llm, app.config.llm.facts_output_tokens())?;
    if app.config.api_key().is_empty() {
        return Err(ReportError::new(
            "MODEL_CREDENTIAL_MISSING",
            "模型凭据尚未配置",
        ));
    }
    for path in [
        &app.config.paths.db_dir,
        &app.config.paths.reports_dir,
        &app.config.paths.log_dir,
        &app.snapshots.root,
    ] {
        reject_redirects(path, "PATH_REDIRECTED")?;
        fs::create_dir_all(path)?;
        let mut file = tempfile::NamedTempFile::new_in(path)?;
        file.write_all(b"doctor")?;
        file.as_file().sync_all()?;
    }
    for (name, expected) in TEMPLATES {
        let path = app.config.program_dir.join("templates").join(name);
        if fs::read(&path)
            .map_err(|_| ReportError::new("TEMPLATE_MISSING", format!("缺少模板 {name}")))?
            != expected.as_bytes()
        {
            return Err(ReportError::new(
                "TEMPLATE_BUILD_MISMATCH",
                format!("模板与程序版本不一致：{name}"),
            ));
        }
    }
    let integrity: String = app
        .store
        .connection()?
        .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(ReportError::new(
            "DATABASE_INTEGRITY_FAILED",
            "报告数据库完整性检查失败",
        ));
    }
    let scanner = Scanner::open(ScannerConfig {
        work_dir: app.config.paths.work_dir.to_string_lossy().into(),
        scan_db_path: app.config.index_db_path.to_string_lossy().into(),
        scanner_settings: app.config.scanner.clone(),
        adapters: AdapterPaths {
            office_worker_path: app.config.office_worker_path.to_string_lossy().into(),
            pdf_worker_path: app.config.pdf_worker_path.to_string_lossy().into(),
        },
    })
    .map_err(|_| ReportError::new("SCANNER_UNAVAILABLE", "扫描库初始化失败"))?;
    let result = scanner
        .doctor()
        .map_err(|_| ReportError::new("SCANNER_DOCTOR_FAILED", "扫描自检失败"))?;
    if result.exit_code != 0 {
        return Err(ReportError::new(
            "WORKER_DOCTOR_FAILED",
            "解析进程握手失败；请核对同版 EXE 和 PDFium DLL",
        ));
    }
    let model = if check_model {
        model_check(app, now)?
    } else {
        json!({"checked":false,"calls":0})
    };
    Ok(
        json!({"status":"ok","strict":true,"runtime":"rust","build":env!("AI_DAILY_REPORT_BUILD"),"scanner":result.value,"templates":TEMPLATES.len(),"database":"ok","model":model}),
    )
}

pub fn model_check(app: &App, now: DateTime<Utc>) -> Result<Value, ReportError> {
    use crate::model::{plan, slicing::SourceInput, ModelRunner, StepOutput};
    let _lock = crate::store::locking::ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs(5),
    )?;
    let context = types::RunContext::new(
        types::new_run_id(),
        now,
        now.with_timezone(&types::shanghai()).date_naive(),
        app.fingerprint(false)?,
    )?;
    let sources = [SourceInput {
        source_id: "S001".into(),
        source_kind: "md".into(),
        path: "synthetic-model-check.md".into(),
        text: "这是虚构的接口检查材料：一份采购审批方案处于待评审草稿状态，尚未生效。".into(),
        units: vec![],
        history_priority: false,
        text_unverified: false,
    }];
    let plan = plan::prepare(
        &app.config.llm,
        json!({"date":context.target_date}),
        &sources,
        json!({"synthetic":true}),
    )?;
    if plan.batches.len() != 1 {
        return Err(ReportError::new(
            "MODEL_CHECK_INPUT_INVALID",
            "检查请求未能形成完整批次",
        ));
    }
    app.store
        .create(&context, "model_check", &context.run_id, None, now)?;
    let result = (|| {
        let snapshot = app.snapshots.put(
            &context.run_id,
            "input",
            &json!({"version":3,"context":context,"plan":plan,"purpose":"model_check"}),
        )?;
        app.store
            .prepare(&context.run_id, &snapshot, &snapshot.sha256, &[], now)?;
        let started = Instant::now();
        let runner = ModelRunner {
            store: &app.store,
            snapshots: &app.snapshots,
            transport: app.transport.as_ref(),
            llm: &app.config.llm,
            recovery: &app.config.recovery,
            recovery_inputs: None,
            allow_new_calls: true,
            run_id: &context.run_id,
            started_at: now,
            started,
            deadline: started + Duration::from_secs(app.config.recovery.run_budget_seconds),
        };
        let output = runner.execute(&plan.batches[0])?;
        let StepOutput::Facts(facts) = output.output else {
            unreachable!()
        };
        if facts.is_empty() {
            return Err(ReportError::new(
                "MODEL_CHECK_EMPTY",
                "模型检查没有返回有效事实",
            ));
        }
        app.store
            .finish_model_check(&context.run_id, &serde_json::to_value(facts)?, now)?;
        Ok(
            json!({"checked":true,"run_id":context.run_id,"calls":app.store.calls(&context.run_id)?.len()}),
        )
    })();
    if let Err(error) = &result {
        app.store.fail(&context.run_id, error.code, false, now)?;
    }
    result
}

fn run_phase(config: &AppConfig, run_id: &str) -> String {
    let path = config.paths.db_dir.join("reports.sqlite3");
    let Ok(connection) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return "unknown".into();
    };
    connection
        .query_row(
            "SELECT phase FROM report_runs WHERE run_id=?",
            [run_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "unknown".into())
}

pub fn append_scheduled_log(
    config: &AppConfig,
    kind: &str,
    duration_ms: u64,
    run_id: Option<&str>,
    target_date: Option<&str>,
    result: &Result<Value, ReportError>,
) -> Result<(), ReportError> {
    fs::create_dir_all(&config.paths.log_dir)?;
    let path = config.paths.log_dir.join(format!(
        "scheduled-{}.jsonl",
        Utc::now()
            .with_timezone(&types::shanghai())
            .format("%Y-%m-%d")
    ));
    reject_redirects(&path, "LOG_PATH_REDIRECTED")?;
    let event = match result {
        Ok(value) => json!({
            "at": Utc::now(),
            "kind": kind,
            "status": "succeeded",
            "run_id": if value["run_id"].is_null() { json!(run_id) } else { value["run_id"].clone() },
            "target_date": if value["target_date"].is_null() { json!(target_date) } else { value["target_date"].clone() },
            "action": value["action"],
            "quality": value["quality"],
            "duration_ms": duration_ms,
        }),
        Err(e) => json!({
            "at": Utc::now(),
            "kind": kind,
            "status": "failed",
            "run_id": run_id,
            "target_date": target_date,
            "phase": run_id.map(|id| run_phase(config, id)).unwrap_or_else(|| "unknown".into()),
            "error_code": e.code,
            "duration_ms": duration_ms,
        }),
    };
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{event}")?;
    file.sync_all()?;
    Ok(())
}
