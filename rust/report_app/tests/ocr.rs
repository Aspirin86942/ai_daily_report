use ai_daily_report::{
    config::AppConfig,
    maintenance::ocr::{self, OcrLimits},
    run::{prepare_daily_input, App},
    snapshots::digest,
    types::{Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, File, FileTimes},
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant, UNIX_EPOCH},
};
struct Fixture {
    _dir: tempfile::TempDir,
    config: AppConfig,
    manifest: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        fs::write(dir.path().join("config/settings.yaml"),"paths:\n  work_dir: work\nllm:\n  max_request_utf8_bytes: 64000\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\n").unwrap();
        let mut config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        let workers = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug");
        config.office_worker_path =
            fs::canonicalize(workers.join("ai-daily-office-parser.exe")).unwrap();
        config.pdf_worker_path = fs::canonicalize(workers.join("ai-daily-pdf-parser.exe")).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_classifier/text_cjk_01.pdf");
        let pdf = config.paths.work_dir.join("scan.pdf");
        fs::copy(source, &pdf).unwrap();
        File::options()
            .write(true)
            .open(&pdf)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1791684000)))
            .unwrap();
        let text = "识别文字：拟调整审批金额，尚未核实。";
        fs::write(dir.path().join("page.txt"), text).unwrap();
        let manifest = dir.path().join("ocr.json");
        fs::write(&manifest,json!([{"source_id":format!("src_{}",digest(b"scan.pdf")),"content_sha256":digest(&fs::read(pdf).unwrap()),"pages_total":1,"pages":[{"page":1,"text":"page.txt","text_sha256":digest(text.as_bytes())}]}]).to_string()).unwrap();
        Self {
            _dir: dir,
            config,
            manifest,
        }
    }
}
#[test]
fn manifest_checks_original_pdf_text_hash_page_count_and_duplicate_identity() {
    let f = Fixture::new();
    let limits = OcrLimits::default();
    let docs = ocr::load_manifest(&f.config, &f.manifest, &limits).unwrap();
    assert_eq!(docs.len(), 1);
    let original = fs::read(&f.manifest).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    value[0]["pages_total"] = json!(2);
    fs::write(&f.manifest, value.to_string()).unwrap();
    assert_eq!(
        ocr::load_manifest(&f.config, &f.manifest, &limits)
            .unwrap_err()
            .code,
        "OCR_SOURCE_CHANGED"
    );
    fs::write(&f.manifest, &original).unwrap();
    fs::write(f.manifest.parent().unwrap().join("page.txt"), "changed").unwrap();
    assert_eq!(
        ocr::load_manifest(&f.config, &f.manifest, &limits)
            .unwrap_err()
            .code,
        "OCR_TEXT_CHANGED"
    );
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    value[0]["pages"][0]["page"] = json!(0);
    fs::write(
        f.manifest.parent().unwrap().join("page.txt"),
        "识别文字：拟调整审批金额，尚未核实。",
    )
    .unwrap();
    fs::write(&f.manifest, value.to_string()).unwrap();
    assert_eq!(
        ocr::load_manifest(&f.config, &f.manifest, &limits)
            .unwrap_err()
            .code,
        "OCR_PAGE_OUT_OF_RANGE"
    );
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let doc = value[0].clone();
    value.as_array_mut().unwrap().push(doc);
    fs::write(&f.manifest, value.to_string()).unwrap();
    assert_eq!(
        ocr::load_manifest(&f.config, &f.manifest, &limits)
            .unwrap_err()
            .code,
        "OCR_INVALID_MANIFEST"
    );
}
#[test]
fn overlay_keeps_unverified_status_original_pages_and_rejects_changed_source() {
    let f = Fixture::new();
    let docs = ocr::load_manifest(&f.config, &f.manifest, &Default::default()).unwrap();
    let now = chrono::Utc::now();
    let app = App::open(f.config, now).unwrap();
    let date = ai_daily_report::types::parse_date("2026-10-11").unwrap();
    let request = ReportRequest {
        context: RunContext::new(
            ai_daily_report::types::new_run_id(),
            now,
            date,
            app.fingerprint(false).unwrap(),
        )
        .unwrap(),
        period: Period {
            mode: ReportMode::Daily,
            source: ReportSource::Scan,
            start: date,
            end: date,
            label: date.to_string(),
        },
        input: String::new(),
        save: false,
        dry_run: false,
        scheduled: false,
        regeneration_reason: None,
    };
    let mut scan = app.scan(&request).unwrap();
    assert_eq!(scan.sources.len(), 1);
    let source = &mut scan.sources[0];
    let original = source.excerpt.clone();
    assert!(!original.is_empty(), "native scan failed: {source:?}");
    let doc = docs.values().next().unwrap();
    ocr::overlay(source, doc, &app.config, &Default::default()).unwrap();
    assert_eq!(source.excerpt, original, "native text is preserved");
    source.excerpt.clear();
    ocr::overlay(source, doc, &app.config, &Default::default()).unwrap();
    assert!(source.excerpt.contains("尚未核实"));
    assert_eq!(
        source.coverage_limits["evidence_audit"]["read_ranges"][0]["start"],
        1
    );
    let input = prepare_daily_input(&app, &request, &scan).unwrap();
    assert!(input.sources[0].unverified());
    assert!(input.gaps.iter().any(|g| g.code == "OCR_TEXT_UNVERIFIED"));
    fs::write(app.config.paths.work_dir.join("scan.pdf"), "changed").unwrap();
    assert_eq!(
        ocr::overlay(&mut scan.sources[0], doc, &app.config, &Default::default())
            .unwrap_err()
            .code,
        "OCR_SOURCE_CHANGED"
    );
}
#[test]
fn identity_cancellation_and_disabled_engine_never_start_work() {
    let f = Fixture::new();
    let limits = OcrLimits::default();
    assert_eq!(
        ocr::pdf_identity(
            &f.config,
            &f.config.paths.work_dir.join("scan.pdf"),
            &limits,
            Arc::new(AtomicBool::new(true)),
            Instant::now() + Duration::from_secs(10)
        )
        .unwrap_err()
        .code,
        "OCR_CANCELLED"
    );
    assert!(
        matches!(ocr::OcrEngine::new(PathBuf::from("missing"),vec![],limits,Arc::new(AtomicBool::new(false))),Err(e) if e.code=="OCR_DISABLED")
    );
}
#[cfg(windows)]
#[test]
fn explicit_engine_has_bounded_protocol_output_cancellation_and_job_budget() {
    let f = Fixture::new();
    let program = PathBuf::from("C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
    let script = f.manifest.parent().unwrap().join("engine.ps1");
    fs::write(&script,r#"$r=[Console]::In.ReadToEnd() | ConvertFrom-Json
$text='unverified'
$sha=[Security.Cryptography.SHA256]::Create()
$hash=([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($text)))).Replace('-','').ToLowerInvariant()
$pages=@($r.pages | ForEach-Object { @{page=$_;text=$text;text_sha256=$hash} })
$out=@{content_sha256=$r.content_sha256;pages=$pages;text_unverified=$true}|ConvertTo-Json -Depth 5 -Compress
[Console]::Out.Write($out)
"#).unwrap();
    let limits = OcrLimits {
        enabled: true,
        max_files: 1,
        ..Default::default()
    };
    let mut engine = ocr::OcrEngine::new(
        program.clone(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-File".into(),
            script.clone().into_os_string(),
        ],
        limits.clone(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let result = engine
        .recognize(&f.config, Path::new("scan.pdf"), &[1])
        .unwrap();
    assert_eq!(result.pages[0].text, "unverified");
    let out = ocr::export_document(&f.manifest.parent().unwrap().join("export"), &result).unwrap();
    assert_eq!(
        ocr::load_manifest(&f.config, &out, &limits).unwrap().len(),
        1
    );
    assert_eq!(
        engine
            .recognize(&f.config, Path::new("scan.pdf"), &[1])
            .unwrap_err()
            .code,
        "OCR_BUDGET_EXHAUSTED"
    );
    fs::write(&script, "[Console]::Out.Write('x' * 100000)").unwrap();
    let mut engine = ocr::OcrEngine::new(
        program,
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-File".into(),
            script.into_os_string(),
        ],
        limits,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(
        engine
            .recognize(&f.config, Path::new("scan.pdf"), &[1])
            .unwrap_err()
            .code,
        "OCR_OUTPUT_LIMIT"
    );
}
