#[path = "support/worker_path.rs"]
mod worker_path;
use ai_daily_report::{
    config::AppConfig,
    health,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    operations,
    reports::personal::{PersonalReport, MAX_FILE_CHARS, MAX_REPORT_BYTES},
    run::{run_daily, App},
    types::{new_run_id, utc_window, Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, FileTimes},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

struct Model(Arc<Mutex<Vec<Value>>>);

fn model_response(items: Vec<Value>) -> HttpResponse {
    let response = json!({"schema_version":5,"items":items});
    HttpResponse {
        status: 200,
        retry_after: None,
        body: serde_json::to_vec(&json!({
            "choices":[{"finish_reason":"stop","message":{"content":response.to_string()}}],
            "usage":{"prompt_tokens":100,"completion_tokens":100}
        }))
        .unwrap(),
    }
}

impl ModelTransport for Model {
    fn send(&self, bytes: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        let request: Value = serde_json::from_slice(bytes).unwrap();
        assert!(request["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with(ai_daily_report::model::personal::SCAN_PROMPT_V9));
        let input: Value =
            serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(input["max_chars"], 1500);
        let sources = input["sources"].as_array().unwrap();
        let source = sources
            .iter()
            .find(|source| source["text"].as_str().unwrap().contains("120"))
            .unwrap();
        let id = source["source_id"].clone();
        self.0.lock().unwrap().push(input);
        Ok(model_response(vec![json!({
            "title":"方案仍待审批", "text":"金额 120 元，满足条件后生效；责任人与截止日期待确认。",
            "kind":"attention", "statement_kind":"observed", "source_ids":[id]
        })]))
    }
}

fn install_pdf_worker(program_dir: &Path) {
    let worker = worker_path::pdf_worker();
    fs::copy(&worker, program_dir.join("ai-daily-pdf-worker.exe")).unwrap();
    fs::copy(
        worker.with_file_name("pdfium.dll"),
        program_dir.join("pdfium.dll"),
    )
    .unwrap();
}

fn oversized_corpus(fixture: &Fixture) {
    let start = utc_window(fixture.day).unwrap().0;
    for index in 0..40 {
        fixture.file(
            &format!("{index}.md"),
            &"金额待核对，未经审批不得生效。\n".repeat(350),
            start,
        );
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    app: App,
    day: NaiveDate,
    sent: Arc<Mutex<Vec<Value>>>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        fs::create_dir(dir.path().join("work")).unwrap();
        fs::write(dir.path().join("config/settings.yaml"),json!({
            "paths":{"work_dir":"work"}, "report":{"profile":"personal"},
            "archive":{"allow_work_dir_for_model":true},
            "llm":{"max_request_utf8_bytes":256000,"context_window_tokens":256000,"verified_max_output_tokens":8192,"max_tokens":4096},
            "recovery":{"enforce_model_budgets":false}
        }).to_string()).unwrap();
        let mut config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
        config.pdf_worker_path = worker_path::pdf_worker();
        assert_eq!(
            config.scanner.max_workers,
            Some(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u64)
        );
        assert_eq!(config.scanner.total_max_chars, Some(MAX_FILE_CHARS as u64));
        assert_eq!(config.scanner.total_deadline_ms, Some(120_000));
        let mut app = App::open(config, Utc::now()).unwrap();
        let sent = Arc::new(Mutex::new(vec![]));
        app.transport = Box::new(Model(sent.clone()));
        let day = Utc::now()
            .with_timezone(&ai_daily_report::types::shanghai())
            .date_naive()
            .pred_opt()
            .unwrap();
        Self {
            _dir: dir,
            app,
            day,
            sent,
        }
    }
    fn file(&self, name: &str, text: &str, at: DateTime<Utc>) {
        let path = self.app.config.paths.work_dir.join(name);
        fs::write(&path, text).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(at.timestamp() as u64)),
            )
            .unwrap();
    }
    fn request(&self) -> ReportRequest {
        ReportRequest {
            context: RunContext::new(new_run_id(), Utc::now(), self.day, String::new()).unwrap(),
            period: Period {
                mode: ReportMode::Daily,
                source: ReportSource::Scan,
                start: self.day,
                end: self.day,
                label: self.day.to_string(),
            },
            input: String::new(),
            save: true,
            dry_run: false,
            scheduled: true,
            regeneration_reason: None,
        }
    }
    fn cached_observations(&self) -> i64 {
        rusqlite::Connection::open(&self.app.config.index_db_path)
            .unwrap()
            .query_row("SELECT count(*) FROM scan_snapshots", [], |r| r.get(0))
            .unwrap()
    }
}

#[test]
fn daily_scan_covers_calendar_boundaries_publishes_and_reuses_without_collection() {
    let f = Fixture::new();
    let (start, end) = utc_window(f.day).unwrap();
    let text = "金额 120 元。满足条件后生效；例外情况仍需审批，责任人与截止日期待确认。";
    f.file("start.md", text, start);
    f.file("end.md", text, end - chrono::Duration::seconds(1));
    f.file(
        "older.md",
        "previous day",
        start - chrono::Duration::seconds(1),
    );
    f.file("today.md", "next day", end);
    let request = f.request();
    assert_eq!(request.context.target_date, f.day);
    let result = run_daily(request, &f.app).unwrap();
    assert_eq!(result.action, "published");
    let sent = f.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let sources = sent[0]["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2);
    for source in sources {
        assert!(matches!(
            source["path"].as_str(),
            Some("start.md" | "end.md")
        ));
        assert!(source["text"]
            .as_str()
            .unwrap()
            .replace('\n', "")
            .ends_with(text));
        assert_eq!(source["parts"][0]["side"], "current");
    }
    drop(sent);
    let stored = f.app.store.get(&result.run_id).unwrap();
    let report: PersonalReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    report.validate().unwrap();
    for refs in report.evidence.values() {
        assert!(refs[0].version_id.starts_with("scan-content:"));
        assert!(!refs[0].ranges.is_empty());
    }
    let attachment = report.evidence_attachment.unwrap();
    assert!(attachment.markdown.contains("原文来自扫描时的观察"));
    assert!(attachment.markdown.contains("当天中间版本、已删除文件"));
    assert!(!f
        .app
        .config
        .paths
        .data_dir
        .join("daily_cache/archive")
        .exists());
    let check = health::check(&f.app.config, f.day, Utc::now()).unwrap();
    assert!(!check
        .checks
        .iter()
        .any(|c| matches!(c.code, "COLLECTION_GAP" | "PARSE_QUEUE")));
    let scans = f.cached_observations();
    fs::rename(
        &f.app.config.paths.work_dir,
        f.app.config.paths.work_dir.with_file_name("offline-work"),
    )
    .unwrap();
    let again = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(again.action, "existing");
    assert_eq!(again.markdown, result.markdown);
    assert_eq!(f.cached_observations(), scans);
    assert_eq!(f.sent.lock().unwrap().len(), 1);
    let path = Path::new(result.output_path.as_ref().unwrap())
        .parent()
        .unwrap()
        .join(attachment.link(&f.day.to_string()));
    fs::remove_file(&path).unwrap();
    run_daily(f.request(), &f.app).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), attachment.markdown);
}

