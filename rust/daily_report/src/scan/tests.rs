use super::*;
use daily_parsers::settings::{ContextAction, ParseStatus, PdfClassificationStatus};
use serde_json::json;
use std::{
    fs::{self, FileTimes},
    io::Write,
};

fn fixture(tmp: &Path, settings: serde_json::Value, pdf: Option<&Path>) -> Scanner {
    let input = tmp.join("input");
    fs::create_dir_all(&input).unwrap();
    let worker_path = pdf.map_or_else(|| tmp.join("ai-daily-pdf-worker.exe"), Path::to_owned);
    Scanner::open(ScannerConfig {
        work_dir: input.to_string_lossy().into(),
        scan_db_path: tmp
            .join("cache")
            .join(cache::CACHE_FILENAME)
            .to_string_lossy()
            .into(),
        scanner_settings: serde_json::from_value(settings).unwrap(),
        adapters: AdapterPaths {
            office_worker_path: tmp.join("ai-daily-report.exe").to_string_lossy().into(),
            pdf_worker_path: worker_path.to_string_lossy().into(),
        },
    })
    .unwrap()
}

fn run(scanner: &Scanner) -> ContextResult {
    let ret = scanner
        .build_context_with_request_id_at(
            &ScanRequest {
                report_mode: ReportMode::Daily,
                start_date: "1970-01-01".into(),
                end_date: "2099-12-31".into(),
                window_start_utc: None,
                window_end_utc: None,
                compression_profile: None,
            },
            "e6ecffdc-4aec-4872-a8bc-f376da969901".into(),
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        )
        .unwrap();
    assert_eq!(ret.exit_code, 0, "{:?}", ret.value.envelope.warnings);
    ret.value.envelope.validate().unwrap();
    ret.value.evidence().validate().unwrap();
    ret.value
}

fn materials(scan: &ContextResult) -> Vec<serde_json::Value> {
    scan.sources()
        .iter()
        .map(|source| serde_json::to_value(source).unwrap())
        .collect()
}

#[test]
fn worker_count_changes_throughput_without_changing_admission_or_ranges() {
    let tmp = tempfile::tempdir().unwrap();
    let files: Vec<_> = (0..9)
        .map(|index| {
            let text: String = (0..220)
                .map(|line| {
                    format!(
            "Material {index} row {line:03}: amount 1200 CNY, approvals and units apply. {}\n",
            "All project owners must verify their totals. ".repeat(2)
        )
                })
                .collect();
            (format!("file-{index:02}.md"), text)
        })
        .collect();
    let mut first: Option<ContextResult> = None;
    for workers in [1, 2, 4, 8] {
        let root = tmp.path().join(format!("workers-{workers}"));
        let scanner = fixture(
            &root,
            json!({
                "max_workers":workers,
                "text_max_chars":60_000,
                "total_max_chars":180_000,
            }),
            None,
        );
        for (name, text) in &files {
            let mut file = File::create_new(root.join("input").join(name)).unwrap();
            file.write_all(text.as_bytes()).unwrap();
            file.set_times(
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            )
            .unwrap();
        }
        let cold = run(&scanner);
        let evidence = cold.evidence();
        assert_eq!(
            evidence.execution_metrics.parse_attempt_count,
            files.len() as u64
        );
        assert!(cold.sources().iter().all(|source| {
            !source.excerpt.is_empty()
                && source.coverage_limits["evidence_audit"]["excerpt_ranges"].is_array()
        }));
        assert!(evidence.execution_metrics.rendered_chars <= 180_000);
        assert!(evidence.execution_metrics.rendered_chars > 160_000);
        let warm = run(&scanner);
        assert_eq!(warm.evidence().execution_metrics.parse_attempt_count, 0);
        assert!(warm.evidence().execution_metrics.snapshot_hit);
        assert_eq!(materials(&cold), materials(&warm));
        if let Some(first) = &first {
            assert_eq!(cold.envelope.file_context, first.envelope.file_context);
            assert_eq!(evidence.decisions, first.evidence().decisions);
            assert_eq!(materials(&cold), materials(first));
        } else {
            first = Some(cold);
        }
    }
}

