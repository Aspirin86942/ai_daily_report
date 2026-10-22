//! Read-only replay of personal input selection and model request construction.
//! No collection, cache maintenance, report writes, or network requests.
use ai_daily_report::{
    archive::selection,
    cli::Cli,
    config::{AppConfig, ReportProfile},
    model::personal,
    reports::personal::PersonalReport,
    types::new_run_id,
};
use chrono::Utc;
use clap::Parser;
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config_dir: PathBuf,
    #[arg(long)]
    target_date: String,
    #[arg(long)]
    worker_dir: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let started = Instant::now();
    let mut config = AppConfig::load(&std::env::current_dir()?, Some(&args.config_dir))?;
    if let Some(directory) = args.worker_dir {
        let directory = std::fs::canonicalize(directory)?;
        config.office_worker_path = directory.join("ai-daily-office-parser.exe");
        config.pdf_worker_path = directory.join("ai-daily-pdf-parser.exe");
    }
    if config.report.profile != ReportProfile::Personal {
        return Err("personal profile required".into());
    }
    let command = Cli::try_parse_from([
        "preflight",
        "daily",
        "--source",
        "archive",
        "--target-date",
        &args.target_date,
        "--dry-run",
    ])?
    .command
    .ok_or("daily command missing")?;
    let request = command.report_request(&config, Utc::now(), new_run_id(), String::new())?;
    let mut selected = selection::prepare(&config, &request, started + Duration::from_secs(120))?;
    if selected.materials.is_empty()
        && selected
            .summary
            .omissions
            .get("read_failed")
            .is_some_and(|count| *count > 0)
    {
        return Err("observed changes have no readable material for this parser profile".into());
    }
    let initial_materials = selected.materials.len();
    let plan = if selected.materials.is_empty() {
        None
    } else {
        Some(personal::prepare(
            &config.llm,
            &request.context,
            &mut selected,
        )?)
    };
    selected.validate()?;
    PersonalReport::from_selection(&request.context, "work", &selected).validate()?;
    let references: Vec<_> = selected
        .materials
        .iter()
        .flat_map(|m| &m.evidence)
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "prepared", "target_date": args.target_date,
            "read_only": true, "model_calls": 0, "initial_materials": initial_materials,
            "materials": selected.materials.len(), "selected_files": selected.summary.selected_files,
            "selected_chars": selected.summary.selected_chars,
            "references": references.len(),
            "references_without_original_ranges": references.iter().filter(|r| r.ranges.is_empty()).count(),
            "request_bytes": plan.as_ref().map(|p| p.bytes().map(|b| b.len())).transpose()?,
            "omissions": selected.summary.omissions,
            "gap_codes": selected.gaps.iter().map(|g| &g.code).collect::<Vec<_>>(),
            "duration_ms": started.elapsed().as_millis(),
        }))?
    );
    Ok(())
}
