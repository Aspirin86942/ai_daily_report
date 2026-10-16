#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
use ai_daily_report::{
    cli::{Cli, Command, CoverageCommand},
    config::AppConfig,
    run::{repair_output, run_report, App},
    types::new_run_id,
    ReportError,
};
use clap::{CommandFactory, Parser};

fn main() {
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
    if let Err(error) = execute(command, args.config_dir.as_deref()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn execute(command: Command, config_dir: Option<&std::path::Path>) -> Result<(), ReportError> {
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
    let executable = std::env::current_exe()?;
    let config = AppConfig::load(executable.parent().unwrap(), config_dir)?;
    let now = chrono::Utc::now();
    match command {
        Command::ArchiveScanner { .. } => unreachable!(),
        command @ (Command::Daily(_) | Command::Weekly(_) | Command::Monthly(_)) => {
            let scheduled = matches!(&command,Command::Daily(args)if args.scheduled);
            let result = (|| {
                let request = command.report_request(&config, now, new_run_id(), String::new())?;
                let mut app = App::open(config.clone(), now)?;
                if let Command::Daily(args) = &command {
                    if let Some(path) = &args.ocr_manifest {
                        app.ocr_manifest = ai_daily_report::maintenance::ocr::load_manifest(
                            &app.config,
                            path,
                            &Default::default(),
                        )?;
                    }
                }
                serde_json::to_value(run_report(request, &app)?).map_err(ReportError::from)
            })();
            if scheduled {
                ai_daily_report::operations::append_scheduled_log(&config, &result)?;
            }
            println!("{}", serde_json::to_string_pretty(&result?)?);
            Ok(())
        }
        Command::List => {
            let app = App::open(config, now)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&app.store.list_reports()?)?
            );
            Ok(())
        }
        Command::Runs { target_date, limit } => {
            let app = App::open(config, now)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&app.store.list_runs(
                    target_date.as_ref().map(ToString::to_string).as_deref(),
                    limit
                )?)?
            );
            Ok(())
        }
        Command::RepairOutput { target_date } => {
            let app = App::open(config, now)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&repair_output(&app, &target_date.to_string(), now)?)?
            );
            Ok(())
        }
        Command::Doctor {
            strict: false,
            check_model: false,
        } => {
            println!("{}", serde_json::to_string_pretty(&config.config_check())?);
            Ok(())
        }
        Command::Coverage { command } => {
            use ai_daily_report::maintenance::coverage;
            let mut app = App::open(config, now)?;
            let result = match command {
                CoverageCommand::Plan {
                    dates,
                    gap_codes,
                    scope_root,
                    call_budget,
                    token_budget,
                    cost_budget_microunits,
                    refresh_preflight,
                } => {
                    serde_json::json!({"plan":coverage::make_plan(&app,coverage::PlanRequest{dates:&dates,gap_codes:&gap_codes,scope_root:&scope_root,calls_limit:call_budget,tokens_limit:token_budget,cost_limit_microunits:cost_budget_microunits,refresh:refresh_preflight},now)?})
                }
                CoverageCommand::Generate { plan, workers } => {
                    app.model_workers = workers;
                    serde_json::json!({"candidates":coverage::generate(&app,&plan,now)?})
                }
                CoverageCommand::Publish { candidates } => {
                    coverage::publish(&app, &candidates, now)?
                }
                CoverageCommand::Recover {
                    plan,
                    reason,
                    authorization_note,
                    calls_limit,
                    revise_runtime,
                } => {
                    serde_json::json!({"recovery":coverage::recover(&app,&plan,&reason,&authorization_note,calls_limit,revise_runtime,now)?})
                }
                CoverageCommand::Amend { plan, reason } => {
                    serde_json::json!({"amendment":coverage::amend(&app,&plan,&reason,now)?})
                }
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Command::Ocr { command } => {
            use ai_daily_report::{cli::OcrCommand, maintenance::ocr};
            let result = match command {
                OcrCommand::Validate { manifest } => {
                    let documents = ocr::load_manifest(&config, &manifest, &Default::default())?;
                    serde_json::json!({"status":"validated","documents":documents.len(),"text_unverified":true})
                }
                OcrCommand::Run {
                    engine,
                    engine_arg,
                    file,
                    pages,
                    output_dir,
                } => {
                    let limits = ocr::OcrLimits {
                        enabled: true,
                        ..Default::default()
                    };
                    let mut worker = ocr::OcrEngine::new(
                        engine,
                        engine_arg.into_iter().map(Into::into).collect(),
                        limits,
                        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    )?;
                    let document = worker.recognize(&config, &file, &pages)?;
                    serde_json::json!({"manifest":ocr::export_document(&output_dir,&document)?,"text_unverified":true})
                }
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Command::Backfill(args) => {
            let mut config = config;
            if let Some(tokens) = args.output_tokens {
                if tokens == 0
                    || config
                        .llm
                        .verified_max_output_tokens
                        .is_none_or(|limit| tokens > limit)
                {
                    return Err(ReportError::new(
                        "MODEL_CAPABILITY_INVALID",
                        "输出容量超过已验证模型上限",
                    ));
                }
                config.llm.max_tokens = tokens;
                config.llm.facts_max_tokens = tokens;
            }
            let mut app = App::open(config, now)?;
            app.model_workers = args.workers;
            if let Some(path) = &args.ocr_manifest {
                app.ocr_manifest = ai_daily_report::maintenance::ocr::load_manifest(
                    &app.config,
                    path,
                    &Default::default(),
                )?;
            }
            let result = ai_daily_report::maintenance::backfill::run(&app, &args, now)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            if result["complete"] == false && !args.only_no_model {
                return Err(ReportError::new(
                    "BACKFILL_INCOMPLETE",
                    "补跑尚有未完成日期，请查看保存的状态与错误码",
                ));
            }
            Ok(())
        }
        Command::Cleanup { execute } => {
            let app = App::open(config, now)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&ai_daily_report::maintenance::retention::cleanup(
                    &app, now, execute
                )?)?
            );
            Ok(())
        }
        Command::Backup { output } => {
            let app = App::open(config, now)?;
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
        Command::Task {
            command: ai_daily_report::cli::TaskCommand::Plan { sid },
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&ai_daily_report::operations::task_plan(
                    &config, &sid, now
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
