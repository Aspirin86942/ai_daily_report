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

fn execute(
    command: Command,
    config_dir: Option<&std::path::Path>,
    started: std::time::Instant,
) -> Result<(), ReportError> {
    if let Command::ArchiveScanner {
        source,
        archive_dir,
    } = &command
    {
        println!(
            "{}",
            serde_json::to_string_pretty(&ai_daily_report::operations::archive_scanner(
                source,
                archive_dir,
                chrono::Utc::now()
            )?)?
        );
        return Ok(());
    }
    if matches!(command, Command::Retired(_)) {
        return Err(ReportError::new("FEATURE_RETIRED", "周报、月报、批量补跑、覆盖修复和 OCR 工作流已退役；请使用 daily，历史报告仍可查询和恢复"));
    }
    let executable = std::env::current_exe()?;
    let config = AppConfig::load(executable.parent().unwrap(), config_dir)?;
    let now = chrono::Utc::now();
    match command {
        Command::Archive { format, command } => ai_daily_report::archive::cli::print(
            &ai_daily_report::archive::cli::execute(&config, command)?,
            format,
        ),
        Command::Stats(args) => {
            let value = ai_daily_report::archive::stats::query(
                &config,
                args.from.as_ref().map(ToString::to_string).as_deref(),
                args.to.as_ref().map(ToString::to_string).as_deref(),
                args.run_id.as_deref(),
            )?;
            if let Some(path) = args.files_csv {
                ai_daily_report::archive::stats::write_csv(
                    &config,
                    args.from.as_ref().map(ToString::to_string).as_deref(),
                    args.to.as_ref().map(ToString::to_string).as_deref(),
                    args.run_id.as_deref(),
                    &path,
                )?;
            }
            ai_daily_report::archive::cli::print(&value, args.format)
        }
        Command::ArchiveScanner { .. } => unreachable!(),
        command @ Command::Daily(_) => {
            let scheduled = matches!(&command,Command::Daily(args)if args.scheduled);
            let kind = if matches!(&command, Command::Daily(_)) {
                "daily"
            } else {
                "period"
            };
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
                    kind,
                    started.elapsed().as_millis() as u64,
                    Some(&run_id),
                    target_date.as_deref(),
                    &result,
                )?;
            }
            println!("{}", serde_json::to_string_pretty(&result?)?);
            Ok(())
        }
        Command::Search(args) => {
            let value = ai_daily_report::search::execute(&config, &args)?;
            println!("{}", ai_daily_report::search::render(&value, args.format));
            Ok(())
        }
        Command::List => {
            let app = App::open_history(config, now)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&app.store.list_reports()?)?
            );
            Ok(())
        }
        Command::Runs { target_date, limit } => {
            let app = App::open_history(config, now)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&app.store.list_runs(
                    target_date.as_ref().map(ToString::to_string).as_deref(),
                    limit
                )?)?
            );
            Ok(())
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
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Command::Doctor {
            strict: false,
            check_model: false,
        } => {
            println!("{}", serde_json::to_string_pretty(&config.config_check())?);
            Ok(())
        }
        Command::Retired(_) => unreachable!(),
        Command::Cleanup { execute } => {
            let app = App::open_history(config, now)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&ai_daily_report::maintenance::retention::cleanup(
                    &app, now, execute
                )?)?
            );
            Ok(())
        }
        Command::Backup { output } => {
            let app = App::open_history(config, now)?;
            let _lock = ai_daily_report::store::locking::ReportLock::acquire(
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
            let target = args.target_date.unwrap_or_else(|| {
                (now.with_timezone(&ai_daily_report::types::shanghai()) - chrono::Days::new(1))
                    .date_naive()
            });
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
            command: ai_daily_report::cli::TaskCommand::Plan { sid, kind },
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&ai_daily_report::operations::task_plan_kind(
                    &config, &sid, now, &kind
                )?)?
            );
            Ok(())
        }
        Command::Doctor { check_model, .. } => {
            let app = App::open(config, now)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&ai_daily_report::operations::doctor(
                    &app,
                    check_model,
                    now
                )?)?
            );
            Ok(())
        }
    }
}