#[test]
fn verified_snapshot_bypasses_scheduler_until_a_reference_is_corrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let scanner = fixture(tmp.path(), json!({"max_workers": 2}), None);
    fs::write(tmp.path().join("input/first.md"), "# First\nAmount 120.\n").unwrap();
    fs::write(
        tmp.path().join("input/second.md"),
        "# Second\nAmount 240.\n",
    )
    .unwrap();
    let scans = || scheduler::SCAN_CALLS.with(std::cell::Cell::get);
    let before = scans();
    let cold = run(&scanner);
    assert_eq!(scans(), before + 1);
    let warm = run(&scanner);
    assert_eq!(scans(), before + 1);
    assert!(warm.evidence().execution_metrics.snapshot_hit);
    assert_eq!(warm.envelope.file_context, cold.envelope.file_context);
    assert_eq!(materials(&warm), materials(&cold));

    let connection = rusqlite::Connection::open(&scanner.config.scan_db_path).unwrap();
    assert_eq!(connection.execute(
        "UPDATE parse_cache SET record_sha256='corrupt' WHERE file_id=(SELECT min(file_id) FROM parse_cache)",
        [],
    ).unwrap(), 1);
    drop(connection);

    let recovered = run(&scanner);
    assert_eq!(scans(), before + 2);
    let metrics = recovered.evidence().execution_metrics;
    assert!(!metrics.snapshot_hit);
    assert_eq!(metrics.parse_attempt_count, 1);
    assert_eq!(materials(&recovered), materials(&cold));
    assert_eq!(recovered.envelope.file_context, cold.envelope.file_context);
    assert!(run(&scanner).evidence().execution_metrics.snapshot_hit);
    assert_eq!(scans(), before + 2);
}

#[test]
fn same_size_and_mtime_replacement_invalidates_cached_content() {
    let tmp = tempfile::tempdir().unwrap();
    let scanner = fixture(tmp.path(), json!({"max_workers":1}), None);
    let path = tmp.path().join("input/report.md");
    fs::write(&path, "# Old\nAmount is 200.\n").unwrap();
    let time = fs::metadata(&path).unwrap().modified().unwrap();
    let first = run(&scanner);
    fs::rename(&path, tmp.path().join("old.md")).unwrap();
    let mut replaced = File::create_new(path).unwrap();
    replaced.write_all(b"# New\nAmount is 450.\n").unwrap();
    replaced
        .set_times(FileTimes::new().set_modified(time))
        .unwrap();
    drop(replaced);
    let changed = run(&scanner);
    assert_ne!(
        first.sources()[0].content_sha256,
        changed.sources()[0].content_sha256
    );
    assert_eq!(
        changed.sources()[0].modified_at_utc,
        first.sources()[0].modified_at_utc
    );
    assert!(changed.sources()[0].excerpt.contains("450"));
    assert_eq!(changed.evidence().execution_metrics.parse_attempt_count, 1);
}