#[test]
fn expanded_scan_material_reaches_model_and_large_attachment_is_restored_without_loss() {
    let mut f = Fixture::new();
    f.app.config.llm.max_request_utf8_bytes = 1_000_000;
    f.app.config.llm.context_window_tokens = Some(1_000_000);
    let start = utc_window(f.day).unwrap().0;
    let files: BTreeMap<_, _> = (0..30)
        .map(|index| {
            let name = format!("material-{index:02}.md");
            let text: String = (0..100)
                .map(|line| {
                    format!(
                        "材料 {index:02} 第 {line:03} 项：金额 120 元，审批条件和负责人待确认。\n"
                    )
                })
                .collect();
            f.file(&name, &text, start);
            (name, text)
        })
        .collect();
    let material_chars: usize = files.values().map(|text| text.chars().count()).sum();
    assert!(material_chars > 100_000);
    assert!(material_chars < MAX_FILE_CHARS);

    let result = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(result.action, "published");
    let sent = f.sent.lock().unwrap();
    assert_eq!(
        sent.len(),
        1,
        "{}",
        f.app
            .store
            .get(&result.run_id)
            .unwrap()
            .report_json
            .unwrap()
    );
    let sources = sent[0]["sources"].as_array().unwrap();
    let mut received = BTreeMap::<&str, String>::new();
    for source in sources {
        let path = source["path"].as_str().unwrap();
        received
            .entry(path)
            .or_default()
            .push_str(source["text"].as_str().unwrap());
    }
    assert_eq!(received.len(), files.len());
    for (path, expected) in &files {
        let text = &received[path.as_str()];
        for line in expected.lines() {
            assert!(text.contains(line), "{path}: {line}");
        }
    }
    drop(sent);

    let stored = f.app.store.get(&result.run_id).unwrap();
    let report: PersonalReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    report.validate().unwrap();
    assert_eq!(report.selection.selected_files, files.len() as u64);
    assert!(report.selection.selected_chars >= material_chars);
    assert_eq!(report.selection.omissions.get("budget"), None);
    let attachment = report.evidence_attachment.as_ref().unwrap();
    assert!(attachment.markdown.len() > 100_000);
    assert!(report.output_bytes(&result.markdown) <= MAX_REPORT_BYTES);
    for text in files.values() {
        for line in text.lines() {
            assert!(attachment.markdown.contains(line));
        }
    }
    let mut frozen = BTreeMap::<&str, String>::new();
    for source in &report.sources {
        let path = source.relative_path.as_deref().unwrap();
        for evidence in &report.evidence[&source.source_id] {
            frozen.entry(path).or_default().push_str(&evidence.quote);
        }
    }
    for (path, expected) in &files {
        for line in expected.lines() {
            assert!(frozen[path.as_str()].contains(line), "{path}: {line}");
        }
    }
    let path = result.output_path.as_ref().unwrap();
    let evidence_path = path
        .parent()
        .unwrap()
        .join(attachment.link(&f.day.to_string()));
    let original = fs::read(path).unwrap();
    let original_evidence = attachment.markdown.as_bytes();
    assert_eq!(fs::read(&evidence_path).unwrap(), original_evidence);
    assert!(result
        .markdown
        .contains(&attachment.link(&f.day.to_string())));

    let scans = f.cached_observations();
    fs::rename(
        &f.app.config.paths.work_dir,
        f.app.config.paths.work_dir.with_file_name("offline-work"),
    )
    .unwrap();
    fs::remove_file(&evidence_path).unwrap();
    ai_daily_report::run::repair_output(&f.app, &f.day.to_string(), Utc::now()).unwrap();
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(fs::read(&evidence_path).unwrap(), original_evidence);
    let again = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(again.action, "existing");
    assert_eq!(again.markdown, result.markdown);
    assert_eq!(f.cached_observations(), scans);
    assert_eq!(f.sent.lock().unwrap().len(), 1);
}

