use super::*;
use crate::scan::sources::ScannedFile;
use daily_parsers::{
    contract::{ClassifyStatus, WorkerLane},
    settings::ParseStatus,
};
use serde_json::json;
use std::{
    fs::{self, FileTimes},
    io::Write,
};

fn fixture_config(tmp: &Path, settings: serde_json::Value, pdf: Option<&Path>) -> ScannerConfig {
    let input = tmp.join("input");
    fs::create_dir_all(&input).unwrap();
    ScannerConfig {
        work_dir: input,
        scan_db_path: tmp.join("cache").join(cache::CACHE_FILENAME),
        scanner_settings: serde_json::from_value(settings).unwrap(),
        pdf_worker_path: pdf.map_or_else(|| tmp.join("ai-daily-pdf-worker.exe"), Path::to_owned),
    }
}

fn fixture(tmp: &Path, settings: serde_json::Value, pdf: Option<&Path>) -> Scanner {
    Scanner::open(fixture_config(tmp, settings, pdf)).unwrap()
}

fn request() -> ScanRequest {
    ScanRequest {
        start_date: NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
        end_date: NaiveDate::from_ymd_opt(2099, 12, 31).unwrap(),
        utc_window: None,
    }
}

const OBSERVED_AT: DateTime<Utc> = DateTime::from_timestamp(1_800_000_000, 0).unwrap();

fn run(scanner: &Scanner) -> ScanResult {
    let scan = scanner.build_context_at(&request(), OBSERVED_AT).unwrap();
    assert_scan_result(&scan);
    scan
}

fn run_benchmark(scanner: &Scanner) -> (ScanResult, Vec<ScanBenchFile>) {
    let (scan, observed) = scanner
        .build_context_for_benchmark(&request(), OBSERVED_AT)
        .unwrap();
    assert_scan_result(&scan);
    assert_eq!(scan.files.len(), observed.len());
    (scan, observed)
}

fn assert_scan_result(scan: &ScanResult) {
    assert_ne!(scan.status, EngineStatus::Error, "{:?}", scan.warnings);
    assert_eq!(
        scan.stats.rendered_chars,
        scan.files
            .iter()
            .map(|file| file.excerpt.chars as u64)
            .sum::<u64>()
    );
}

fn pdf_runtime(observed: &[ScanBenchFile]) -> impl Iterator<Item = &PdfRuntime> {
    observed.iter().filter_map(|file| file.pdf_runtime.as_ref())
}

fn by_path(scan: &ScanResult) -> Vec<&ScannedFile> {
    let mut files: Vec<_> = scan.files.iter().collect();
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    files
}

fn assert_same_admission(left: &ScanResult, right: &ScanResult) {
    assert_eq!(left.files.len(), right.files.len());
    assert_eq!(
        left.source_profile.observed_at_utc,
        right.source_profile.observed_at_utc
    );
    for (a, b) in left.files.iter().zip(&right.files) {
        assert_eq!(a.source_id, b.source_id);
        assert_eq!(a.relative_path, b.relative_path);
        assert_eq!(a.extension, b.extension);
        assert_eq!(a.size_bytes, b.size_bytes);
        assert_eq!(a.modified_at_utc, b.modified_at_utc);
        assert_eq!(a.raw_sha256, b.raw_sha256);
        assert_eq!(a.excerpt_sha256, b.excerpt_sha256);
        assert_eq!(a.excerpt.text, b.excerpt.text);
        assert_eq!(a.excerpt.chars, b.excerpt.chars);
        assert_eq!(a.excerpt.truncated, b.excerpt.truncated);
        assert_eq!(a.parser_backend, b.parser_backend);
        assert_eq!(a.worker_lane, b.worker_lane);
        assert_eq!(a.parse_status, b.parse_status);
        assert_eq!(a.parse_truncated, b.parse_truncated);
        assert_eq!(a.reason, b.reason);
        assert_eq!(a.priority, b.priority);
        assert_eq!(a.ocr_tag, b.ocr_tag);
        assert_eq!(a.excerpt.audit, b.excerpt.audit);
        assert_eq!(a.excerpt.units, b.excerpt.units);
        assert_eq!(a.excerpt.read_ranges, b.excerpt.read_ranges);
        assert_eq!(a.excerpt.ranges, b.excerpt.ranges);
        assert_eq!(a.excerpt.omitted_ranges, b.excerpt.omitted_ranges);
        assert!(a.benchmark_blocks().eq(b.benchmark_blocks()));
        assert_eq!(
            a.coverage_limits(&left.source_profile),
            b.coverage_limits(&right.source_profile),
            "{}",
            a.relative_path
        );
    }
}

