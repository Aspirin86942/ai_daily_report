use crate::{
    config::{AppConfig, ReportProfile},
    infra::{fs::display_path, json::fingerprint, sqlite::open_read_only},
    store::queries::has_verified_model_check,
    types, ReportError,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::time::Duration;

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
        if config.report.profile == ReportProfile::Personal {
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