#[test]
fn expanded_context_admits_all_files_before_bounding_personal_material() {
    let mut f = Fixture::new();
    let settings_path = f.app.config.config_dir.join("settings.yaml");
    let mut settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    settings["scanner"]["context_max_chars"] = json!(5_000_000);
    settings["scanner"]["text_max_chars"] = json!(60_000);
    settings["scanner"]["text_excerpt_max_chars"] = json!(60_000);
    settings["llm"]["max_request_utf8_bytes"] = json!(1_600_000);
    settings["llm"]["context_window_tokens"] = json!(1_050_000);
    fs::write(&settings_path, settings.to_string()).unwrap();
    let mut config = AppConfig::load_with_env(f._dir.path(), None, &BTreeMap::new()).unwrap();
    config.pdf_worker_path = worker_path::pdf_worker();
    f.app.config = config;
    let start = utc_window(f.day).unwrap().0;
    let files: BTreeMap<_, _> = (0..12)
        .map(|index| {
            let path = format!("wide-context-{index:02}.md");
            let marker = format!("材料 {index:02}：");
            let line = format!(
                "{marker}{}。\n",
                "金额 120 元，审批条件和负责人待确认；".repeat(7)
            );
            f.file(&path, &line.repeat(300), start);
            (path, marker)
        })
        .collect();

    let result = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(result.action, "published");
    let sent = f.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let sources = sent[0]["sources"].as_array().unwrap();
    let mut seen_paths = std::collections::BTreeSet::new();
    for source in sources {
        let path = source["path"].as_str().unwrap();
        if source["text"]
            .as_str()
            .unwrap()
            .contains(files[path].as_str())
        {
            seen_paths.insert(path);
        }
    }
    assert_eq!(seen_paths.len(), files.len());
    let selected_chars: usize = sources
        .iter()
        .map(|source| source["text"].as_str().unwrap().chars().count())
        .sum();
    assert!(selected_chars > 100_000);
    assert!(selected_chars <= MAX_FILE_CHARS);
    drop(sent);

    let (parsed_files, parsed_chars): (i64, i64) =
        rusqlite::Connection::open(&f.app.config.index_db_path)
            .unwrap()
            .query_row(
                "SELECT count(*), coalesce(sum(length(content_text)), 0) FROM parse_cache",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    assert_eq!(parsed_files, files.len() as i64);
    assert!(parsed_chars > 500_000);
    let stored = f.app.store.get(&result.run_id).unwrap();
    let report: PersonalReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    report.validate().unwrap();
    assert_eq!(report.selection.selected_files, files.len() as u64);
    assert_eq!(report.selection.selected_chars, selected_chars);
    assert_eq!(report.selection.omissions.get("read_failed"), None);
    assert!(!report
        .gaps
        .iter()
        .any(|gap| gap.code == "SOURCE_UNAVAILABLE"));
    assert!(report.sources.iter().all(|source| source.usable()));
}

#[test]
fn scan_dry_run_keeps_budgets_and_reports_unread_ranges_without_model_calls() {
    let f = Fixture::new();
    oversized_corpus(&f);
    let mut request = f.request();
    request.dry_run = true;
    let result = run_daily(request, &f.app).unwrap();
    assert_eq!(result.action, "dry_run");
    assert_eq!(result.model_calls, 0);
    let selection: Value = serde_json::from_str(&result.markdown).unwrap();
    let selected_chars = selection["summary"]["selected_chars"].as_u64().unwrap();
    assert!(selected_chars > 20_000);
    assert!(selected_chars <= MAX_FILE_CHARS as u64);
    let gaps = selection["gaps"].as_array().unwrap();
    assert!(gaps
        .iter()
        .any(|gap| gap["code"] == "PERSONAL_REPORT_TOO_LARGE"));
    assert!(gaps.iter().any(|gap| gap["code"] == "PARSE_BOUNDED"
        || gap["code"] == "EXCERPT_BOUNDED"
        || gap["code"] == "SCAN_SOURCE_OMITTED"));
    assert!(f.sent.lock().unwrap().is_empty());
    assert!(f.app.store.current(&f.day.to_string()).unwrap().is_none());
    let observations = f.cached_observations();
    let mut request = f.request();
    request.dry_run = true;
    run_daily(request, &f.app).unwrap();
    assert_eq!(f.cached_observations(), observations);
    let execution = f
        .app
        .scan(&f.request())
        .unwrap()
        .evidence()
        .execution_metrics;
    assert_eq!(execution.parse_attempt_count, 0);
    assert!(execution.snapshot_hit);
}

#[test]
fn oversized_selected_material_keeps_the_previous_publication_and_frozen_input() {
    let f = Fixture::new();
    f.file(
        "prior.md",
        "金额 120 元，审批后生效。",
        utc_window(f.day).unwrap().0,
    );
    let original = run_daily(f.request(), &f.app).unwrap();
    let path = original.output_path.unwrap();
    let report: PersonalReport = serde_json::from_str(
        f.app
            .store
            .get(&original.run_id)
            .unwrap()
            .report_json
            .as_deref()
            .unwrap(),
    )
    .unwrap();
    let attachment = report.evidence_attachment.unwrap();
    let evidence_path = path.parent().unwrap().join(attachment.link(&report.date));

    oversized_corpus(&f);
    let mut regenerate = f.request();
    regenerate.scheduled = false;
    regenerate.regeneration_reason = Some("核验完整附件的容量".into());
    let id = regenerate.context.run_id.clone();
    assert_eq!(
        run_daily(regenerate, &f.app).unwrap_err().code,
        "PERSONAL_REPORT_TOO_LARGE"
    );
    let failed = f.app.store.get(&id).unwrap();
    assert!(failed.report_json.is_none());
    let snapshot = failed.snapshot_path.unwrap();
    assert!(snapshot.starts_with(&format!("{id}/personal-materials-")));
    let staged = f
        .app
        .processing
        .as_ref()
        .unwrap()
        .quota
        .root
        .join("snapshots")
        .join(snapshot);
    let bytes = fs::read(staged).unwrap();
    assert_eq!(
        Some(ai_daily_report::snapshots::digest(&bytes)),
        failed.snapshot_sha256
    );
    let material: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(material["plan"].is_null());
    assert!(
        material["selection"]["summary"]["selected_chars"]
            .as_u64()
            .unwrap()
            > 20_000
    );
    assert!(f.app.store.calls(&id).unwrap().is_empty());
    assert_eq!(f.sent.lock().unwrap().len(), 1);

    assert_eq!(
        f.app.store.current(&report.date).unwrap().unwrap().run_id,
        original.run_id
    );
    assert_eq!(fs::read(path).unwrap(), original.markdown.as_bytes());
    assert_eq!(
        fs::read(evidence_path).unwrap(),
        attachment.markdown.as_bytes()
    );
}

struct Interrupted;

#[test]
fn size_policy_and_metadata_only_office_have_distinct_gaps() {
    let mut f = Fixture::new();
    f.app.config.scanner.max_file_size_mb = Some(1);
    let work = &f.app.config.paths.work_dir;
    let empty = work.join("empty.pptx");
    fs::write(&empty, include_bytes!("fixtures/office-metadata-only.pptx")).unwrap();
    let large = work.join("large.xlsx");
    File::create_new(&large)
        .unwrap()
        .set_len(1_048_577)
        .unwrap();
    let modified =
        UNIX_EPOCH + Duration::from_secs(utc_window(f.day).unwrap().0.timestamp() as u64);
    for path in [&empty, &large] {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
    }
    let mut request = f.request();
    request.dry_run = true;
    let result = run_daily(request, &f.app).unwrap();
    let selection: Value = serde_json::from_str(&result.markdown).unwrap();
    let omissions = &selection["summary"]["omissions"];
    assert_eq!(omissions["file_size_policy"], 1);
    assert_eq!(omissions["office_content_missing"], 1);
    let gaps = selection["gaps"].as_array().unwrap();
    assert!(gaps
        .iter()
        .any(|gap| gap["code"] == "OFFICE_CONTENT_MISSING"));
    assert!(gaps.iter().any(|gap| gap["code"] == "FILE_SIZE_LIMIT"
        && gap["message"].as_str().unwrap().contains("1048576")));
    assert!(f.sent.lock().unwrap().is_empty());
}

#[test]
fn reloaded_configuration_uses_the_adjacent_pdf_worker_for_daily_evidence() {
    use std::fmt::Write;
    let mut f = Fixture::new();
    install_pdf_worker(f._dir.path());
    let config = AppConfig::load_with_env(f._dir.path(), None, &BTreeMap::new()).unwrap();
    assert_eq!(
        config.pdf_worker_path,
        config.program_dir.join("ai-daily-pdf-worker.exe")
    );
    drop(f.app);
    f.app = App::open(config, Utc::now()).unwrap();

    let sentence = "Amount: CNY 120. Status: pending approval.";
    let stream = format!("BT /F1 12 Tf 72 720 Td ({sentence}) Tj ET\n");
    let page_stream = format!("<< /Length {} >>\nstream\n{stream}endstream", stream.len());
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        &page_stream,
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.into_iter().enumerate() {
        offsets.push(pdf.len());
        writeln!(pdf, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
    }
    let xref = pdf.len();
    pdf.push_str("xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        pdf,
        "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
    )
    .unwrap();
    f.file("reload.pdf", &pdf, utc_window(f.day).unwrap().0);
    let mut request = f.request();
    request.dry_run = true;
    let scan = run_daily(request, &f.app).unwrap();
    assert_eq!(scan.model_calls, 0);
    let selection: ai_daily_report::reports::personal::Selection =
        serde_json::from_str(&scan.markdown).unwrap();
    let [material] = selection.materials.as_slice() else {
        panic!("expected one PDF material, got {:?}", selection.gaps);
    };
    assert_eq!(
        material.source.parser_backend.as_deref(),
        Some("rust_pdfium_v3")
    );
    assert!(material.text.contains(sentence));
    assert!(material
        .evidence
        .iter()
        .flat_map(|evidence| &evidence.ranges)
        .any(|range| {
            range.unit == daily_parsers::contract::SourceRangeUnit::PdfPage
                && (range.start, range.end) == (1, 2)
        }));
}

#[test]
fn staged_first_publication_rechecks_permissions_but_published_byte_repair_is_historical() {
    use ai_daily_report::{publish::Publisher, snapshots::SnapshotRef, store::StagedReport};
    let f = Fixture::new();
    f.file("receipt.md", "收款120元。", utc_window(f.day).unwrap().0);
    let mut preview = f.request();
    preview.save = false;
    preview.scheduled = false;
    let preview = run_daily(preview, &f.app).unwrap();
    let preview = f.app.store.get(&preview.run_id).unwrap();
    let value: Value = serde_json::from_str(preview.report_json.as_deref().unwrap()).unwrap();
    let report: PersonalReport = serde_json::from_value(value.clone()).unwrap();
    let markdown = preview.markdown.unwrap();
    let mut context = f.request().context;
    context.config_fingerprint = f.app.fingerprint(false).unwrap();
    let id = &context.run_id;
    let now = Utc::now();
    f.app
        .store
        .create_personal(&context, "report", id, None, now)
        .unwrap();
    let snapshot = SnapshotRef {
        path: preview.snapshot_path.unwrap(),
        sha256: preview.snapshot_sha256.unwrap(),
    };
    f.app
        .store
        .prepare(id, &snapshot, &preview.input_fingerprint.unwrap(), &[], now)
        .unwrap();
    let path = Publisher::new(&f.app.config.paths.reports_dir, &f.app.store)
        .unwrap()
        .output_path(&context.target_date.to_string())
        .unwrap();
    let attachment = report.evidence_attachment.unwrap();
    let appendix = path.parent().unwrap().join(attachment.link(&report.date));
    f.app
        .store
        .stage_report(
            id,
            &StagedReport {
                report: &value,
                markdown: &markdown,
                output_path: path.to_str().unwrap(),
                publication_fingerprint: &ai_daily_report::snapshots::digest(markdown.as_bytes()),
            },
            now,
        )
        .unwrap();
    let settings_path = f.app.config.config_dir.join("settings.yaml");
    let original = fs::read(&settings_path).unwrap();
    let mut restricted: Value = serde_json::from_slice(&original).unwrap();
    restricted["scanner"]["ignored_patterns"] = json!(["receipt.md"]);
    fs::write(&settings_path, restricted.to_string()).unwrap();
    assert_eq!(
        run_daily(f.request(), &f.app).unwrap_err().code,
        "ARCHIVE_MODEL_PERMISSION_DENIED"
    );
    let repair = || ai_daily_report::run::repair_output(&f.app, &report.date, Utc::now());
    assert_eq!(
        repair().unwrap_err().code,
        "ARCHIVE_MODEL_PERMISSION_DENIED"
    );
    assert!(!path.exists() && !appendix.exists());
    assert!(f.app.store.current(&report.date).unwrap().is_none());
    fs::write(&settings_path, original).unwrap();
    repair().unwrap();
    fs::write(&settings_path, restricted.to_string()).unwrap();
    fs::remove_file(&path).unwrap();
    fs::remove_file(&appendix).unwrap();
    repair().unwrap();
    assert_eq!(fs::read(path).unwrap(), markdown.as_bytes());
    assert_eq!(fs::read(appendix).unwrap(), attachment.markdown.as_bytes());
    assert_eq!(f.sent.lock().unwrap().len(), 1);
}

#[test]
fn permission_revoked_after_reservation_is_a_known_failure_and_can_resume() {
    struct RevokeBeforeSend {
        settings: std::path::PathBuf,
        store: ai_daily_report::store::Store,
        run_id: String,
    }
    impl ModelTransport for RevokeBeforeSend {
        fn before_send(&self) -> Result<(), ai_daily_report::ReportError> {
            let calls = self.store.calls(&self.run_id).unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0]["status"], "reserved");
            let mut settings: Value =
                serde_json::from_slice(&fs::read(&self.settings).unwrap()).unwrap();
            settings["scanner"]["ignored_patterns"] = json!(["permission.md"]);
            fs::write(&self.settings, settings.to_string()).unwrap();
            Ok(())
        }

        fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
            panic!("permission was revoked before dispatch");
        }
    }

    let mut f = Fixture::new();
    f.file(
        "permission.md",
        "金额 120 元，审批后生效。",
        utc_window(f.day).unwrap().0,
    );
    let request = f.request();
    let id = request.context.run_id.clone();
    let settings = f.app.config.config_dir.join("settings.yaml");
    let original = fs::read(&settings).unwrap();
    f.app.transport = Box::new(RevokeBeforeSend {
        settings: settings.clone(),
        store: f.app.store.clone(),
        run_id: id.clone(),
    });
    assert_eq!(
        run_daily(request, &f.app).unwrap_err().code,
        "ARCHIVE_MODEL_PERMISSION_DENIED"
    );
    assert!(f.app.store.current(&f.day.to_string()).unwrap().is_none());
    let calls = f.app.store.calls(&id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["status"], "failed");
    assert_eq!(calls[0]["error_code"], "ARCHIVE_MODEL_PERMISSION_DENIED");
    assert!(calls[0]["response_path"].is_null());
    assert!(f.sent.lock().unwrap().is_empty());

    fs::write(settings, original).unwrap();
    f.app.transport = Box::new(Model(f.sent.clone()));
    let result = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(
        (result.run_id, result.action.as_str()),
        (id.clone(), "published")
    );
    let resumed = f.app.store.calls(&id).unwrap();
    assert_eq!(resumed.len(), 2);
    assert_eq!(resumed[0], calls[0]);
    assert_eq!(resumed[1]["status"], "succeeded");
    assert_eq!(resumed[1]["request_sha256"], calls[0]["request_sha256"]);
    assert_eq!(f.sent.lock().unwrap().len(), 1);
}

