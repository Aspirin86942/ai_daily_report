use crate::{
    infra::{
        fs::{reject_redirects, write_synced},
        locking::ReportLock,
    },
    run::App,
    scan::Scanner,
    types, ReportError,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    fs,
    time::{Duration, Instant},
};

pub const TEMPLATES: &[(&str, &str)] = &[
    (
        "daily_prompt.md",
        include_str!("../../../../templates/daily_prompt.md"),
    ),
    (
        "daily_compact_prompt.md",
        include_str!("../../../../templates/daily_compact_prompt.md"),
    ),
    (
        "daily_render_template.md",
        include_str!("../../../../templates/daily_render_template.md"),
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
