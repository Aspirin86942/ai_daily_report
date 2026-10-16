//! Offline, exhaustive corpus benchmark. Four-file batches keep the production
//! context budget from silently excluding files. Source documents are copied once;
//! preparation is timed separately and the same frozen corpus can be replayed.
use ai_daily_discovery::{discover_files_with_diagnostics, DiscoveryRequest};
use ai_daily_scanner_contract::{AdapterPaths, ReportMode, ScannerSettings};
use ai_daily_scanner_core::store::sha256_hex;
use ai_daily_scanner_core::{
    config::normalize_scanner_settings, ScanRequest, Scanner, ScannerConfig,
};
use chrono::NaiveDate;
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: corpus_performance WORKERS SETTINGS_JSON CORPUS_DIR OUTPUT_DIR".into());
    }
    let workers = fs::canonicalize(&args[0])?;
    let config: Value = serde_json::from_slice(&fs::read(&args[1])?)?;
    let mut settings: ScannerSettings = serde_json::from_value(config["scanner"].clone())?;
    let profile = normalize_scanner_settings(&settings, ReportMode::Daily)?;
    let corpus = PathBuf::from(&args[2]);
    let output = PathBuf::from(&args[3]);
    if output.exists() {
        return Err("output directory already exists".into());
    }
    fs::create_dir_all(&output)?;
    let start_date = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let end_date = NaiveDate::from_ymd_opt(2099, 12, 31).unwrap();
    let manifest_path = corpus.join("manifest.json");
    let manifest: Value = if manifest_path.exists() {
        serde_json::from_slice(&fs::read(&manifest_path)?)?
    } else {
        if corpus.exists() {
            return Err("corpus directory exists without a completed manifest".into());
        }
        fs::create_dir_all(&corpus)?;
        let started = Instant::now();
        let discovery = discover_files_with_diagnostics(&DiscoveryRequest {
            work_dir: PathBuf::from(config["work_dir"].as_str().ok_or("work_dir missing")?),
            start_date,
            end_date,
            window_start_utc: None,
            window_end_utc: None,
            follow_symlinks: profile.discovery.follow_symlinks,
            allowed_extensions: profile.discovery.allowed_extensions.clone(),
            ignored_patterns: profile.discovery.ignored_patterns.clone(),
            excluded_dirs: profile
                .discovery
                .excluded_dirs
                .iter()
                .map(PathBuf::from)
                .collect(),
        })?;
        let discovery_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut files = Vec::new();
        for (index, source) in discovery.files.iter().enumerate() {
            let batch = index / 4;
            let name = format!("file-{index:05}{}", source.extension);
            let directory = corpus.join(format!("batch-{batch:04}"));
            fs::create_dir_all(&directory)?;
            let original = Path::new(&source.path);
            let before = fs::metadata(original)?;
            fs::copy(original, directory.join(&name))?;
            let after = fs::metadata(original)?;
            if before.len() != source.size_bytes
                || before.len() != after.len()
                || before.modified()? != after.modified()?
            {
                return Err("source changed while preparing corpus".into());
            }
            files.push(json!({"batch":batch,"name":name,"original":source}));
        }
        let manifest = json!({
            "work_dir":config["work_dir"],"scanner":config["scanner"],
            "batch_size":4,"file_count":files.len(),"files":files,
            "discovery_issues":discovery.issues,"discovery_ms":discovery_ms,
            "preparation_ms":started.elapsed().as_secs_f64()*1000.0
        });
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        manifest
    };
    if manifest["scanner"] != config["scanner"] || manifest["work_dir"] != config["work_dir"] {
        return Err("frozen corpus configuration mismatch".into());
    }
    let files = manifest["files"]
        .as_array()
        .ok_or("manifest files missing")?;
    if files.is_empty() {
        return Err("corpus has no eligible files".into());
    }
    let batches = files.len().div_ceil(4);
    // Discovery filters were already applied to the original paths. The staged
    // names are neutral, and each batch retains the production parsing limits.
    settings.ignored_patterns = Some(Vec::new());
    settings.excluded_dirs = Some(Vec::new());
    let mut log = BufWriter::new(File::create(output.join("samples.jsonl"))?);
    let mut totals = [0.0_f64; 2];
    let mut initialization_ms = 0.0;
    let suite_started = Instant::now();
    for batch in 0..batches {
        let database_dir = output.join(format!("batch-{batch:04}"));
        fs::create_dir(&database_dir)?;
        let started = Instant::now();
        let scanner = Scanner::open(ScannerConfig {
            work_dir: fs::canonicalize(corpus.join(format!("batch-{batch:04}")))?
                .to_string_lossy()
                .into_owned(),
            scan_db_path: database_dir
                .join("scan_index_rust_v3.sqlite3")
                .to_string_lossy()
                .into_owned(),
            scanner_settings: settings.clone(),
            adapters: AdapterPaths {
                office_worker_path: workers
                    .join("ai-daily-office-parser.exe")
                    .to_string_lossy()
                    .into_owned(),
                pdf_worker_path: workers
                    .join("ai-daily-pdf-parser.exe")
                    .to_string_lossy()
                    .into_owned(),
            },
        })?;
        initialization_ms += started.elapsed().as_secs_f64() * 1000.0;
        for (temperature_index, temperature) in ["cold", "warm"].iter().enumerate() {
            let started = Instant::now();
            let result = scanner.build_context(&ScanRequest {
                report_mode: ReportMode::Daily,
                start_date: start_date.to_string(),
                end_date: end_date.to_string(),
                window_start_utc: None,
                window_end_utc: None,
                compression_profile: None,
            })?;
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            totals[temperature_index] += elapsed_ms;
            let result = serde_json::to_value(result.value)?;
            let evidence = &result["evidence"];
            let audit_files = evidence["files"]
                .as_array()
                .ok_or("file evidence missing")?;
            if audit_files.len() != files.len().saturating_sub(batch * 4).min(4) {
                return Err(format!("batch {batch}: incomplete file accounting").into());
            }
            let sample = json!({
                "batch":batch,"temperature":temperature,"elapsed_ms":elapsed_ms,
                "status":result["envelope"]["status"],"summary":evidence["summary"],
                "stage_metrics":evidence["stage_metrics"],"extension_metrics":evidence["extension_metrics"],
                "execution_metrics":evidence["execution_metrics"],"files":audit_files,
                "decisions":evidence["decisions"],"reuse_kind":evidence["reuse_kind"],
                "context_sha256":sha256_hex(result["envelope"]["file_context"]
                    .as_str().ok_or("file context missing")?.as_bytes())
            });
            serde_json::to_writer(&mut log, &sample)?;
            writeln!(log)?;
            log.flush()?;
        }
        if (batch + 1) % 25 == 0 || batch + 1 == batches {
            println!(
                "batches={}/{} files={} cold_ms={:.1} warm_ms={:.1}",
                batch + 1,
                batches,
                ((batch + 1) * 4).min(files.len()),
                totals[0],
                totals[1]
            );
        }
    }
    let summary = json!({
        "files":files.len(),"batches":batches,"cold_ms":totals[0],"warm_ms":totals[1],
        "initialization_ms":initialization_ms,"suite_wall_ms":suite_started.elapsed().as_secs_f64()*1000.0,
        "manifest_sha256":sha256_hex(&fs::read(&manifest_path)?),
        "scope":"all eligible source documents in fixed four-file batches; cold/warm include scanning, parsing, cache, context and audit; copying and database initialization measured separately; no model calls"
    });
    fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}