#[test]
fn publication_rechecks_cited_and_supplemental_materials_after_the_model_returns() {
    struct ChangePermission {
        config: std::path::PathBuf,
        exclude: &'static str,
    }
    impl ModelTransport for ChangePermission {
        fn send(&self, bytes: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            let input: Value =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            let cited = input["sources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["path"] == "cited.md")
                .unwrap();
            let mut settings: Value =
                serde_json::from_slice(&fs::read(&self.config).unwrap()).unwrap();
            settings["scanner"]["ignored_patterns"] = json!([self.exclude]);
            fs::write(&self.config, settings.to_string()).unwrap();
            Ok(model_response(vec![
                json!({"title":"金额已登记","text":"金额为120元。",
                "kind":"progress","statement_kind":"observed","source_ids":[cited["source_id"]]}),
            ]))
        }
    }
    for exclude in ["cited.md", "supplement.md"] {
        let mut f = Fixture::new();
        let start = utc_window(f.day).unwrap().0;
        f.file("cited.md", "金额为120元。", start);
        f.file("supplement.md", "内部补充材料。", start);
        let settings_path = f.app.config.config_dir.join("settings.yaml");
        let original = fs::read(&settings_path).unwrap();
        f.app.transport = Box::new(ChangePermission {
            config: settings_path.clone(),
            exclude,
        });
        let request = f.request();
        let id = request.context.run_id.clone();
        let date = f.day.to_string();
        assert_eq!(
            run_daily(request, &f.app).unwrap_err().code,
            "ARCHIVE_MODEL_PERMISSION_DENIED"
        );
        assert!(f.app.store.current(&date).unwrap().is_none());
        let failed = f.app.store.get(&id).unwrap();
        assert!(failed.report_json.is_none());
        assert!(failed.snapshot_path.is_some());
        let calls = f.app.store.calls(&id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["status"], "succeeded");

        fs::write(settings_path, original).unwrap();
        f.app.transport = Box::new(Interrupted);
        let outcome = run_daily(f.request(), &f.app).unwrap();
        assert_eq!(outcome.run_id, id);
        assert_eq!(outcome.action, "published");
        assert_eq!(f.app.store.calls(&id).unwrap(), calls);
        let record = f.app.store.get(&id).unwrap();
        let report: PersonalReport =
            serde_json::from_str(record.report_json.as_deref().unwrap()).unwrap();
        assert!(report
            .evidence_attachment
            .unwrap()
            .markdown
            .contains("内部补充材料"));
    }
}

#[test]
fn daily_compacts_eight_grounded_items_and_restores_full_uncited_evidence() {
    struct EchoDetails;
    impl ModelTransport for EchoDetails {
        fn send(&self, bytes: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
            let request: Value = serde_json::from_slice(bytes).unwrap();
            let input: Value =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            assert_eq!(input["max_chars"], 1500);
            if let Some(draft) = input["draft_items"].as_array() {
                assert!(request["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .starts_with(ai_daily_report::model::personal::COMPACT_PROMPT_V8));
                assert!(input["draft_chars"].as_u64().unwrap() > 1500);
                let mut items = draft.clone();
                for item in &mut items {
                    let (fact, _) = item["text"].as_str().unwrap().split_once('。').unwrap();
                    item["text"] = json!(format!("{fact}。"));
                }
                return Ok(model_response(items));
            }
            assert!(request["messages"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with(ai_daily_report::model::personal::SCAN_PROMPT_V9));
            let sources = input["sources"].as_array().unwrap();
            let mut items = Vec::new();
            for i in 1..=8 {
                let path = format!("项目{i}.md");
                let fact = format!(
                    "项目{i}第1批的合同预算为{}元，支付前须核对发票和实收数量。",
                    i * 100 + 1
                );
                let source = sources
                    .iter()
                    .find(|source| {
                        source["path"] == path && source["text"].as_str().unwrap().contains(&fact)
                    })
                    .unwrap_or_else(|| panic!("{path} lacks {fact}"));
                items.push(
                    json!({"title":format!("项目{i}预算待复核"),"text":fact.repeat(6),
                    "kind":"attention","statement_kind":"observed",
                    "source_ids":[source["source_id"]] }),
                );
            }
            Ok(model_response(items))
        }
    }
    let mut fixture = Fixture::new();
    fixture.app.transport = Box::new(EchoDetails);
    let start = utc_window(fixture.day).unwrap().0;
    for i in 1..=8 {
        let source: String = (1..=8)
            .map(|j| {
                format!(
                    "项目{i}第{j}批的合同预算为{}元，支付前须核对发票和实收数量。\n",
                    i * 100 + j
                )
            })
            .collect();
        fixture.file(&format!("项目{i}.md"), &source, start);
    }
    let other = "补充原文：样品需留存三十日。";
    fixture.file("补充材料.md", other, start);
    let result = run_daily(fixture.request(), &fixture.app).unwrap();
    let stored = fixture.app.store.get(&result.run_id).unwrap();
    let report: PersonalReport =
        serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
    assert_eq!(report.schema_version, 6);
    assert_eq!(report.items.len(), 8);
    let body = result
        .markdown
        .split_once("\n\n")
        .unwrap()
        .1
        .split_once("\n[完整依据](")
        .unwrap()
        .0;
    assert!(body.chars().count() <= 1500);
    assert!(!result.markdown.contains(other));
    let attachment = report.evidence_attachment.as_ref().unwrap();
    assert!(attachment.markdown.contains(other));
    assert!(report.output_bytes(&result.markdown) <= MAX_REPORT_BYTES);
    for (index, item) in report.items.iter().enumerate() {
        let i = index + 1;
        assert!(item.text.contains(&format!("预算为{}元", i * 100 + 1)));
        let label = report
            .sources
            .iter()
            .position(|source| source.source_id == item.source_ids[0])
            .unwrap()
            + 1;
        assert!(body.contains(&format!("{}#s{label:03}", attachment.link(&report.date))));
        for j in 1..=8 {
            assert!(attachment
                .markdown
                .contains(&format!("项目{i}第{j}批的合同预算为{}元", i * 100 + j)));
        }
    }
    let calls = fixture.app.store.calls(&result.run_id).unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["logical_step"], "personal_summary");
    assert_eq!(calls[1]["logical_step"], "personal_summary.compact");
    assert!(calls.iter().all(|call| call["status"] == "succeeded"));
    let path = result
        .output_path
        .as_ref()
        .unwrap()
        .parent()
        .unwrap()
        .join(attachment.link(&report.date));
    fs::remove_file(&path).unwrap();
    assert_eq!(
        run_daily(fixture.request(), &fixture.app).unwrap().action,
        "existing"
    );
    assert_eq!(fs::read(path).unwrap(), attachment.markdown.as_bytes());
    assert_eq!(fixture.app.store.calls(&result.run_id).unwrap(), calls);
}

impl ModelTransport for Interrupted {
    fn prepare(&self) -> Result<(), ai_daily_report::ReportError> {
        panic!("interrupted after input freeze")
    }
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        unreachable!()
    }
}

#[test]
fn interrupted_scan_resumes_frozen_content_after_source_changes() {
    let mut f = Fixture::new();
    let start = utc_window(f.day).unwrap().0;
    let original = "金额 120 元。满足条件后生效；责任人与截止日期待确认。";
    f.file("work.md", original, start);
    f.app.transport = Box::new(Interrupted);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_daily(
            f.request(),
            &f.app
        )))
        .is_err()
    );
    let scans = f.cached_observations();
    f.file("work.md", "changed later", Utc::now());
    f.app.transport = Box::new(Model(f.sent.clone()));
    let result = run_daily(f.request(), &f.app).unwrap();
    assert_eq!(result.action, "published");
    assert_eq!(f.cached_observations(), scans);
    assert!(f.sent.lock().unwrap()[0]["sources"][0]["text"]
        .as_str()
        .unwrap()
        .replace('\n', "")
        .ends_with(original));
}

