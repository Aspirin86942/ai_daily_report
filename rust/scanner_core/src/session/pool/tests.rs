use super::*;
use crate::parsers::{document, office, register_worker};
use ai_daily_scanner_contract::AdapterPaths;

#[test]
#[ignore = "measures the full 15-second OCR queue deadline with a real worker registration"]
fn image_ocr_queue_obeys_the_parent_deadline() {
    let dir = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
    let command = document::worker_command(&AdapterPaths {
        pdf_worker_path: dir
            .join("ai-daily-pdf-parser.exe")
            .to_string_lossy()
            .into_owned(),
        office_worker_path: String::new(),
    });
    let timeout = Duration::from_secs(30);
    let worker = register_worker(&command, timeout).unwrap();
    let pool = WorkerPool::new(
        command,
        worker.identity.clone(),
        worker,
        SessionParams::with_default_concurrency(1),
        WorkerRssTracker::default(),
    );
    let _held_slot = pool
        .slots
        .checkout(&pool.expected, Instant::now(), timeout)
        .unwrap();
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pdf_parser/tests/fixtures/ocr-image-pages.pdf");
    let file_path = file.to_str().unwrap();
    let request = ParseOperation {
        request_id: crate::scanner::next_request_id(),
        payload: ai_daily_worker_contract::ParseRequest {
            file_path: file_path.to_owned(),
            file_type: ".pdf".into(),
            backend: ParserBackend::RustPdfiumV3,
            remaining_timeout_ms: 30_000,
            max_file_size_bytes: 10_000_000,
            parser_limits: ai_daily_worker_contract::ParserLimits::PdfImageOcr {
                max_pages: 1,
                excerpt_max_chars: 512,
                selected_pages: vec![1],
            },
            expected_source_version: crate::parsers::current_source(file_path).unwrap().0,
        },
    };
    let started = Instant::now();
    let failure = pool.parse_worker(&request, timeout).unwrap_err();
    let elapsed = started.elapsed();
    assert!(failure.failure.is_timeout());
    assert_eq!(failure.attempt_count, 0);
    assert!(
        (Duration::from_secs(14)..Duration::from_secs(19)).contains(&elapsed),
        "{elapsed:?}"
    );
    println!("OCR queue timed out after {elapsed:?}");
}

#[test]
fn occupied_slots_respect_deadline_and_timeout_replaces_only_one_process() {
    let directory = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
    let paths = AdapterPaths {
        office_worker_path: directory
            .join("ai-daily-office-parser.exe")
            .to_string_lossy()
            .into_owned(),
        pdf_worker_path: directory
            .join("ai-daily-pdf-parser.exe")
            .to_string_lossy()
            .into_owned(),
    };
    let timeout = Duration::from_secs(10);
    let slots = SessionSlots::new(64);
    let pool = |command| {
        let worker = register_worker(&command, timeout).unwrap();
        WorkerPool::with_slots(
            command,
            worker.identity.clone(),
            worker,
            SessionParams::default(),
            WorkerRssTracker::default(),
            slots.clone(),
        )
    };
    let office = pool(office::worker_command(&paths));
    let pdf = pool(document::worker_command(&paths));
    let pid =
        |session: &mut WorkerSession, _: Duration| Ok(session.child.as_ref().unwrap().child.id());
    office.execute_operation("fixture", timeout, pid).unwrap();
    let pdf_pid = pdf
        .execute_operation("fixture", timeout, pid)
        .unwrap()
        .value;
    let pdf_busy = slots
        .checkout(&pdf.expected, Instant::now(), timeout)
        .unwrap();
    let office_busy = slots
        .checkout(&office.expected, Instant::now(), timeout)
        .unwrap();
    let short = Duration::from_millis(30);
    let started = Instant::now();
    let waiting = office.execute_operation("fixture", short, pid).unwrap_err();
    assert!(waiting.failure.is_timeout());
    assert_eq!(waiting.attempt_count, 0);
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(office_busy);

    // No request was sent. This live Office child waits for input until its
    // response deadline expires. The checked-out PDF child stays usable.
    let expired = office
        .execute_operation("fixture", short, |session, remaining| {
            session.child.as_ref().unwrap().read_line(remaining)
        })
        .unwrap_err();
    assert!(expired.failure.is_timeout());
    assert_eq!(expired.attempt_count, 1);
    office.execute_operation("fixture", timeout, pid).unwrap();
    assert_eq!(office.stats().session_restart_count, 1);
    drop(pdf_busy);
    assert_eq!(
        pdf.execute_operation("fixture", timeout, pid)
            .unwrap()
            .value,
        pdf_pid
    );
    assert_eq!(pdf.stats().session_restart_count, 0);
    assert_eq!(lock_unpoison(&slots.idle).len(), 2);
}