#[test]
#[ignore = "requires AI_DAILY_TEST_PDF_WORKER and its pinned PDFium DLL"]
fn real_pdf_reservations_refill_in_order_and_reuse_page_observations() {
    let worker = PathBuf::from(std::env::var_os("AI_DAILY_TEST_PDF_WORKER").expect("worker path"));
    let tmp = tempfile::tempdir().unwrap();
    let scanner = fixture(
        tmp.path(),
        json!({
            "max_workers":4,
            "pdf_max_pages":2,
            "max_total_pdf_classification_pages":5,
            "max_pdf_text_extractions":1
        }),
        Some(&worker),
    );
    let work = tmp.path().join("input");
    fs::write(work.join("01-blank.pdf"), pdf(&[None])).unwrap();
    fs::write(
        work.join("02-text.pdf"),
        pdf(&[None, Some("Amount 450 CNY")]),
    )
    .unwrap();
    fs::write(work.join("03-text.pdf"), pdf(&[Some("Amount 900 CNY")])).unwrap();
    let doctor = scanner.doctor().unwrap();
    assert_eq!(doctor.exit_code, 0);
    let cold = run(&scanner);
    assert_eq!(
        cold.sources()[0].reason.as_deref(),
        Some("pdf_window_no_text")
    );
    assert!(cold.sources()[1].excerpt.contains("450 CNY"));
    assert_eq!(cold.sources()[2].parse_status, ParseStatus::NotParsed);
    assert_eq!(
        cold.sources()[2].reason.as_deref(),
        Some("pdf_text_extraction_quota_exhausted")
    );
    let audit = cold.evidence();
    assert_eq!(audit.decisions[2].action, ContextAction::Omit);
    assert_eq!(
        audit.files[1].pdf_classification.0.as_ref().unwrap().status,
        PdfClassificationStatus::TextInParseWindow
    );
    assert_eq!(
        audit
            .execution_metrics
            .refill_audit
            .as_ref()
            .unwrap()
            .rounds
            .len(),
        1
    );
    assert!(cold
        .pdf_runtime
        .iter()
        .all(|audit| audit.requests == 1 && audit.native_document_opens == 1));
    let warm = run(&scanner);
    assert_eq!(materials(&cold), materials(&warm));
    let warm_metrics = warm.evidence().execution_metrics;
    assert_eq!(warm_metrics.parse_attempt_count, 0);
    assert_eq!(warm_metrics.classify_attempt_count, 0);
    assert!(warm_metrics.snapshot_hit);
    assert!(warm
        .pdf_runtime
        .iter()
        .all(|audit| audit.requests == 0 && audit.native_document_opens == 0));

    fs::write(work.join("04-broken.pdf"), b"%PDF-1.7\ninvalid document\n").unwrap();
    let mut changed = scanner.config.clone();
    changed.scanner_settings.pdf_max_pages = Some(1);
    let narrower = Scanner::open(changed).unwrap();
    let cold = run(&narrower);
    let cold_sources = cold.sources();
    assert_eq!(
        cold_sources[1].reason.as_deref(),
        Some("pdf_window_no_text")
    );
    assert!(cold_sources[1].parse_truncated);
    assert!(cold_sources[2].excerpt.contains("900 CNY"));
    assert_eq!(cold_sources[3].reason.as_deref(), Some("pdf_damaged"));
    assert_eq!(cold.envelope.status, EngineStatus::Partial);
    assert!(cold
        .envelope
        .warnings
        .iter()
        .any(|warning| warning.error_code == ErrorCode::ParserFailed));
    assert!(cold
        .pdf_runtime
        .iter()
        .all(|audit| audit.requests == 1 && audit.native_document_opens == 1));
    let wire = serde_json::to_value(&cold).unwrap();
    let audit = &wire["sources"][1]["coverage_limits"]["evidence_audit"];
    assert_eq!(
        audit["pdf_page_audit"],
        wire["evidence"]["files"][1]["pdf_classification"]["page_audit"]
    );
    assert_eq!(
        audit["omitted_ranges"],
        json!([{"unit":"pdf_page", "start":2, "end":3, "label":"unread",
            "column_start":null, "column_end":null}])
    );
    assert_eq!(
        wire["evidence"]["files"][3]["final_diagnostic"]["error_code"],
        "PARSER_FAILED"
    );
    let warm = run(&narrower);
    assert_eq!(warm.envelope.status, EngineStatus::Partial);
    assert!(warm.evidence().execution_metrics.snapshot_hit);
    assert!(warm.pdf_runtime.iter().all(|audit| audit.requests == 0));
}