#[test]
fn legacy_scan_cap_resumes_saved_response_with_same_yaml_and_rejects_other_changes() {
    use ai_daily_report::{
        model::{personal::prepare_detailed_scan, ModelRunner},
        reports::personal::{Selection, DETAILED_SCAN_TEMPLATE},
        run::personal::{finish, PersonalInput},
        snapshots::{fingerprint, SnapshotStore},
    };
    use std::time::Instant;

    struct LegacyModel {
        request: Vec<u8>,
        sent: Arc<Mutex<Vec<Value>>>,
    }
    impl ModelTransport for LegacyModel {
        fn send(&self, bytes: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
            assert_eq!(bytes, self.request);
            let request: Value = serde_json::from_slice(bytes).unwrap();
            let input: Value =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            let id = input["sources"][0]["source_id"].clone();
            self.sent.lock().unwrap().push(input);
            Ok(model_response(vec![json!({
                "title":"金额待确认","text":"金额 120 元，满足条件后生效。",
                "kind":"attention","statement_kind":"observed","source_ids":[id]
            })]))
        }
    }

    for configured in [None, Some(50_000)] {
        let mut f = Fixture::new();
        let settings_path = f.app.config.config_dir.join("settings.yaml");
        let mut settings: Value =
            serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        if let Some(chars) = configured {
            settings["scanner"]["total_max_chars"] = json!(chars);
        }
        fs::write(&settings_path, settings.to_string()).unwrap();
        let yaml = fs::read(&settings_path).unwrap();
        f.app.config = AppConfig::load_with_env(f._dir.path(), None, &BTreeMap::new()).unwrap();
        f.app.config.scanner.total_max_chars = Some(18_000);
        f.app.config.scanner.max_workers = Some(1);
        let original = "金额 120 元。满足条件后生效；责任人与截止日期待确认。";
        f.file("work.md", original, utc_window(f.day).unwrap().0);
        let mut preview = f.request();
        preview.scheduled = false;
        preview.dry_run = true;
        let selection: Selection =
            serde_json::from_str(&run_daily(preview, &f.app).unwrap().markdown).unwrap();
        let mut context = f.request().context;
        context.config_fingerprint = f.app.fingerprint(false).unwrap();
        let plan = prepare_detailed_scan(&f.app.config.llm, &context, &selection).unwrap();
        assert_eq!(plan.version, 7);
        let mut frozen = json!({
            "version":6, "context":context, "selection":selection,
            "plan":plan, "template":DETAILED_SCAN_TEMPLATE
        });
        let input_fingerprint = fingerprint(&frozen).unwrap();
        frozen["input_fingerprint"] = json!(input_fingerprint);
        frozen["recovery_fingerprint"] = json!(f.app.fingerprint(true).unwrap());
        let id = &context.run_id;
        let now = context.started_at;
        f.app
            .store
            .create_personal(&context, "report", id, None, now)
            .unwrap();
        f.app.store.set_scanning(id, now).unwrap();
        let session = f.app.processing.as_ref().unwrap();
        let snapshots = SnapshotStore::with_quota(
            session.quota.root.join("snapshots"),
            ai_daily_report::processing_cache::MAX_BYTES,
            Arc::clone(&session.quota),
        )
        .unwrap();
        let input_ref = snapshots.put(id, "personal-input", &frozen).unwrap();
        let sources = selection
            .materials
            .iter()
            .map(|material| json!(material.source))
            .collect::<Vec<_>>();
        f.app
            .store
            .prepare(id, &input_ref, &input_fingerprint, &sources, now)
            .unwrap();
        let run = f.app.store.get(id).unwrap();
        let frozen_bytes = snapshots.read_bytes(&input_ref).unwrap();
        let input: PersonalInput =
            serde_json::from_value(snapshots.read(&input_ref).unwrap()).unwrap();
        assert_eq!(input.version, 6);
        let model = LegacyModel {
            request: plan.bytes().unwrap(),
            sent: f.sent.clone(),
        };
        let started = Instant::now();
        let saved = ModelRunner {
            store: &f.app.store,
            snapshots: &snapshots,
            transport: &model,
            llm: &f.app.config.llm,
            allow_new_calls: true,
            run_id: &run.run_id,
            started_at: Utc::now(),
            started,
            deadline: started + Duration::from_secs(60),
        }
        .execute(input.plan.as_ref().unwrap())
        .unwrap();
        assert!(!saved.reused);
        let calls = f.app.store.calls(&run.run_id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["status"], "succeeded");

        let config = AppConfig::load_with_env(f._dir.path(), None, &BTreeMap::new()).unwrap();
        assert_eq!(
            config.scanner.total_max_chars,
            Some(configured.unwrap_or(MAX_FILE_CHARS as u64))
        );
        assert_ne!(config.fingerprint(false).unwrap(), run.config_fingerprint);
        assert!(config
            .matches_personal_fingerprint(&run.config_fingerprint, false)
            .unwrap());
        assert!(config
            .matches_personal_fingerprint(&input.recovery_fingerprint, true)
            .unwrap());
        let Fixture {
            _dir,
            app,
            day,
            sent,
        } = f;
        drop(app);
        let mut f = Fixture {
            app: App::open(config, Utc::now()).unwrap(),
            _dir,
            day,
            sent,
        };
        f.app.transport = Box::new(Interrupted);
        let unchanged = f.app.config.clone();
        let mut restricted = unchanged.clone();
        restricted.scanner.excluded_dirs = Some(vec![unchanged
            .paths
            .work_dir
            .to_string_lossy()
            .into_owned()]);
        assert!(!ai_daily_report::evidence::permitted(
            &restricted,
            &unchanged.paths.work_dir.join("work.md")
        ));
        let mut different_model = unchanged.clone();
        different_model.llm.model_id.push_str("-changed");
        let mut different_path = unchanged.clone();
        different_path.paths.work_dir = different_path.paths.work_dir.join("other-work");
        for changed in [restricted, different_model, different_path] {
            f.app.config = changed;
            let started = Instant::now();
            assert_eq!(
                finish(
                    &f.app,
                    &run.run_id,
                    started,
                    started + Duration::from_secs(60)
                )
                .unwrap_err()
                .code,
                "SNAPSHOT_CONTEXT_MISMATCH"
            );
        }
        f.app.config = unchanged;
        let scans = f.cached_observations();
        fs::rename(
            &f.app.config.paths.work_dir,
            f.app.config.paths.work_dir.with_file_name("offline-work"),
        )
        .unwrap();
        let result = run_daily(f.request(), &f.app).unwrap();
        assert_eq!(result.action, "published");
        assert_eq!(result.run_id, run.run_id);
        assert_eq!(f.cached_observations(), scans);
        assert_eq!(f.sent.lock().unwrap().len(), 1);
        assert_eq!(f.app.store.calls(&run.run_id).unwrap(), calls);
        assert_eq!(snapshots.read_bytes(&input_ref).unwrap(), frozen_bytes);
        assert_eq!(fs::read(&settings_path).unwrap(), yaml);
        let stored = f.app.store.get(&result.run_id).unwrap();
        assert_eq!(stored.config_fingerprint, run.config_fingerprint);
        let report: PersonalReport =
            serde_json::from_str(stored.report_json.as_deref().unwrap()).unwrap();
        assert_eq!(report.schema_version, 6);
        assert!(result.markdown.contains("今日概览"));
        assert!(!report.items.is_empty());
        assert_eq!(
            report.evidence.values().next().unwrap()[0].quote,
            input.selection.materials[0].text
        );
        let attachment = report.evidence_attachment.unwrap();
        for sentence in original.split('。').filter(|part| !part.is_empty()) {
            assert!(attachment.markdown.contains(sentence));
        }
    }
}

