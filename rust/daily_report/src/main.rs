#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
use ai_daily_report::{
    cli::{Cli, Command},
    config::AppConfig,
    run::{repair_output, run_report, App},
    types::new_run_id,
    ReportError,
};
use clap::{CommandFactory, Parser};

fn main() {
    #[cfg(delay_network_imports)]
    {
        use windows_sys::Win32::System::LibraryLoader::{
            SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
        };
        // All delayed imports in the main executable are Windows system components.
        if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } == 0 {
            eprintln!(
                "Windows DLL search initialization failed: {}",
                std::io::Error::last_os_error()
            );
            std::process::exit(1);
        }
    }
    let started = std::time::Instant::now();
    #[cfg(windows)]
    if !std::env::args_os().any(|arg| arg == "--scheduled") {
        // Preserve redirected pipes; attach only when launched without an output handle.
        unsafe {
            use windows_sys::Win32::{
                Foundation::INVALID_HANDLE_VALUE,
                System::Console::{
                    AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
                },
            };
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            if output.is_null() || output == INVALID_HANDLE_VALUE {
                AttachConsole(ATTACH_PARENT_PROCESS);
            }
        }
    }
    let args = Cli::parse();
    let Some(command) = args.command else {
        Cli::command().print_help().unwrap();
        return;
    };
    if let Err(error) = execute(command, args.config_dir.as_deref(), started) {
        eprintln!(
            "{}",
            serde_json::json!({"error_code":error.code,"message":error.message,"retryable":error.retryable()})
        );
        std::process::exit(1);
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), ReportError> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn execute(
    command: Command,
    config_dir: Option<&std::path::Path>,
    started: std::time::Instant,
) -> Result<(), ReportError> {
    if matches!(command, Command::BuildInfo) {
        println!(
            "{}",
            serde_json::json!({
                "engine_build": env!("AI_DAILY_REPORT_BUILD"),
                "parser_content_identity": daily_parsers::PARSER_CONTENT_IDENTITY,
                "protocol_version": daily_parsers::pdf_wire::PDF_PROTOCOL_VERSION,
            })
        );
        return Ok(());
    }
    if matches!(command, Command::Retired(_)) {
        return Err(ReportError::new("FEATURE_RETIRED", "周报、月报、批量补跑、覆盖修复和 OCR 工作流已退役；请使用 daily，历史报告仍可查询和恢复"));
    }
    let executable = std::env::current_exe()?;
    // A captured helper resolves relative paths against the installation that used them.
    let program_dir = match &command {
        Command::Task {
            command:
                ai_daily_report::cli::TaskCommand::RollbackGuard {
                    candidate_root: Some(root),
                    ..
                },
        } => root.as_path(),
        _ => executable.parent().unwrap(),
    };
    let config = AppConfig::load(program_dir, config_dir)?;
    let now = chrono::Utc::now();
    match command {
        command @ Command::Daily(_) => {
            let scheduled = matches!(&command,Command::Daily(args)if args.scheduled);
            let run_id = new_run_id();
            let mut target_date: Option<String> = None;
            let result = (|| {
                let request =
                    command.report_request(&config, now, run_id.clone(), String::new())?;
                target_date = Some(request.context.target_date.to_string());
                let app = App::open_started(config.clone(), now, started)?;
                let value = serde_json::to_value(run_report(request, &app)?)?;
                Ok(value)
            })();
            if scheduled {
                ai_daily_report::operations::append_scheduled_log(
                    &config,
                    "daily",
                    started.elapsed().as_millis() as u64,
                    Some(&run_id),
                    target_date.as_deref(),
                    &result,
                )?;
            }
            print_json(&result?)
        }
        Command::Search(args) => {
            let value = ai_daily_report::search::execute(&config, &args)?;
            println!("{}", ai_daily_report::search::render(&value, args.format));
            Ok(())
        }
        Command::List => {
            let app = App::open_history(config, now)?;
            print_json(&app.store.list_reports()?)
        }
        Command::Runs { target_date, limit } => {
            let app = App::open_history(config, now)?;
            print_json(&app.store.list_runs(
                target_date.as_ref().map(ToString::to_string).as_deref(),
                limit,
            )?)
        }
        Command::RepairOutput {
            target_date,
            run_id,
        } => {
            let app = App::open_history(config, now)?;
            let result = match (target_date, run_id) {
                (Some(date), None) => repair_output(&app, &date.to_string(), now)?,
                (None, Some(id)) => ai_daily_report::run::repair_by_id(&app, &id, now)?,
                _ => {
                    return Err(ReportError::new(
                        "COMMAND_INVALID",
                        "选择目标日期或运行编号",
                    ))
                }
            };
            print_json(&result)
        }
        Command::Doctor {
            strict: false,
            check_model: false,
        } => print_json(&config.config_check()),
        Command::Retired(_) | Command::BuildInfo => unreachable!(),
        Command::Cleanup { execute } => {
            let app = App::open_history(config, now)?;
            print_json(&ai_daily_report::maintenance::retention::cleanup(
                &app, now, execute,
            )?)
        }
        Command::Backup { output } => {
            let app = App::open_history(config, now)?;
            let _lock = ai_daily_report::infra::locking::ReportLock::acquire(
                &app.store.path,
                &app.config.report.scope_id,
                std::time::Duration::from_secs(5),
            )?;
            println!(
                "{}",
                serde_json::json!({"path":output,"sha256":app.store.backup(&output)?})
            );
            Ok(())
        }
        Command::Health(args) => {
            let target = match args.target_date {
                Some(target) => target,
                None => ai_daily_report::types::target_date(
                    now,
                    None,
                    true,
                    config.schedule.due_time()?,
                )?,
            };
            let started = std::time::Instant::now();
            let report = ai_daily_report::health::check(&config, target, now)?;
            if args.scheduled {
                ai_daily_report::health::persist(&report)?;
            }
            if let Some(path) = &args.output {
                ai_daily_report::health::write_report(&report, path)?;
            }
            if args.scheduled {
                let value = serde_json::to_value(&report)?;
                ai_daily_report::operations::append_scheduled_log(
                    &config,
                    "health",
                    started.elapsed().as_millis() as u64,
                    None,
                    Some(&target.to_string()),
                    &Ok(value),
                )?;
            }
            println!("{}", ai_daily_report::health::render(&report, args.format));
            let code = ai_daily_report::health::exit_code(&report);
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Task {
            command:
                ai_daily_report::cli::TaskCommand::RollbackGuard {
                    previous_config_dir,
                    previous_root,
                    identity_only,
                    ..
                },
        } => {
            use ai_daily_report::operations::rollback;
            let prior_config = previous_config_dir.as_deref();
            let prior_root = previous_root.as_deref();
            if identity_only {
                let identity = rollback::target_identity(&config, prior_config, prior_root)?;
                println!("{}", serde_json::json!({"target_sha256": identity}));
                Ok(())
            } else {
                rollback::guard(&config, prior_config, prior_root)
            }
        }
        Command::Task {
            command: ai_daily_report::cli::TaskCommand::Plan { sid, kind },
        } => print_json(&ai_daily_report::operations::task_plan_kind(
            &config, &sid, now, &kind,
        )?),
        Command::Doctor { check_model, .. } => {
            let app = App::open(config, now)?;
            print_json(&ai_daily_report::operations::doctor(
                &app,
                check_model,
                now,
            )?)
        }
    }
}