#[test]
#[ignore = "requires a matching PDF worker with the pinned DLL and OCR model files"]
fn optional_ocr_keeps_native_priority_and_marks_qualified_image_evidence() {
    use daily_parsers::settings::{OCR_BACKEND, OCR_RANGE_LABEL};

    let worker = PathBuf::from(std::env::var_os("AI_DAILY_TEST_PDF_WORKER").expect("worker path"));
    let tmp = tempfile::tempdir().unwrap();
    let scanner = fixture(
        tmp.path(),
        json!({
            "max_workers": 1,
            "pdf_max_pages": 2,
            "pdf_image_ocr": {"enabled": true, "max_files": 2, "max_pages": 2},
        }),
        Some(&worker),
    );
    if !daily_parsers::OCR_AVAILABLE {
        assert_eq!(
            scanner.doctor().unwrap_err().code,
            "SCANNER_OCR_UNAVAILABLE"
        );
        return;
    }
    let work = tmp.path().join("input");
    let images = include_bytes!("../../../pdf_worker/tests/fixtures/ocr-image-pages.pdf");
    fs::write(work.join("01-pictures.pdf"), images).unwrap();
    fs::write(work.join("02-pictures.pdf"), images).unwrap();
    fs::write(
        work.join("03-native.pdf"),
        pdf(&[Some("Native amount 450")]),
    )
    .unwrap();
    fs::write(work.join("04-blank.pdf"), pdf(&[None])).unwrap();
    assert_eq!(scanner.doctor().unwrap().exit_code, 0);
    let cold = run(&scanner);
    let cold_sources = cold.sources();
    let recognized = &cold_sources[0];
    assert_eq!(recognized.parser_backend, OCR_BACKEND);
    assert!(
        recognized.excerpt.contains("1936"),
        "{}",
        recognized.excerpt
    );
    let audit = &recognized.coverage_limits["evidence_audit"];
    assert_eq!(audit["text_unverified"], true);
    let ranges = audit["excerpt_ranges"].as_array().unwrap();
    assert!(!ranges.is_empty());
    assert!(ranges.iter().all(|range| range["label"] == OCR_RANGE_LABEL));
    assert_eq!(
        cold.sources()[1].reason.as_deref(),
        Some("pdf_image_ocr_page_quota_exhausted")
    );
    assert!(cold.sources()[2].excerpt.contains("Native amount 450"));
    assert_eq!(
        cold.sources()[3].reason.as_deref(),
        Some("pdf_window_no_text")
    );
    assert_eq!(
        (
            cold.pdf_runtime[0].requests,
            cold.pdf_runtime[0].native_document_opens
        ),
        (2, 2)
    );
    assert!(cold.pdf_runtime[1..].iter().all(|item| item.requests == 1));
    let warm = run(&scanner);
    assert_eq!(materials(&cold), materials(&warm));
    assert!(warm.evidence().execution_metrics.snapshot_hit);
    assert!(warm.pdf_runtime.iter().all(|item| item.requests == 0));

    let mut changed = scanner.config.clone();
    changed
        .scanner_settings
        .pdf_image_ocr
        .as_mut()
        .unwrap()
        .max_files = 1;
    let capped = run(&Scanner::open(changed.clone()).unwrap());
    assert_eq!(
        capped.sources()[1].reason.as_deref(),
        Some("pdf_image_ocr_file_quota_exhausted")
    );
    changed.scanner_settings.pdf_max_pages = Some(1);
    let narrower = run(&Scanner::open(changed).unwrap());
    assert_eq!(
        narrower.sources()[0].reason.as_deref(),
        Some("pdf_window_no_text")
    );
    assert!(narrower.sources()[0].parse_truncated);
    assert_eq!(narrower.pdf_runtime[0].requests, 1);

    let without_models = tmp.path().join("without-models");
    fs::create_dir(&without_models).unwrap();
    fs::copy(&worker, without_models.join("ai-daily-pdf-worker.exe")).unwrap();
    fs::copy(
        worker.with_file_name("pdfium.dll"),
        without_models.join("pdfium.dll"),
    )
    .unwrap();
    let bare = fixture(
        &without_models,
        json!({
            "pdf_max_pages": 2,
            "pdf_image_ocr": {"enabled": true, "max_files":1, "max_pages": 2},
        }),
        Some(&without_models.join("ai-daily-pdf-worker.exe")),
    );
    fs::write(without_models.join("input/images.pdf"), images).unwrap();
    let failed = run(&bare);
    assert_eq!(
        failed.sources()[0].reason.as_deref(),
        Some("pdf_image_ocr_failed")
    );
    assert_eq!(
        failed.sources()[0].coverage_limits["evidence_audit"]["secondary_tags"][0],
        "PDF_OCR_MODEL_MISSING"
    );
}

fn pdf(pages: &[Option<&str>]) -> Vec<u8> {
    let children: String = (0..pages.len())
        .map(|i| format!("{} 0 R ", 4 + 2 * i))
        .collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Kids [{children}] /Count {} >>",
            pages.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    for (index, page) in pages.iter().enumerate() {
        let stream = page.map_or_else(String::new, |text| {
            format!("BT /F1 12 Tf 30 280 Td ({text}) Tj ET")
        });
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>",
            5 + 2 * index,
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    let mut bytes = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        writeln!(&mut bytes, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
    }
    let xref = bytes.len();
    writeln!(
        &mut bytes,
        "xref\n0 {}\n0000000000 65535 f ",
        offsets.len() + 1
    )
    .unwrap();
    for offset in offsets {
        writeln!(&mut bytes, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        &mut bytes,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
        objects.len() + 1
    )
    .unwrap();
    bytes
}