#[test]
fn personal_scan_plan_has_one_daily_trigger_and_no_background_recovery() {
    let f = Fixture::new();
    fs::write(
        f.app.config.program_dir.join("ai-daily-report.exe"),
        "fixture",
    )
    .unwrap();
    let plan = operations::task_plan(&f.app.config, "S-1-5-21-123", Utc::now()).unwrap();
    let xml = plan["xml"].as_str().unwrap();
    assert_eq!(xml.matches("<CalendarTrigger").count(), 1);
    assert!(xml.contains("T09:00:00+08:00"));
    assert!(!xml.contains("LogonTrigger"));
    assert!(!xml.contains("RestartOnFailure"));
    assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
    for kind in ["collect", "parse"] {
        assert_eq!(
            operations::task_plan_kind(&f.app.config, "S-1-5-21-123", Utc::now(), kind)
                .unwrap_err()
                .code,
            "TASK_KIND_INVALID"
        );
    }
}

#[test]
fn material_checkpoint_resumes_before_selection_and_replays_the_committed_request() {
    use ai_daily_report::{
        run::personal::PersonalInput,
        snapshots::{digest, fingerprint},
        store::{RunRecord, Store},
        types::RunPhase,
    };
    use std::path::PathBuf;

    fn saved_input(root: &Path, run: &RunRecord) -> (Vec<u8>, PersonalInput) {
        let bytes = fs::read(root.join(run.snapshot_path.as_ref().unwrap())).unwrap();
        assert_eq!(digest(&bytes), *run.snapshot_sha256.as_ref().unwrap());
        let input: PersonalInput = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            Some(&input.input_fingerprint),
            run.input_fingerprint.as_ref()
        );
        (bytes, input)
    }

    struct CheckBeforeSend {
        store: Store,
        root: PathBuf,
        id: String,
        sent: Arc<Mutex<Vec<Value>>>,
    }
    impl ModelTransport for CheckBeforeSend {
        fn send(&self, bytes: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
            let run = self.store.get(&self.id).unwrap();
            assert_eq!(run.phase, RunPhase::Generating);
            let (_, input) = saved_input(&self.root, &run);
            let plan = input.plan.unwrap();
            assert_eq!(plan.bytes().unwrap(), bytes);
            let calls = self.store.calls(&self.id).unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0]["status"], "reserved");
            assert_eq!(
                calls[0]["request_fingerprint"],
                fingerprint(&json!({
                    "generation_id":run.generation_id, "config_fingerprint":run.config_fingerprint,
                    "plan_sha256":plan.identity().unwrap(), "request_sha256":digest(bytes),
                }))
                .unwrap()
            );
            assert_eq!(
                fs::read(self.root.join(calls[0]["request_path"].as_str().unwrap())).unwrap(),
                bytes
            );
            let payload: Value =
                serde_json::from_str(plan.request["messages"][1]["content"].as_str().unwrap())
                    .unwrap();
            let file = payload["sources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["path"] == "work.md")
                .unwrap();
            let output = json!({"schema_version":5,"items":[{
                "title":"金额待审批","text":"金额 120 元，条件满足后生效。","kind":"attention",
                "statement_kind":"observed","source_ids":[file["source_id"]]
            }]});
            self.sent.lock().unwrap().push(payload);
            Ok(HttpResponse { status:200, retry_after:None, body:serde_json::to_vec(
                &json!({"choices":[{"finish_reason":"stop","message":{"content":output.to_string()}}]})
            ).unwrap() })
        }
    }

    let f = Fixture::new();
    let original = "金额 120 元，条件满足后生效。";
    f.file("work.md", original, utc_window(f.day).unwrap().0);
    let mut request = f.request();
    request.input = "原始用户附注。".into();
    let id = request.context.run_id.clone();
    let root = f.app.config.paths.data_dir.join("daily_cache/snapshots");
    // Stop the real report at its first durable prepared state, before its planner can run.
    let conn = f.app.store.connection().unwrap();
    conn.execute_batch(
        "CREATE TRIGGER stop_before_selection AFTER UPDATE OF phase ON report_runs
         WHEN OLD.phase='scanning' AND NEW.phase='prepared' BEGIN
           UPDATE report_runs SET status='failed' WHERE run_id=NEW.run_id;
         END;",
    )
    .unwrap();
    assert_eq!(
        run_daily(request, &f.app).unwrap_err().code,
        "RUN_PHASE_INVALID"
    );
    conn.execute_batch("DROP TRIGGER stop_before_selection")
        .unwrap();
    let materials = f
        .app
        .store
        .pending(Some(&f.day.to_string()))
        .unwrap()
        .remove(0);
    assert_eq!(materials.run_id, id);
    assert_eq!(materials.phase, RunPhase::Prepared);
    assert_eq!(f.cached_observations(), 1);
    let path = root.join(materials.snapshot_path.as_ref().unwrap());
    let (original_bytes, observation) = saved_input(&root, &materials);
    assert!(observation.plan.is_none());
    assert_eq!(fs::read_dir(root.join(&id)).unwrap().count(), 1);
    assert!(f.app.store.calls(&id).unwrap().is_empty());
    assert_eq!(observation.selection.materials.len(), 2);
    let file = observation
        .selection
        .materials
        .iter()
        .find(|m| m.source.source_kind == "file")
        .unwrap();
    assert!(file.text.contains(original));
    assert!(!file.evidence[0].ranges.is_empty());
    let sources: i64 = conn
        .query_row(
            "SELECT count(*) FROM report_run_sources WHERE run_id=?",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sources as usize, observation.selection.materials.len());
    let health = health::check(&f.app.config, f.day, Utc::now()).unwrap();
    assert!(health
        .checks
        .iter()
        .any(|c| c.code == "RUN_ATTENTION" && c.evidence["pending"] == 1));

    f.file("work.md", "改写后的内容。", utc_window(f.day).unwrap().0);
    install_pdf_worker(f._dir.path());
    let Fixture {
        _dir,
        app,
        day,
        sent,
    } = f;
    let config = AppConfig::load_with_env(
        _dir.path(),
        None,
        &BTreeMap::from([("DEEPSEEK_API_KEY".into(), "synthetic-key".into())]),
    )
    .unwrap();
    assert_eq!(
        config.pdf_worker_path,
        config.program_dir.join("ai-daily-pdf-worker.exe")
    );
    drop(app);
    let mut f = Fixture {
        app: App::open(config, Utc::now()).unwrap(),
        _dir,
        day,
        sent,
    };
    f.app.store.resume(&id, Utc::now()).unwrap();
    let health = health::check(&f.app.config, f.day, Utc::now()).unwrap();
    assert!(health
        .checks
        .iter()
        .any(|c| c.code == "REPORT_IN_PROGRESS" && c.level == "ok"));
    assert!(health.checks.iter().any(|c| c.code == "RUN_ATTENTION"
        && c.evidence["active"] == 1
        && c.evidence["pending"] == 0));
    let historical = health
        .checks
        .iter()
        .find(|c| c.code == "HISTORICAL_REPORT_ATTENTION")
        .unwrap();
    let templates = f._dir.path().join("templates");
    fs::create_dir(&templates).unwrap();
    for (name, body) in operations::TEMPLATES {
        fs::write(templates.join(name), body).unwrap();
    }
    let doctor = operations::doctor(&f.app, false, Utc::now()).unwrap();
    assert_eq!(doctor["status"], "ok");
    assert_eq!(doctor["model"]["calls"], 0);
    fs::rename(
        &f.app.config.paths.work_dir,
        f._dir.path().join("offline-work"),
    )
    .unwrap();
    let context_limit = f.app.config.llm.context_window_tokens;
    f.app.config.llm.context_window_tokens = Some(64_000);
    assert_eq!(
        run_daily(f.request(), &f.app).unwrap_err().code,
        "RECOVERY_CONFIG_CHANGED"
    );
    assert_eq!(
        saved_input(&root, &f.app.store.get(&id).unwrap()).0,
        original_bytes
    );
    f.app.config.llm.context_window_tokens = context_limit;
    let quota = &f.app.processing.as_ref().unwrap().quota;
    let reservation = quota.reserve(quota.available_bytes().unwrap()).unwrap();
    assert_eq!(
        run_daily(f.request(), &f.app).unwrap_err().code,
        "PROCESSING_CACHE_FULL"
    );
    assert_eq!(
        saved_input(&root, &f.app.store.get(&id).unwrap()).0,
        original_bytes
    );
    assert!(f.app.store.calls(&id).unwrap().is_empty());
    drop(reservation);

    // Interrupt after the answer is saved, while the final input and call are still recoverable.
    conn.execute_batch(
        "CREATE TRIGGER stop_before_publish BEFORE UPDATE OF phase ON report_runs
         WHEN NEW.phase='db_committed' BEGIN SELECT RAISE(ABORT,'test interruption'); END;",
    )
    .unwrap();
    f.app.transport = Box::new(CheckBeforeSend {
        store: f.app.store.clone(),
        root: root.clone(),
        id: id.clone(),
        sent: f.sent.clone(),
    });
    assert_eq!(
        run_daily(f.request(), &f.app).unwrap_err().code,
        "REPORT_DATABASE_FAILED"
    );
    conn.execute_batch("DROP TRIGGER stop_before_publish")
        .unwrap();
    assert_eq!(f.cached_observations(), 1);
    assert!(!path.exists());
    let planned = f.app.store.get(&id).unwrap();
    let (planned_bytes, input) = saved_input(&root, &planned);
    assert_eq!(
        json!(input.selection.materials),
        json!(observation.selection.materials)
    );
    assert_eq!(planned.generation_id, materials.generation_id);
    assert_eq!(planned.config_fingerprint, materials.config_fingerprint);
    let sent = f.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let payload = sent[0]["sources"].to_string();
    assert!(payload.contains(original) && payload.contains("原始用户附注"));
    assert!(!payload.contains("改写后的内容"));
    drop(sent);
    let calls = f.app.store.calls(&id).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["status"], "succeeded");
    let response = root.join(calls[0]["response_path"].as_str().unwrap());
    let response_bytes = fs::read(&response).unwrap();
    f.app.transport = Box::new(Interrupted);
    let (first, second) = (f.request(), f.request());
    std::thread::scope(|scope| {
        let other = scope.spawn(|| run_daily(second, &f.app).unwrap());
        let one = run_daily(first, &f.app).unwrap();
        let two = other.join().unwrap();
        assert_eq!((&one.run_id, &two.run_id), (&id, &id));
        let mut actions = [one.action, two.action];
        actions.sort();
        assert_eq!(actions, ["existing", "published"]);
    });
    assert_eq!(f.cached_observations(), 1);
    assert_eq!(f.app.store.calls(&id).unwrap(), calls);
    assert_eq!(
        saved_input(&root, &f.app.store.get(&id).unwrap()).0,
        planned_bytes
    );
    assert_eq!(fs::read(response).unwrap(), response_bytes);
    assert_eq!(historical.level, "ok", "{}", historical.evidence);
}

