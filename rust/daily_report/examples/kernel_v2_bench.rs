//! Isolated M4 evidence driver for the v2 scanner and its packaged PDF worker.
use ai_daily_report::scan::{
    cache::{ScanCache, CACHE_FILENAME},
    config::normalize_scanner_settings,
    ContextResult, ScanRequest, Scanner, ScannerConfig,
};
use chrono::{DateTime, NaiveDate, Utc};
use daily_parsers::{
    discovery::{discover_files_with_diagnostics, DiscoveryRequest},
    pdf_wire::PDF_PROTOCOL_VERSION,
    settings::{AdapterPaths, ReportMode, ScannerSettings},
    PARSER_CONTENT_IDENTITY,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const ENGINE_BUILD: &str = env!("AI_DAILY_REPORT_BUILD");
type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Deserialize)]
struct Request {
    schema: u32,
    case: String,
    manifest_sha256: String,
    engine: String,
    package: PathBuf,
    output: PathBuf,
    workers: Option<u64>,
    scanner_settings: Option<ScannerSettings>,
    small_corpus: Option<PathBuf>,
    mixed_corpus: Option<PathBuf>,
    discovery_corpus: Option<PathBuf>,
    start_date: Option<String>,
    end_date: Option<String>,
    target_date: Option<String>,
    window_start_utc: Option<String>,
    window_end_utc: Option<String>,
    #[serde(default)]
    allowed_extensions: Vec<String>,
    #[serde(default)]
    excluded_dirs: Vec<PathBuf>,
    #[serde(default)]
    follow_symlinks: bool,
    #[serde(default)]
    ocr_enabled: bool,
}

fn required<'a, T>(value: &'a Option<T>, field: &str) -> Result<&'a T> {
    value
        .as_ref()
        .ok_or_else(|| format!("missing {field}").into())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, value)?;
    writer.flush()?;
    Ok(())
}

fn scan_request(req: &Request) -> Result<ScanRequest> {
    Ok(ScanRequest {
        report_mode: ReportMode::Daily,
        start_date: required(&req.start_date, "start_date")?.clone(),
        end_date: required(&req.end_date, "end_date")?.clone(),
        window_start_utc: req.window_start_utc.clone(),
        window_end_utc: req.window_end_utc.clone(),
        compression_profile: None,
    })
}

fn scanner(
    root: &Path,
    cache: &Path,
    settings: ScannerSettings,
    package: &Path,
) -> Result<Scanner> {
    Ok(Scanner::open(ScannerConfig {
        work_dir: root.canonicalize()?.to_string_lossy().into_owned(),
        scan_db_path: cache.join(CACHE_FILENAME).to_string_lossy().into_owned(),
        scanner_settings: settings,
        adapters: AdapterPaths {
            office_worker_path: package
                .join("ai-daily-report.exe")
                .to_string_lossy()
                .into_owned(),
            pdf_worker_path: package
                .join("ai-daily-pdf-worker.exe")
                .to_string_lossy()
                .into_owned(),
        },
    })?)
}

#[derive(Serialize)]
struct ScanSample {
    elapsed_ms: f64,
    batch: u32,
    temperature: &'static str,
    exit_code: i32,
    result: ContextResult,
}

fn take_scan(
    scanner: &Scanner,
    request: &ScanRequest,
    batch: u32,
    warm: bool,
    start: Instant,
) -> Result<ScanSample> {
    let sequence = batch * 2 + u32::from(warm);
    let request_id = format!("71616161-7161-4161-8161-{sequence:012}");
    let observed = "2026-11-22T02:00:00Z".parse::<DateTime<Utc>>()?;
    let outcome = scanner.build_context_with_request_id_at(request, request_id, observed)?;
    Ok(ScanSample {
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        batch,
        temperature: if warm { "warm" } else { "cold" },
        exit_code: outcome.exit_code,
        result: outcome.value,
    })
}

