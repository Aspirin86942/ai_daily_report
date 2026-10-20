use ai_daily_report::{
    cli::Cli,
    config::AppConfig,
    run::{prepare_archive_input, App},
    snapshots::canonical_json,
    types::new_run_id,
};
use chrono::Utc;
use clap::Parser;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::{fs, path::PathBuf, time::Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config_dir: PathBuf,
    #[arg(long)]
    target_date: String,
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long)]
    worker_dir: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let started = Instant::now();
    let now = Utc::now();
    let mut config = AppConfig::load(&std::env::current_dir()?, Some(&args.config_dir))?;
    if let Some(directory) = args.worker_dir {
        let directory = fs::canonicalize(directory)?;
        config.office_worker_path = directory.join("ai-daily-office-parser.exe");
        config.pdf_worker_path = directory.join("ai-daily-pdf-parser.exe");
    }
    if args.output_dir.exists() {
        return Err("output directory must not already exist".into());
    }
    fs::create_dir_all(args.output_dir.join("data/db"))?;
    let output = fs::canonicalize(args.output_dir)?;
    let source = Connection::open_with_flags(
        config.paths.db_dir.join("reports.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    source.execute(
        "VACUUM INTO ?1",
        [output.join("data/db/reports.sqlite3").to_string_lossy()],
    )?;
    drop(source);
    let mut isolated = config.clone();
    isolated.paths.data_dir = output.join("data");
    isolated.paths.db_dir = output.join("data/db");
    let mut app = App::open(isolated, now)?;
    // Preparation reads the real archive; all report and snapshot writes stay isolated.
    app.config = config;
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
    let request =
        command.report_request(&app.config, now, new_run_id(), app.fingerprint(false)?)?;
    let calls_before: i64 =
        app.store
            .connection()?
            .query_row("SELECT COUNT(*) FROM report_model_calls", [], |r| r.get(0))?;
    let input = prepare_archive_input(&app, &request)?;
    if input.sources.is_empty()
        && input
            .gaps
            .iter()
            .any(|gap| gap.code == "ARCHIVE_PARSE_PENDING")
    {
        return Err("no prepared sources while archive versions remain pending".into());
    }
    if input.sources.iter().any(|source| source.model_truncated) {
        return Err("preflight found incomplete model input coverage".into());
    }
    for batch in &input.plan.batches {
        batch.validate(&app.config.llm)?;
    }
    let bytes = canonical_json(&serde_json::to_value(&input)?)?;
    eprintln!(
        "Prepared {} sources in {} batches; frozen input: {} bytes",
        input.sources.len(),
        input.plan.batches.len(),
        bytes.len()
    );
    let reference = app
        .snapshots
        .put_bytes(&input.context.run_id, "input", &bytes)?;
    if app.snapshots.read_bytes(&reference)? != bytes {
        return Err("snapshot round trip changed input bytes".into());
    }
    let calls_after: i64 =
        app.store
            .connection()?
            .query_row("SELECT COUNT(*) FROM report_model_calls", [], |r| r.get(0))?;
    if calls_after != calls_before {
        return Err("preflight unexpectedly changed the model call ledger".into());
    }
    let result = json!({
        "status": "prepared",
        "target_date": args.target_date,
        "model_calls": 0,
        "sources": input.sources.len(),
        "changes": input.plan.target["archive_changes"].as_array().map(Vec::len),
        "batches": input.plan.batches.len(),
        "max_batches": app.config.llm.max_batches,
        "excerpt_chars": input.sources.iter().map(|source| source.excerpt_chars).sum::<usize>(),
        "model_chars": input.sources.iter().map(|source| source.model_chars).sum::<usize>(),
        "max_request_bytes": input.plan.batches.iter().map(|batch| batch.bytes().map(|bytes| bytes.len())).collect::<Result<Vec<_>, _>>()?.into_iter().max(),
        "request_byte_limit": app.config.llm.max_request_utf8_bytes,
        "snapshot_bytes": bytes.len(),
        "snapshot_sha256": reference.sha256,
        "snapshot_path": reference.path,
        "quality": input.quality,
        "duration_seconds": started.elapsed().as_secs_f64()
    });
    fs::write(output.join("verification.json"), canonical_json(&result)?)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