#[test]
fn failed_plan_swap_keeps_the_material_reference_and_both_files_expire() {
    use ai_daily_report::{
        processing_cache::{ProcessingSession, CONTROL_BYTES, MAX_BYTES, RETENTION_DAYS},
        snapshots::digest,
    };
    let f = Fixture::new();
    f.file(
        "original.md",
        "待核实的金额为 120 元。",
        utc_window(f.day).unwrap().0,
    );
    let request = f.request();
    let id = request.context.run_id.clone();
    let conn = f.app.store.connection().unwrap();
    conn.execute_batch(
        "CREATE TRIGGER stop_plan_swap BEFORE UPDATE OF snapshot_path ON report_runs
         WHEN OLD.phase='prepared' BEGIN SELECT RAISE(ABORT,'test interruption'); END;",
    )
    .unwrap();
    assert_eq!(
        run_daily(request, &f.app).unwrap_err().code,
        "REPORT_DATABASE_FAILED"
    );
    conn.execute_batch("DROP TRIGGER stop_plan_swap").unwrap();
    let run = f.app.store.get(&id).unwrap();
    let cache = &f.app.processing.as_ref().unwrap().quota.root;
    let competing = ProcessingSession::at(
        cache,
        Utc::now(),
        std::time::Instant::now() + Duration::from_secs(3),
        MAX_BYTES,
        CONTROL_BYTES,
        Duration::ZERO,
    );
    assert!(matches!(competing, Err(e) if e.code == "REPORT_ALREADY_RUNNING"));
    let root = cache.join("snapshots");
    let files: Vec<_> = fs::read_dir(root.join(&id))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2);
    let ledger: Value =
        serde_json::from_slice(&fs::read(cache.join(".cache-ledger.json")).unwrap()).unwrap();
    for path in &files {
        let key = path
            .strip_prefix(cache)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(ledger[&key]["pending"], false);
    }
    let path = run.snapshot_path.as_ref().unwrap();
    assert!(path.starts_with(&format!("{id}/personal-materials-")));
    let material = fs::read(root.join(path)).unwrap();
    assert_eq!(Some(digest(&material)), run.snapshot_sha256);
    assert!(serde_json::from_slice::<Value>(&material).unwrap()["plan"].is_null());
    assert!(f.app.store.calls(&id).unwrap().is_empty());
    assert_eq!(f.cached_observations(), 1);
    let config = f.app.config.clone();
    drop(f.app);
    let expired = App::open(
        config,
        Utc::now() + chrono::Duration::days(RETENTION_DAYS + 1),
    )
    .unwrap();
    assert!(files.iter().all(|file| !file.exists()));
    assert!(expired.store.get(&id).unwrap().snapshot_path.is_none());
}