fn scans(req: &Request) -> Result<Value> {
    let small = req.case == "small";
    let wal_warm = req.case == "wal_warm";
    let request = scan_request(req)?;
    let mut settings = required(&req.scanner_settings, "scanner_settings")?.clone();
    settings.max_workers = req.workers;
    if small {
        // The archived batch fixture was admitted before these files were staged.
        settings.ignored_patterns = Some(Vec::new());
        settings.excluded_dirs = Some(Vec::new());
    }
    let effective = normalize_scanner_settings(&settings, ReportMode::Daily)?;
    if effective.parse.pdf.image_ocr.is_some() || req.ocr_enabled {
        return Err("this benchmark requires OCR disabled".into());
    }
    let mut samples = Vec::new();
    let mut initialization_ms = 0.0;
    if small {
        let corpus = required(&req.small_corpus, "small_corpus")?;
        let manifest: Value = serde_json::from_slice(&fs::read(corpus.join("manifest.json"))?)?;
        if manifest["file_count"] != 13 || manifest["batch_size"] != 4 {
            return Err("unexpected four-batch fixture".into());
        }
        for batch in 0..4 {
            let name = format!("batch-{batch:04}");
            let cache = req.output.join(&name).join("daily_cache");
            let start = Instant::now();
            let scanner = scanner(&corpus.join(&name), &cache, settings.clone(), &req.package)?;
            initialization_ms += start.elapsed().as_secs_f64() * 1000.0;
            samples.push(take_scan(&scanner, &request, batch, false, Instant::now())?);
            samples.push(take_scan(&scanner, &request, batch, true, Instant::now())?);
        }
    } else {
        let corpus = required(&req.mixed_corpus, "mixed_corpus")?;
        let cache = req.output.join("daily_cache");
        if req.case == "mixed" && cache.join(CACHE_FILENAME).exists() {
            return Err("mixed cold cache already exists".into());
        }
        let start = Instant::now();
        let scanner = scanner(corpus, &cache, settings, &req.package)?;
        samples.push(take_scan(&scanner, &request, 0, wal_warm, start)?);
    }
    let cold_batches: Vec<_> = samples
        .iter()
        .filter(|sample| sample.temperature == "cold")
        .map(|sample| sample.elapsed_ms)
        .collect();
    let warm_batches: Vec<_> = samples
        .iter()
        .filter(|sample| sample.temperature == "warm")
        .map(|sample| sample.elapsed_ms)
        .collect();
    let cold_ms: f64 = cold_batches.iter().sum();
    let raw = req.output.join(format!("{}.raw.json", req.case));
    write_json(&raw, &samples)?;
    Ok(json!({
        "initialization_ms": initialization_ms,
        "effective_max_workers": effective.execution.max_workers,
        "cold_batches_ms": cold_batches,
        "warm_batches_ms": warm_batches,
        "cold_ms": cold_ms,
        "raw_output": raw.file_name().unwrap().to_string_lossy(),
    }))
}

fn discovery(req: &Request) -> Result<Value> {
    let root = required(&req.discovery_corpus, "discovery_corpus")?.canonicalize()?;
    let day = required(&req.target_date, "target_date")?.parse::<NaiveDate>()?;
    let absolute_time = |value: &Option<String>| -> Result<Option<DateTime<Utc>>> {
        Ok(value.as_deref().map(str::parse).transpose()?)
    };
    let discovery = DiscoveryRequest {
        work_dir: root.clone(),
        start_date: day,
        end_date: day,
        window_start_utc: absolute_time(&req.window_start_utc)?,
        window_end_utc: absolute_time(&req.window_end_utc)?,
        follow_symlinks: req.follow_symlinks,
        allowed_extensions: req.allowed_extensions.clone(),
        ignored_patterns: Vec::new(),
        excluded_dirs: req.excluded_dirs.clone(),
    };
    let started = Instant::now();
    let output = discover_files_with_diagnostics(&discovery)?;
    let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    let paths = output
        .files
        .iter()
        .map(|file| -> Result<String> {
            Ok(Path::new(&file.path)
                .canonicalize()?
                .strip_prefix(&root)?
                .to_string_lossy()
                .replace('\\', "/"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"ms": duration_ms, "paths": paths, "issues": output.issues}))
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 || args[0] != "--request" {
        return Err("usage: kernel_v2_bench --request <request.json>".into());
    }
    let req: Request = serde_json::from_slice(&fs::read(&args[1])?)?;
    if req.schema != 1 || req.engine != ENGINE_BUILD || req.ocr_enabled {
        return Err("request schema, package engine or OCR config mismatched".into());
    }
    if req.output.canonicalize()? != std::env::current_dir()?.canonicalize()? {
        return Err("output must be the isolated working directory".into());
    }
    let mut result = match req.case.as_str() {
        "small" | "mixed" | "wal_cold" | "wal_warm" => scans(&req)?,
        "discovery" => discovery(&req)?,
        "wal_init" => {
            ScanCache::open(
                &req.output.join("daily_cache"),
                Instant::now() + Duration::from_secs(30),
            )?
            .initialize()?;
            json!({"cache_filename": CACHE_FILENAME})
        }
        _ => return Err("unknown test case".into()),
    };
    let object = result.as_object_mut().expect("receipt is an object");
    object.insert("manifest_sha256".into(), json!(req.manifest_sha256));
    object.insert("engine".into(), json!(ENGINE_BUILD));
    object.insert(
        "parser_content_identity".into(),
        json!(PARSER_CONTENT_IDENTITY),
    );
    object.insert("worker_protocol".into(), json!(PDF_PROTOCOL_VERSION));
    object.insert("ocr_enabled".into(), json!(false));
    object.insert("remote_model_requests".into(), json!(0));
    println!("{result}");
    Ok(())
}
