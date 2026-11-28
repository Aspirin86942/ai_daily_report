//! Windows task definitions and explicit operational checks.
pub mod rollback;

pub use crate::infra::fs::display_path;
use crate::{
    config::AppConfig,
    infra::{
        fs::{reject_redirects, write_synced},
        json::fingerprint,
        locking::ReportLock,
        sqlite::open_read_only,
    },
    run::App,
    scan::Scanner,
    store::queries::{has_verified_model_check, run_phase as query_run_phase},
    types, ReportError,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
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

fn calendar_trigger(id: &str, date: chrono::NaiveDate, time: &str) -> String {
    format!("<CalendarTrigger id=\"{id}\"><StartBoundary>{date}T{time}:00+08:00</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>")
}

fn validate_sid(sid: &str) -> Result<(), ReportError> {
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
    Ok(())
}

enum TaskKind {
    Daily,
    Health,
}

pub fn task_plan(config: &AppConfig, sid: &str, now: DateTime<Utc>) -> Result<Value, ReportError> {
    if !config.personal_scan() {
        return Err(ReportError::new(
            "FEATURE_RETIRED",
            "旧通用日报任务已退役，请使用 personal 配置；历史报告仍可查询和恢复",
        ));
    }
    if u64::from(config.schedule.execution_limit_minutes) * 60
        < config.recovery.run_budget_seconds.saturating_add(60)
    {
        return Err(ReportError::new(
            "TASK_RUNTIME_LIMIT_TOO_SHORT",
            "日报任务时限须覆盖程序运行预算及至少一分钟的状态保存时间。",
        ));
    }
    task_definition(config, sid, now, TaskKind::Daily)
}

pub fn task_plan_health(
    config: &AppConfig,
    sid: &str,
    now: DateTime<Utc>,
) -> Result<Value, ReportError> {
    task_definition(config, sid, now, TaskKind::Health)
}

fn task_definition(
    config: &AppConfig,
    sid: &str,
    now: DateTime<Utc>,
    kind: TaskKind,
) -> Result<Value, ReportError> {
    validate_sid(sid)?;
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
    let program = display_path(&program);
    let date = now.with_timezone(&types::shanghai()).date_naive();
    let schedule = &config.schedule;
    let (task_name, command, arguments, triggers, battery, stop, wake, limit) = match kind {
        TaskKind::Daily => (
            schedule.task_name.clone(),
            program.as_str(),
            format!("--config-dir \"{config_dir}\" daily --scheduled"),
            calendar_trigger("DailyShanghai", date, &schedule.local_time),
            !schedule.allow_on_battery,
            schedule.stop_on_battery,
            format!("<WakeToRun>{}</WakeToRun>", schedule.wake_to_run),
            schedule.execution_limit_minutes,
        ),
        TaskKind::Health => {
            let script = display_path(&config.program_dir.join("scripts/health_notify.ps1"));
            let arguments = format!(
                "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{script}\" -Program \"{program}\" -ConfigDirectory \"{config_dir}\""
            );
            let triggers = calendar_trigger("HealthMorning", date, "11:00")
                + &calendar_trigger("HealthEvening", date, "23:00");
            (
                format!("{} Health", schedule.task_name),
                "powershell.exe",
                arguments,
                triggers,
                false,
                false,
                String::new(),
                10,
            )
        }
    };
    let document = format!(
        r#"<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><RegistrationInfo><Description>{owner}</Description><Author>{sid}</Author></RegistrationInfo><Triggers>{triggers}</Triggers><Principals><Principal id="DailyReportUser"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>{battery}</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>{stop}</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable>{wake}<AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><ExecutionTimeLimit>PT{limit}M</ExecutionTimeLimit></Settings><Actions Context="DailyReportUser"><Exec><Command>{program}</Command><Arguments>{arguments}</Arguments><WorkingDirectory>{root}</WorkingDirectory></Exec></Actions></Task>"#,
        owner = xml(&owner),
        sid = xml(sid),
        program = xml(command),
        arguments = xml(&arguments),
        root = xml(&root),
    );
    let db = config.reports_db_path();
    let verified = if matches!(kind, TaskKind::Daily) && db.exists() {
        let conn = open_read_only(
            &db,
            Duration::from_secs(5),
            "DATABASE_PATH_REDIRECTED",
            "DATABASE_MISSING",
        )?;
        has_verified_model_check(&conn, &config.fingerprint(false)?)?
    } else {
        false
    };
    let mut plan = json!({"project_root":root,"program":program,"config_dir":config_dir,"arguments":arguments,"schedule":schedule,"task_name":task_name,"task_path":"\\","user_sid":sid,"owner_marker":owner,"data_dir":display_path(&config.paths.data_dir),"work_dir":display_path(&config.paths.work_dir),"reports_dir":display_path(&config.paths.reports_dir),"credential_present":!config.api_key().is_empty(),"capabilities_configured":config.llm.context_window_tokens.is_some()&&config.llm.verified_max_output_tokens.is_some(),"model_check_verified":verified,"xml":document});
    if matches!(kind, TaskKind::Health) {
        plan["kind"] = json!("health");
    }
    Ok(plan)
}

pub fn task_plan_kind(
    config: &AppConfig,
    sid: &str,
    now: DateTime<Utc>,
    kind: &str,
) -> Result<Value, ReportError> {
    let mut plan = match kind {
        "daily" => task_plan(config, sid, now),
        "health" => task_plan_health(config, sid, now),
        _ => Err(ReportError::new("TASK_KIND_INVALID", "任务类型无效")),
    }?;
    plan["llm"] = serde_json::to_value(&config.llm)?;
    plan["endpoint"] = json!(config.endpoint());
    plan["config_fingerprint"] = json!(config.fingerprint(false)?);
    plan["report_profile"] = json!(config.report.profile);
    plan["processing_dir"] = json!(display_path(&config.paths.data_dir.join(
        if config.report.profile == crate::config::ReportProfile::Personal {
            types::DAILY_CACHE_DIR
        } else {
            "archive"
        }
    )));
    plan["base_task_name"] = json!(config.schedule.task_name);
    plan["execution_scope"] = json!(fingerprint(&json!({
        "profile":config.report.profile,"work_dir":config.paths.work_dir,"data_dir":config.paths.data_dir,
        "roots":config.archive.roots,"model_roots":config.archive.model_roots,
        "allow_work_dir_for_model":config.archive.allow_work_dir_for_model,
        "model_ignored_prefixes":config.archive.model_ignored_prefixes,
        "allowed_extensions":config.scanner.allowed_extensions,
        "excluded_dirs":config.scanner.excluded_dirs,"ignored_patterns":config.scanner.ignored_patterns
    }))?);
    Ok(plan)
}

pub const TEMPLATES: &[(&str, &str)] = &[
    (
        "daily_prompt.md",
        include_str!("../../../templates/daily_prompt.md"),
    ),
    (
        "daily_compact_prompt.md",
        include_str!("../../../templates/daily_compact_prompt.md"),
    ),
    (
        "daily_render_template.md",
        include_str!("../../../templates/daily_render_template.md"),
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
        write_synced(file.as_file_mut(), b"doctor")?;
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
    let scanner = Scanner::open(app.config.scanner_config()).map_err(|error| {
        ReportError::new("SCANNER_UNAVAILABLE", format!("扫描库初始化失败。{error}"))
    })?;
    let result = scanner.doctor().map_err(|error| {
        ReportError::new("SCANNER_DOCTOR_FAILED", format!("扫描自检失败。{error}"))
    })?;
    let model = if check_model {
        model_check(app, now)?
    } else {
        json!({"checked":false,"calls":0})
    };
    Ok(
        json!({"status":"ok","strict":true,"runtime":"rust","build":env!("AI_DAILY_REPORT_BUILD"),"scanner":result,
            "parser_content_identity":crate::scan::PARSER_CONTENT_IDENTITY,"templates":TEMPLATES.len(),"database":"ok","model":model}),
    )
}

pub fn model_check(app: &App, now: DateTime<Utc>) -> Result<Value, ReportError> {
    use crate::{
        model::ModelRunner,
        reports::{
            data::ReportSource,
            personal::{Material, Selection},
        },
    };
    let _lock = ReportLock::acquire(
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
    let text = "这是虚构的接口检查材料：一份采购审批方案处于待评审草稿状态，尚未生效。";
    let mut selection = Selection::default();
    selection.summary.selected_chars = text.chars().count();
    selection.materials.push(Material {
        source: ReportSource::user(text, now),
        text: text.into(),
        evidence: vec![],
        priority: 0,
    });
    let plan = crate::run::personal::prepare_model_check(app, &context, selection)?;
    let personal_snapshots = app
        .processing
        .as_ref()
        .map(|_| crate::run::personal::snapshots(app))
        .transpose()?;
    let snapshots = personal_snapshots.as_ref().unwrap_or(&app.snapshots);
    app.store
        .create_personal(&context, "model_check", &context.run_id, None, now)?;
    let result = (|| {
        let snapshot = snapshots.put(
            &context.run_id,
            "input",
            &json!({"version":3,"context":context,"plan":plan,"purpose":"model_check"}),
        )?;
        app.store
            .prepare(&context.run_id, &snapshot, &snapshot.sha256, &[], now)?;
        let started = Instant::now();
        let runner = ModelRunner {
            store: &app.store,
            snapshots,
            transport: app.transport.as_ref(),
            llm: &app.config.llm,
            allow_new_calls: true,
            run_id: &context.run_id,
            started_at: now,
            started,
            deadline: started + Duration::from_secs(app.config.recovery.run_budget_seconds),
        };
        let output = runner.execute(&plan)?.output;
        if output.items.is_empty() {
            return Err(ReportError::new(
                "MODEL_CHECK_EMPTY",
                "模型检查没有返回有效事项",
            ));
        }
        app.store
            .finish_model_check(&context.run_id, &serde_json::to_value(output.items)?, now)?;
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
    let path = config.reports_db_path();
    let Ok(connection) = open_read_only(
        &path,
        Duration::from_secs(1),
        "DATABASE_PATH_REDIRECTED",
        "DATABASE_MISSING",
    ) else {
        return "unknown".into();
    };
    query_run_phase(&connection, run_id)
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
    let now = Utc::now();
    let local_day = now.with_timezone(&types::shanghai()).date_naive();
    let path = config
        .paths
        .log_dir
        .join(format!("scheduled-{local_day}.jsonl"));
    reject_redirects(&path, "LOG_PATH_REDIRECTED")?;
    let mut event = match result {
        Ok(value) => {
            let action = value["action"].as_str().unwrap_or("");
            let skipped = action == "skipped";
            let status = |key| {
                if skipped {
                    "skipped"
                } else {
                    value[key].as_str().unwrap_or("succeeded")
                }
            };
            let material = value["material_status"]
                .as_str()
                .or(value["quality"].as_str())
                .unwrap_or("unknown");
            let publication = if kind == "daily" {
                if matches!(
                    action,
                    "published" | "existing" | "reused" | "generated" | "repaired"
                ) {
                    "published"
                } else {
                    "not_published"
                }
            } else {
                "not_applicable"
            };
            let reasons = value.get("reason_codes").cloned().unwrap_or_else(|| {
                json!(value["checks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|check| check["level"] != "ok")
                    .filter_map(|check| check["code"].as_str())
                    .collect::<Vec<_>>())
            });
            let field_or = |key, fallback| {
                value
                    .get(key)
                    .filter(|value| !value.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!(fallback))
            };
            let date = field_or("target_date", target_date);
            let mut event = json!({
                "status": status("status"),
                "execution_status": status("execution_status"),
                "material_status": material,
                "publication_status": publication,
                "run_id": field_or("run_id", run_id),
                "date": date,
                "target_date": date,
                "action": value["action"],
                "quality": value["quality"],
                "errors": value["errors"],
                "reason_codes": reasons,
                "phase": "completed",
            });
            if skipped {
                event["reason"] = value["reason"].clone();
                event["model_calls"] = json!(0);
            }
            if let Some(calls) = value.get("model_calls") {
                event["model_calls"] = calls.clone();
            }
            event
        }
        Err(e) => json!({
            "status": "failed",
            "execution_status":"failed", "material_status":"unknown",
            "publication_status":if kind == "daily" { "unknown" } else { "not_applicable" },
            "errors":1, "reason_codes":[e.code],
            "run_id": run_id,
            "target_date": target_date,
            "phase": run_id.map(|id| run_phase(config, id)).unwrap_or_else(|| "unknown".into()),
            "error_code": e.code,
            "message": e.message,
            "retryable": e.retryable(),
        }),
    };
    event["at"] = json!(now);
    event["kind"] = json!(kind);
    event["duration_ms"] = json!(duration_ms);
    event["termination"] = json!("application_exit");
    if config.personal_scan() {
        return personal_log::append(&config.paths.log_dir, event, now);
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{event}")?;
    file.sync_all()?;
    Ok(())
}

mod personal_log;
