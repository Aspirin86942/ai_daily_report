//! Isolated M4 evidence driver for the v2 scanner and its packaged PDF worker.
#[path = "support/scan_projection.rs"]
mod scan_projection;

use ai_daily_report::scan::{
    cache::{ScanCache, CACHE_FILENAME},
    config::normalize_scanner_settings,
    ScanBenchFile, ScanRequest, ScanResult, Scanner, ScannerConfig,
};
use chrono::{DateTime, NaiveDate, Utc};
use daily_parsers::{
    discovery::{discover_files_with_diagnostics, DiscoveryRequest},
    pdf_wire::PDF_PROTOCOL_VERSION,
    settings::{EngineStatus, ReportMode, ScannerSettings},
    PARSER_CONTENT_IDENTITY,
};
use serde::{Deserialize, Serialize, Serializer};
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
type BenchmarkResult = (ScanResult, Vec<ScanBenchFile>);

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
    let start_date = scan_date(required(&req.start_date, "start_date")?, "start_date")?;
    let end_date = scan_date(required(&req.end_date, "end_date")?, "end_date")?;
    let utc_window = match (
        req.window_start_utc.as_deref(),
        req.window_end_utc.as_deref(),
    ) {
        (None, None) => None,
        (Some(start), Some(end)) => Some((utc_instant(start)?, utc_instant(end)?)),
        _ => return Err("UTC window boundaries must be supplied together".into()),
    };
    Ok(ScanRequest {
        start_date,
        end_date,
        utc_window,
    })
}

fn scan_date(value: &str, field: &str) -> Result<NaiveDate> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, byte)| [4, 7].contains(&i) || byte.is_ascii_digit())
    {
        return Err(format!("{field} must use YYYY-MM-DD").into());
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("{field} must be a valid calendar date").into())
}

fn utc_instant(value: &str) -> Result<DateTime<Utc>> {
    const INVALID_TIMESTAMP: &str = "scan window must use RFC 3339 UTC timestamps";
    let body = value
        .strip_suffix('Z')
        .or_else(|| value.strip_suffix("+00:00"))
        .ok_or(INVALID_TIMESTAMP)?;
    let time = DateTime::parse_from_rfc3339(value)?;
    let bytes = body.as_bytes();
    let fraction_ok = bytes.len() == 19 || ((21..=26).contains(&bytes.len()) && bytes[19] == b'.');
    if !fraction_ok
        || bytes.get(10) != Some(&b'T')
        || body.starts_with("0000")
        || time.timestamp_subsec_nanos() >= 1_000_000_000
    {
        return Err(INVALID_TIMESTAMP.into());
    }
    Ok(time.to_utc())
}

fn scanner(
    root: &Path,
    cache: &Path,
    settings: ScannerSettings,
    package: &Path,
) -> Result<Scanner> {
    Ok(Scanner::open(ScannerConfig {
        work_dir: root.canonicalize()?,
        scan_db_path: cache.join(CACHE_FILENAME),
        scanner_settings: settings,
        pdf_worker_path: package.join("ai-daily-pdf-worker.exe"),
    })?)
}

#[derive(Serialize)]
struct ScanSample {
    elapsed_ms: f64,
    batch: u32,
    temperature: &'static str,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<EngineStatus>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_projection"
    )]
    result: Option<BenchmarkResult>,
}

fn serialize_projection<S: Serializer>(
    result: &Option<BenchmarkResult>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match result {
        Some((scan, observed)) => {
            scan_projection::project(scan, observed, ENGINE_BUILD).serialize(serializer)
        }
        None => serializer.serialize_none(),
    }
}

#[derive(Clone, Copy)]
enum ScanMode {
    Evidence(DateTime<Utc>),
    TimingOnly,
}

fn take_scan(
    scanner: &Scanner,
    request: &ScanRequest,
    batch: u32,
    warm: bool,
    start: Instant,
    mode: ScanMode,
) -> Result<ScanSample> {
    let (elapsed, status, result) = match mode {
        ScanMode::Evidence(observed) => {
            let result = scanner.build_context_for_benchmark(request, observed)?;
            (start.elapsed(), result.0.status, Some(result))
        }
        ScanMode::TimingOnly => {
            let start = Instant::now();
            let scan = scanner.build_context(request)?;
            (start.elapsed(), scan.status, None)
        }
    };
    Ok(ScanSample {
        elapsed_ms: elapsed.as_secs_f64() * 1000.0,
        batch,
        temperature: if warm { "warm" } else { "cold" },
        exit_code: i32::from(status == EngineStatus::Error) * 2,
        status: result.is_none().then_some(status),
        result,
    })
}

