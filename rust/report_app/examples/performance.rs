//! Fixed-input benchmark with an in-process model double; never sends network requests.
#[path = "../tests/support/model_mock.rs"]
mod model_mock;
use ai_daily_report::{
    config::AppConfig,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    run::{prepare_daily_input, prepare_frozen, resume_frozen, App},
    types::{self, Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use ai_daily_scanner_contract::{AdapterPaths, ParseStatus, ScannerSettings};
use ai_daily_scanner_core::{ScanRequest, Scanner, ScannerConfig};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, FileTimes},
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};
struct FixedModel;
impl ModelTransport for FixedModel {
    fn send(&self, bytes: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        let request = serde_json::from_slice(bytes).unwrap();
        Ok(HttpResponse{status:200,retry_after:None,body:json!({"choices":[{"finish_reason":"stop","message":{"content":model_mock::reply(&request).to_string()}}],"usage":null}).to_string().into_bytes()})
    }
}
fn set_time(path: &Path) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1791680400)))
        .unwrap();
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: performance WORKER_DIRECTORY OUTPUT_JSON".into());
    }
    let workers = fs::canonicalize(&args[0])?;
    let output = PathBuf::from(&args[1]);
    if output.exists() {
        return Err("output already exists".into());
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut samples = Vec::new();
    let now = DateTime::parse_from_rfc3339("2026-10-12T01:00:00Z")?.with_timezone(&Utc);
    let date = types::parse_date("2026-10-11")?;
    let (start, end) = types::utc_window(date)?;
    for repeat in 0..5 {
        for kind in ["markdown", "office", "pdf", "fixed_report"] {
            let dir = tempfile::tempdir()?;
            let work = dir.path().join("work");
            fs::create_dir(&work)?;
            if matches!(kind, "markdown" | "fixed_report") {
                for index in 0..if kind == "markdown" { 32 } else { 1 } {
                    let path = work.join(if kind == "markdown" {
                        format!("synthetic-{index:02}.md")
                    } else {
                        "work.md".into()
                    });
                    let text = if kind == "markdown" {
                        format!(
                            "采购方案 {index}\n{}",
                            "金额五万元以内由部门负责人审批；紧急采购另行登记。\n".repeat(12)
                        )
                    } else {
                        "方案记载计划于下周验收。".into()
                    };
                    fs::write(&path, text)?;
                    set_time(&path);
                }
            } else {
                let fixture = if kind == "office" {
                    "worker_documents/modern_sample.docx"
                } else {
                    "pdf_classifier/text_plain_01.pdf"
                };
                let path = work.join(if kind == "office" {
                    "sample.docx"
                } else {
                    "sample.pdf"
                });
                fs::copy(repo.join("tests/fixtures").join(fixture), &path)?;
                set_time(&path);
            }
            let settings: ScannerSettings = serde_json::from_value(
                json!({"allowed_extensions":[if kind=="office"{".docx"}else if kind=="pdf"{".pdf"}else{".md"}],"total_deadline_ms":30000,"total_max_chars":50000}),
            )?;
            let scanner = Scanner::open(ScannerConfig {
                work_dir: work.to_string_lossy().into(),
                scan_db_path: dir
                    .path()
                    .join("scan_index_rust_v3.sqlite3")
                    .to_string_lossy()
                    .into(),
                scanner_settings: settings,
                adapters: AdapterPaths {
                    office_worker_path: workers
                        .join("ai-daily-office-parser.exe")
                        .to_string_lossy()
                        .into(),
                    pdf_worker_path: workers
                        .join("ai-daily-pdf-parser.exe")
                        .to_string_lossy()
                        .into(),
                },
            })?;
            let request = ScanRequest {
                report_mode: ai_daily_scanner_contract::ReportMode::Daily,
                start_date: date.to_string(),
                end_date: date.to_string(),
                window_start_utc: Some(start.to_rfc3339()),
                window_end_utc: Some(end.to_rfc3339()),
                compression_profile: None,
            };
            if kind == "fixed_report" {
                let scan = scanner.build_context(&request)?.value;
                let config_dir = dir.path().join("config");
                fs::create_dir(&config_dir)?;
                fs::write(config_dir.join("settings.yaml"),json!({"paths":{"work_dir":work,"data_dir":dir.path().join("data"),"db_dir":dir.path().join("db"),"reports_dir":dir.path().join("reports")},"llm":{"max_request_utf8_bytes":64000,"context_window_tokens":100000,"verified_max_output_tokens":8192,"max_tokens":4096},"recovery":{"enforce_model_budgets":false}}).to_string())?;
                let mut app = App::open(
                    AppConfig::load_with_env(dir.path(), Some(&config_dir), &BTreeMap::new())?,
                    now,
                )?;
                app.transport = Box::new(FixedModel);
                let request = ReportRequest {
                    period: Period {
                        mode: ReportMode::Daily,
                        source: ReportSource::Scan,
                        start: date,
                        end: date,
                        label: date.to_string(),
                    },
                    context: RunContext::new(
                        types::new_run_id(),
                        now,
                        date,
                        app.fingerprint(false)?,
                    )?,
                    input: String::new(),
                    save: true,
                    dry_run: false,
                    scheduled: false,
                    regeneration_reason: None,
                };
                let started = Instant::now();
                let input = prepare_daily_input(&app, &request, &scan)?;
                let id = prepare_frozen(&app, &input, true, "fixed-response performance")?;
                let result = resume_frozen(&app, &id, now)?;
                samples.push(json!({"repeat":repeat,"kind":kind,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"model_calls":result.model_calls}));
            } else {
                let mut content = None;
                for temperature in ["cold", "warm"] {
                    let started = Instant::now();
                    let scan = scanner.build_context(&request)?.value;
                    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                    let value = serde_json::to_value(&scan)?;
                    assert_eq!(value["envelope"]["status"], "ok");
                    assert!(scan
                        .sources
                        .iter()
                        .all(|s| s.parse_status == ParseStatus::Success));
                    let texts: Vec<_> = scan.sources.iter().map(|s| s.excerpt.clone()).collect();
                    if let Some(previous) = content.as_ref() {
                        assert_eq!(&texts, previous);
                    }
                    content = Some(texts);
                    samples.push(json!({"repeat":repeat,"kind":kind,"temperature":temperature,"elapsed_ms":elapsed,"sources":scan.sources.len(),"excerpt_chars":scan.sources.iter().map(|s|s.excerpt.chars().count()).sum::<usize>(),"peak_worker_rss_bytes":value["evidence"]["execution_metrics"]["peak_worker_rss_bytes"]}));
                }
            }
        }
    }
    let result: Value = json!({"scope":"five runs; same fixtures; fixed local model; fixed report scan prepared outside timed block","samples":samples});
    fs::write(output, serde_json::to_vec_pretty(&result)?)?;
    println!("{{\"samples\":{}}}", samples.len());
    Ok(())
}