#[test]
fn scanner_validates_configuration_and_request_before_scanning() {
    let tmp = tempfile::tempdir().unwrap();
    let config = fixture_config(tmp.path(), json!({}), None);
    let mut invalid_settings = config.clone();
    invalid_settings.scanner_settings.max_workers = Some(0);
    let mut relative_worker = config.clone();
    relative_worker.pdf_worker_path = PathBuf::from("relative-worker.exe");
    let mut wrong_database = config.clone();
    wrong_database.scan_db_path = tmp.path().join("cache/old.sqlite3");
    for invalid in [invalid_settings, relative_worker, wrong_database] {
        assert_eq!(
            Scanner::open(invalid).err().unwrap().code,
            "SCANNER_INVALID_CONFIGURATION"
        );
    }

    let scanner = Scanner::open(config).unwrap();
    let start_date = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
    let end_date = start_date.succ_opt().unwrap();
    let start = start_date.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let end = end_date.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let valid = ScanRequest {
        start_date,
        end_date,
        utc_window: Some((start, end)),
    };
    for request in [
        ScanRequest {
            start_date: end_date,
            end_date: start_date,
            utc_window: None,
        },
        ScanRequest {
            utc_window: Some((end, start)),
            ..valid
        },
        ScanRequest {
            utc_window: Some((start, start)),
            ..valid
        },
    ] {
        assert_eq!(
            scanner.build_context(&request).err().unwrap().code,
            "SCANNER_INVALID_CONFIGURATION",
            "{request:?}"
        );
    }
    assert!(!scanner.cache_dir.join(cache::CACHE_FILENAME).exists());

    for (name, modified) in [("first.md", start), ("outside.md", end)] {
        let mut file = File::create_new(tmp.path().join("input").join(name)).unwrap();
        file.write_all(b"# Amount\n42 CNY\n").unwrap();
        file.set_times(FileTimes::new().set_modified(modified.into()))
            .unwrap();
    }
    let scan = scanner.build_context(&valid).unwrap();
    assert_eq!(scan.status, EngineStatus::Ok);
    assert_eq!(scan.files.len(), 1);
    assert_eq!(scan.files[0].relative_path, "first.md");
    assert!(scan.files[0].excerpt.text.contains("42 CNY"));
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
    let mut first: Option<ScanResult> = None;
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
        assert_eq!(cold.stats.parse_attempt_count, files.len() as u64);
        assert_eq!(cold.stats.parse_cache_lookup_count, files.len() as u64);
        assert!(!cold.stats.snapshot_hit);
        assert!(cold
            .files
            .iter()
            .all(|file| { !file.excerpt.text.is_empty() && file.excerpt.ranges.is_some() }));
        assert!(cold.stats.rendered_chars <= 180_000);
        assert!(cold.stats.rendered_chars > 160_000);
        let warm = run(&scanner);
        assert_eq!(warm.stats.parse_attempt_count, 0);
        assert!(warm.stats.snapshot_hit);
        assert_same_admission(&cold, &warm);
        if let Some(first) = &first {
            assert_same_admission(&cold, first);
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
    assert_eq!(cold.stats.parse_attempt_count, 2);
    assert_eq!(cold.stats.parse_cache_lookup_count, 2);
    assert_eq!(cold.stats.parse_cache_all_hit, Some(false));

    let connection =
        rusqlite::Connection::open(scanner.cache_dir.join(cache::CACHE_FILENAME)).unwrap();
    let (key, manifest, refs): (String, Vec<u8>, String) = connection
        .query_row(
            "SELECT snapshot_key, manifest, refs FROM scan_snapshots",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let bound: serde_json::Value = serde_json::from_str(&refs).unwrap();
    // The v2.0 writer used the first eight bytes of the request UUID's SHA-256 as a u64.
    let request_hash = sha256_hex(b"e6ecffdc-4aec-4872-a8bc-f376da969901");
    let origin = u64::from_str_radix(&request_hash[..16], 16).unwrap();
    let mut legacy = origin.to_le_bytes().to_vec();
    let tuple = json!([
        "scan_snapshot_v4",
        96,
        8,
        80,
        180_000,
        [],
        [
            [
                "first.md",
                "success",
                null,
                bound[0]["reference"]["key"],
                null
            ],
            [
                "second.md",
                "success",
                null,
                bound[1]["reference"]["key"],
                null
            ],
        ]
    ]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&manifest[8..]).unwrap(),
        tuple
    );
    serde_json::to_writer(&mut legacy, &tuple).unwrap();
    let mut signed = b"scan-cache-v4\0snapshot".to_vec();
    for piece in [key.as_bytes(), legacy.as_slice(), refs.as_bytes()] {
        signed.extend_from_slice(&(piece.len() as u64).to_le_bytes());
        signed.extend_from_slice(piece);
    }
    assert_eq!(
        connection
            .execute(
                "UPDATE scan_snapshots SET manifest=?1, record_sha256=?2 WHERE snapshot_key=?3",
                rusqlite::params![legacy, sha256_hex(&signed), key],
            )
            .unwrap(),
        1
    );
    drop(connection);

    let warm = run(&scanner);
    assert_eq!(scans(), before + 1);
    assert!(warm.stats.snapshot_hit);
    assert_eq!(warm.stats.parse_attempt_count, 0);
    assert_eq!(warm.stats.parse_cache_lookup_count, 2);
    assert_eq!(warm.stats.parse_cache_all_hit, Some(true));
    assert_same_admission(&warm, &cold);

    let connection =
        rusqlite::Connection::open(scanner.cache_dir.join(cache::CACHE_FILENAME)).unwrap();
    assert_eq!(connection.execute(
        "UPDATE parse_cache SET record_sha256='corrupt' WHERE file_id=(SELECT min(file_id) FROM parse_cache)",
        [],
    ).unwrap(), 1);
    drop(connection);

    let recovered = run(&scanner);
    assert_eq!(scans(), before + 2);
    assert!(!recovered.stats.snapshot_hit);
    assert_eq!(recovered.stats.parse_attempt_count, 1);
    assert_eq!(recovered.stats.parse_cache_lookup_count, 2);
    assert_same_admission(&recovered, &cold);
    assert!(run(&scanner).stats.snapshot_hit);
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
    assert_ne!(first.files[0].raw_sha256, changed.files[0].raw_sha256);
    assert_eq!(
        changed.files[0].modified_at_utc,
        first.files[0].modified_at_utc
    );
    assert!(changed.files[0].excerpt.text.contains("450"));
    assert_eq!(changed.stats.parse_attempt_count, 1);
}

#[test]
#[ignore = "requires AI_DAILY_TEST_PDF_WORKER and its pinned PDFium DLL"]
fn real_pdf_reservations_refill_in_order_and_reuse_page_observations() {
    let worker = PathBuf::from(std::env::var_os("AI_DAILY_TEST_PDF_WORKER").expect("worker path"));
    let tmp = tempfile::tempdir().unwrap();
    let mut config = fixture_config(
        tmp.path(),
        json!({
            "max_workers":4,
            "pdf_max_pages":2,
            "max_total_pdf_classification_pages":5,
            "max_pdf_text_extractions":1
        }),
        Some(&worker),
    );
    let scanner = Scanner::open(config.clone()).unwrap();
    let work = tmp.path().join("input");
    fs::write(work.join("01-blank.pdf"), pdf(&[None])).unwrap();
    fs::write(
        work.join("02-text.pdf"),
        pdf(&[None, Some("Amount 450 CNY")]),
    )
    .unwrap();
    fs::write(work.join("03-text.pdf"), pdf(&[Some("Amount 900 CNY")])).unwrap();
    let doctor = scanner.doctor().unwrap();
    assert_eq!(doctor.status, EngineStatus::Ok);
    let (cold, observed) = run_benchmark(&scanner);
    let cold_files = by_path(&cold);
    assert_eq!(cold_files[0].reason, Some("pdf_window_no_text"));
    let parsed = cold_files[1];
    assert!(parsed.excerpt.text.contains("450 CNY"));
    assert_eq!(parsed.parse_status, ParseStatus::Success);
    assert_eq!(parsed.worker_lane, WorkerLane::RustPdfProcessV3.as_str());
    assert_eq!(parsed.pdf_charged_pages, Some(2));
    assert_eq!(parsed.pdf_extraction_slot, Some(true));
    assert_eq!(parsed.pdf_error_code, None);
    let pages = parsed.pdf_pages().unwrap();
    assert_eq!(pages.checked_pages, [1, 2]);
    let observations = pages.observations.as_ref().unwrap();
    assert_eq!(
        observations
            .iter()
            .map(|page| (page.page, page.has_text))
            .collect::<Vec<_>>(),
        [(1, false), (2, true)]
    );
    assert_eq!(cold_files[2].parse_status, ParseStatus::NotParsed);
    assert!(cold_files[2].excerpt.text.is_empty());
    assert_eq!(
        cold_files[2].reason,
        Some("pdf_text_extraction_quota_exhausted")
    );
    assert_eq!(cold.stats.refill_rounds.len(), 1);
    assert!(cold.stats.classify_attempt_count > 0);
    let text_runtime = cold
        .files
        .iter()
        .zip(&observed)
        .find(|(file, _)| file.relative_path == "02-text.pdf")
        .unwrap()
        .1
        .pdf_runtime
        .as_ref()
        .unwrap();
    assert_eq!(text_runtime.status, ClassifyStatus::TextInParseWindow);
    assert_eq!(text_runtime.page_count, Some(2));
    assert_eq!(text_runtime.examined_pages, Some(2));
    assert!(text_runtime.native_page_reads >= 2);
    assert!(
        pdf_runtime(&observed).all(|audit| audit.requests == 1 && audit.native_document_opens == 1)
    );
    let (warm, warm_observed) = run_benchmark(&scanner);
    assert_same_admission(&cold, &warm);
    assert_eq!(warm.stats.parse_attempt_count, 0);
    assert_eq!(warm.stats.classify_attempt_count, 0);
    assert!(warm.stats.snapshot_hit);
    assert_eq!(warm.stats.refill_rounds, cold.stats.refill_rounds);
    assert_eq!(warm.peak_pdf_requests_in_flight, 0);
    assert_eq!(
        pdf_runtime(&warm_observed).count(),
        pdf_runtime(&observed).count()
    );
    assert!(pdf_runtime(&warm_observed).all(|audit| audit.requests == 0
        && audit.native_document_opens == 0
        && audit.native_page_reads == 0));

    fs::write(work.join("04-broken.pdf"), b"%PDF-1.7\ninvalid document\n").unwrap();
    config.scanner_settings.pdf_max_pages = Some(1);
    let narrower = Scanner::open(config).unwrap();
    let (cold, observed) = run_benchmark(&narrower);
    let cold_files = by_path(&cold);
    assert_eq!(cold_files[1].reason, Some("pdf_window_no_text"));
    assert!(cold_files[1].parse_truncated);
    assert!(cold_files[2].excerpt.text.contains("900 CNY"));
    assert_eq!(cold_files[3].reason, Some("pdf_damaged"));
    assert_eq!(cold.status, EngineStatus::Partial);
    assert!(cold
        .warnings
        .iter()
        .any(|warning| warning.error_code == ErrorCode::ParserFailed));
    assert!(
        pdf_runtime(&observed).all(|audit| audit.requests == 1 && audit.native_document_opens == 1)
    );
    let limits = cold_files[1].coverage_limits(&cold.source_profile);
    let audit = &limits["evidence_audit"];
    assert_eq!(
        audit["pdf_page_audit"],
        json!(cold_files[1].pdf_pages().unwrap())
    );
    assert_eq!(
        json!(cold_files[1].excerpt.omitted_ranges),
        json!([{"unit":"pdf_page", "start":2, "end":3, "label":"unread",
            "column_start":null, "column_end":null}])
    );
    assert_eq!(cold_files[3].parse_status, ParseStatus::Error);
    assert_eq!(cold_files[3].pdf_error_code, Some(ErrorCode::ParserFailed));
    let (warm, warm_observed) = run_benchmark(&narrower);
    assert_eq!(warm.status, EngineStatus::Partial);
    assert!(warm.stats.snapshot_hit);
    assert_eq!(
        pdf_runtime(&warm_observed).count(),
        pdf_runtime(&observed).count()
    );
    assert!(pdf_runtime(&warm_observed).all(|audit| audit.requests == 0));
}

#[test]
#[ignore = "requires a matching PDF worker with the pinned DLL and OCR model files"]
fn optional_ocr_keeps_native_priority_and_marks_qualified_image_evidence() {
    use daily_parsers::settings::{OCR_BACKEND, OCR_RANGE_LABEL};

    let worker = PathBuf::from(std::env::var_os("AI_DAILY_TEST_PDF_WORKER").expect("worker path"));
    let tmp = tempfile::tempdir().unwrap();
    let mut config = fixture_config(
        tmp.path(),
        json!({
            "max_workers": 1,
            "pdf_max_pages": 2,
            "pdf_image_ocr": {"enabled": true, "max_files": 2, "max_pages": 2},
        }),
        Some(&worker),
    );
    let scanner = Scanner::open(config.clone()).unwrap();
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
    assert_eq!(scanner.doctor().unwrap().status, EngineStatus::Ok);
    let (cold, observed) = run_benchmark(&scanner);
    let cold_files = by_path(&cold);
    let recognized = cold_files[0];
    assert_eq!(recognized.parser_backend, OCR_BACKEND);
    assert!(
        recognized.excerpt.text.contains("1936"),
        "{}",
        recognized.excerpt.text
    );
    let limits = recognized.coverage_limits(&cold.source_profile);
    let audit = &limits["evidence_audit"];
    assert_eq!(audit["text_unverified"], true);
    let ranges = recognized.excerpt.ranges.as_ref().unwrap();
    assert!(!ranges.is_empty());
    assert!(ranges
        .iter()
        .all(|range| range.label.as_deref() == Some(OCR_RANGE_LABEL)));
    assert_eq!(
        cold_files[1].reason,
        Some("pdf_image_ocr_page_quota_exhausted")
    );
    assert!(cold_files[2].excerpt.text.contains("Native amount 450"));
    assert_eq!(cold_files[3].reason, Some("pdf_window_no_text"));
    let runtime: Vec<_> = pdf_runtime(&observed).collect();
    assert_eq!(
        (runtime[0].requests, runtime[0].native_document_opens,),
        (2, 2)
    );
    assert!(runtime[1..].iter().all(|item| item.requests == 1));
    let (warm, warm_observed) = run_benchmark(&scanner);
    assert_same_admission(&cold, &warm);
    assert!(warm.stats.snapshot_hit);
    assert_eq!(pdf_runtime(&warm_observed).count(), runtime.len());
    assert!(pdf_runtime(&warm_observed).all(|item| item.requests == 0));

    config
        .scanner_settings
        .pdf_image_ocr
        .as_mut()
        .unwrap()
        .max_files = 1;
    let capped = run(&Scanner::open(config.clone()).unwrap());
    assert_eq!(
        by_path(&capped)[1].reason,
        Some("pdf_image_ocr_file_quota_exhausted")
    );
    config.scanner_settings.pdf_max_pages = Some(1);
    let (narrower, observed) = run_benchmark(&Scanner::open(config).unwrap());
    assert_eq!(by_path(&narrower)[0].reason, Some("pdf_window_no_text"));
    assert!(by_path(&narrower)[0].parse_truncated);
    assert_eq!(pdf_runtime(&observed).next().unwrap().requests, 1);

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
    let failed_file = &failed.files[0];
    assert_eq!(failed_file.reason, Some("pdf_image_ocr_failed"));
    assert_eq!(
        failed_file.ocr_tag.as_deref(),
        Some("PDF_OCR_MODEL_MISSING")
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