fn scans(req: &Request, mode: ScanMode) -> Result<Value> {
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
            samples.push(take_scan(
                &scanner,
                &request,
                batch,
                false,
                Instant::now(),
                mode,
            )?);
            samples.push(take_scan(
                &scanner,
                &request,
                batch,
                true,
                Instant::now(),
                mode,
            )?);
        }
    } else {
        let corpus = required(&req.mixed_corpus, "mixed_corpus")?;
        let cache = req.output.join("daily_cache");
        if req.case == "mixed" && cache.join(CACHE_FILENAME).exists() {
            return Err("mixed cold cache already exists".into());
        }
        let start = Instant::now();
        let scanner = scanner(corpus, &cache, settings, &req.package)?;
        if matches!(mode, ScanMode::TimingOnly) {
            initialization_ms = start.elapsed().as_secs_f64() * 1000.0;
        }
        samples.push(take_scan(&scanner, &request, 0, wal_warm, start, mode)?);
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
    let mut receipt = json!({
        "initialization_ms": initialization_ms,
        "effective_max_workers": effective.execution.max_workers,
        "cold_batches_ms": cold_batches,
        "warm_batches_ms": warm_batches,
        "cold_ms": cold_ms,
    });
    match mode {
        ScanMode::Evidence(_) => {
            let raw = req.output.join(format!("{}.raw.json", req.case));
            write_json(&raw, &samples)?;
            receipt["raw_output"] = json!(raw.file_name().unwrap().to_string_lossy());
        }
        ScanMode::TimingOnly => {
            receipt["timing_only"] = json!(true);
            receipt["samples"] = json!(samples);
        }
    }
    Ok(receipt)
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
    let (request, mode) = match args.as_slice() {
        [flag, request] if flag == "--request" => {
            (request, ScanMode::Evidence("2026-11-22T02:00:00Z".parse()?))
        }
        [flag, request, timing] if flag == "--request" && timing == "--timing-only" => {
            (request, ScanMode::TimingOnly)
        }
        _ => return Err("usage: kernel_v2_bench --request <request.json> [--timing-only]".into()),
    };
    let req: Request = serde_json::from_slice(&fs::read(request)?)?;
    if req.schema != 1 || req.engine != ENGINE_BUILD || req.ocr_enabled {
        return Err("request schema, package engine or OCR config mismatched".into());
    }
    if req.output.canonicalize()? != std::env::current_dir()?.canonicalize()? {
        return Err("output must be the isolated working directory".into());
    }
    let mut result = match req.case.as_str() {
        "small" | "mixed" | "wal_cold" | "wal_warm" => scans(&req, mode)?,
        _ if matches!(mode, ScanMode::TimingOnly) => {
            return Err("--timing-only requires a scan case".into());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_request_decodes_the_retained_text_date_and_utc_window() {
        let base = json!({
            "schema": 1, "case": "mixed", "manifest_sha256": "", "engine": ENGINE_BUILD,
            "package": "unused", "output": "unused", "start_date": "2026-01-02", "end_date": "2026-01-03",
            "window_start_utc": "2026-01-02T00:00:00.123456Z", "window_end_utc": "2026-01-03T00:00:00+00:00",
        });
        let read = |value| scan_request(&serde_json::from_value(value).unwrap());
        let request = read(base.clone()).unwrap();
        assert_eq!(
            request.start_date,
            NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()
        );
        assert_eq!(
            request.end_date,
            NaiveDate::from_ymd_opt(2026, 1, 3).unwrap()
        );
        let (start, end) = request.utc_window.unwrap();
        assert_eq!(start.timestamp_subsec_micros(), 123_456);
        assert_eq!(
            end,
            "2026-01-03T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );

        for invalid in [
            "2026-01-02t00:00:00Z",
            "2026-01-02T00:00:00z",
            "2026-01-02T00:00:00.1234567Z",
            "2026-01-02T00:00:60Z",
            "0000-01-02T00:00:00Z",
            "2026-01-02T08:00:00+08:00",
            "2026-01-02T00:00:00",
        ] {
            let mut value = base.clone();
            value["window_start_utc"] = json!(invalid);
            assert!(read(value).is_err(), "{invalid}");
        }
        for invalid in ["2026-02-30", "2026-1-02"] {
            let mut value = base.clone();
            value["start_date"] = json!(invalid);
            assert!(read(value).is_err(), "{invalid}");
        }
        for field in ["window_start_utc", "window_end_utc"] {
            let mut value = base.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(read(value).is_err(), "{field}");
        }
        let mut no_window = base;
        no_window
            .as_object_mut()
            .unwrap()
            .remove("window_start_utc");
        no_window.as_object_mut().unwrap().remove("window_end_utc");
        assert!(read(no_window).unwrap().utc_window.is_none());
    }
}
