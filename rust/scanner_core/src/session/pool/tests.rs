use super::*;
use crate::parsers::{document, office, register_worker};
use ai_daily_scanner_contract::AdapterPaths;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const WORKER_TIMEOUT: Duration = Duration::from_secs(10);

fn worker_directory() -> std::path::PathBuf {
    std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"))
}

fn shared_pools(concurrency: usize) -> (Arc<SessionSlots>, Arc<WorkerPool>, Arc<WorkerPool>) {
    let directory = worker_directory();
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
    let slots = SessionSlots::new(concurrency);
    let pool = |command| {
        let worker = register_worker(&command, WORKER_TIMEOUT).unwrap();
        WorkerPool::with_slots(
            command,
            worker.identity.clone(),
            worker,
            SessionParams::with_default_concurrency(concurrency as u64),
            WorkerRssTracker::default(),
            slots.clone(),
        )
    };
    let office = pool(office::worker_command(&paths));
    let pdf = pool(document::worker_command(&paths));
    (slots, office, pdf)
}

fn pid(session: &mut WorkerSession, _: Duration) -> Result<u32, SessionError> {
    Ok(session.child.as_ref().unwrap().child.id())
}

fn process_handle(session: &WorkerSession) -> OwnedHandle {
    session
        .child
        .as_ref()
        .unwrap()
        .child
        .as_handle()
        .try_clone_to_owned()
        .unwrap()
}

fn exited(handle: &OwnedHandle) -> bool {
    unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) == WAIT_OBJECT_0 }
}

fn await_waiters(slots: &SessionSlots, count: usize) {
    let started = Instant::now();
    while lock_unpoison(&slots.state).waiters.len() < count {
        assert!(
            started.elapsed() < WORKER_TIMEOUT,
            "worker request did not queue"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "measures the full 15-second OCR queue deadline with a real worker registration"]
fn image_ocr_queue_obeys_the_parent_deadline() {
    let dir = worker_directory();
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
    let timeout = WORKER_TIMEOUT;
    let (slots, office, pdf) = shared_pools(64);
    let office_process = office
        .execute_operation("fixture", timeout, |session, _| Ok(process_handle(session)))
        .unwrap()
        .value;
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
    let short = Duration::from_millis(100);
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
    assert!(exited(&office_process));
    office.execute_operation("fixture", timeout, pid).unwrap();
    assert_eq!(office.stats().session_restart_count, 1);
    assert_eq!(office.stats().session_restart_reasons.timeout, 1);
    drop(pdf_busy);
    assert_eq!(
        pdf.execute_operation("fixture", timeout, pid)
            .unwrap()
            .value,
        pdf_pid
    );
    assert_eq!(pdf.stats().session_restart_count, 0);
    assert_eq!(lock_unpoison(&slots.state).idle.len(), 2);
}

#[test]
fn shared_slots_allow_two_office_workers_and_prioritize_a_waiting_pdf() {
    let (slots, office, pdf) = shared_pools(2);
    let (first_pid, first_process) = office
        .execute_operation("fixture", WORKER_TIMEOUT, |session, timeout| {
            Ok((pid(session, timeout)?, process_handle(session)))
        })
        .unwrap()
        .value;
    let first_busy = slots
        .checkout(&office.expected, Instant::now(), WORKER_TIMEOUT)
        .unwrap();
    let (second_pid, second_process) = office
        .execute_operation("fixture", WORKER_TIMEOUT, |session, timeout| {
            Ok((pid(session, timeout)?, process_handle(session)))
        })
        .unwrap()
        .value;
    let second_busy = slots
        .checkout(&office.expected, Instant::now(), WORKER_TIMEOUT)
        .unwrap();
    assert_ne!(first_pid, second_pid);
    assert!(!exited(&first_process) && !exited(&second_process));
    assert_eq!(office.stats().session_restart_count, 0);

    let pdf_pid = thread::scope(|scope| {
        let waiting_office = scope.spawn(|| {
            office
                .execute_operation("fixture", WORKER_TIMEOUT, pid)
                .unwrap()
                .value
        });
        await_waiters(&slots, 1);
        let (started, saw_pdf) = mpsc::channel();
        let (finish, release) = mpsc::channel();
        let waiting_pdf = scope.spawn(|| {
            pdf.execute_operation("fixture", WORKER_TIMEOUT, move |session, timeout| {
                let pdf_pid = pid(session, timeout)?;
                started.send(pdf_pid).unwrap();
                release.recv_timeout(WORKER_TIMEOUT).unwrap();
                Ok(pdf_pid)
            })
            .unwrap()
            .value
        });
        await_waiters(&slots, 2);

        drop(second_busy);
        let pdf_pid = saw_pdf.recv_timeout(WORKER_TIMEOUT).unwrap();
        assert!(exited(&second_process));
        assert!(!exited(&first_process));
        assert!(!waiting_office.is_finished());

        drop(first_busy);
        assert_eq!(waiting_office.join().unwrap(), first_pid);
        finish.send(()).unwrap();
        assert_eq!(waiting_pdf.join().unwrap(), pdf_pid);
        pdf_pid
    });

    for _ in 0..4 {
        assert_eq!(
            office
                .execute_operation("fixture", WORKER_TIMEOUT, pid)
                .unwrap()
                .value,
            first_pid
        );
        assert_eq!(
            pdf.execute_operation("fixture", WORKER_TIMEOUT, pid)
                .unwrap()
                .value,
            pdf_pid
        );
    }
    let stats = SessionPoolStats::combine(office.stats(), pdf.stats());
    assert_eq!(stats.session_restart_count, 1);
    assert_eq!(
        stats.session_restart_reasons,
        SessionRestartCounts {
            lane_switch: 1,
            ..SessionRestartCounts::default()
        }
    );
}

#[test]
fn one_shared_slot_reaps_the_old_lane_before_starting_the_next() {
    let (slots, office, pdf) = shared_pools(1);
    let office_process = office
        .execute_operation("fixture", WORKER_TIMEOUT, |session, _| {
            Ok(process_handle(session))
        })
        .unwrap()
        .value;
    let busy = slots
        .checkout(&office.expected, Instant::now(), WORKER_TIMEOUT)
        .unwrap();

    let waiting = pdf
        .execute_operation("fixture", Duration::from_millis(50), pid)
        .unwrap_err();
    assert!(waiting.failure.is_timeout());
    assert_eq!(waiting.attempt_count, 0);
    assert!(!exited(&office_process));

    drop(busy);
    pdf.execute_operation("fixture", WORKER_TIMEOUT, |session, timeout| {
        assert!(exited(&office_process));
        pid(session, timeout)
    })
    .unwrap();
    assert_eq!(pdf.stats().session_restart_reasons.lane_switch, 1);
    assert_eq!(lock_unpoison(&slots.state).idle.len(), 1);
}

#[test]
fn eof_retries_once_and_keeps_retirement_reason_until_replaced() {
    let (slots, office, _) = shared_pools(1);
    let mut exited_generations = Vec::new();
    let failure = office
        .execute_operation("fixture", WORKER_TIMEOUT, |session, timeout| {
            assert!(exited_generations.iter().all(exited));
            exited_generations.push(process_handle(session));
            let child = session.child.as_mut().unwrap();
            child.child.kill().unwrap();
            child.read_line(timeout)
        })
        .unwrap_err();
    assert_eq!(failure.attempt_count, 2);
    assert_eq!(exited_generations.len(), 2);
    assert!(exited_generations.iter().all(exited));
    assert_eq!(office.stats().session_restart_count, 1);
    assert_eq!(office.stats().session_restart_reasons.transport, 1);
    assert_eq!(lock_unpoison(&slots.state).idle.len(), 1);

    // Failed hello/spawn must neither claim an attempt nor consume the reason.
    let missing = tempfile::tempdir().unwrap();
    let mut command = office.command.clone();
    command.program = missing.path().join("missing-worker.exe");
    let unavailable = WorkerPool::with_slots(
        command,
        office.expected.clone(),
        office.worker.clone(),
        office.params,
        WorkerRssTracker::default(),
        slots.clone(),
    );
    let failed_start = unavailable
        .execute_operation("fixture", WORKER_TIMEOUT, pid)
        .unwrap_err();
    assert_eq!(failed_start.attempt_count, 0);
    assert_eq!(unavailable.stats().session_restart_count, 0);

    office
        .execute_operation("fixture", WORKER_TIMEOUT, pid)
        .unwrap();
    assert_eq!(office.stats().session_restart_count, 2);
    assert_eq!(office.stats().session_restart_reasons.transport, 2);
}

#[test]
fn parent_recovers_from_an_oversized_encode_and_exchanges_typed_v4_requests() {
    let (_, _, pdf) = shared_pools(1);
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pdf_parser/tests/fixtures/settlement-pages.pdf")
        .canonicalize()
        .unwrap();
    let source = crate::parsers::current_source(file.to_str().unwrap())
        .unwrap()
        .0;
    let request = build_classify_request(crate::scanner::next_request_id(), &file, &source, 3);
    pdf.execute_operation("fixture", WORKER_TIMEOUT, |session, timeout| {
        let child = session.child.as_mut().unwrap();
        let oversized = "x".repeat(SESSION_REQUEST_FRAME_LIMIT);
        assert_eq!(
            child.write_frame(&oversized, Instant::now(), timeout),
            Err(SessionError::FrameTooLarge)
        );
        assert_eq!(
            child.write_frame(&oversized, Instant::now(), Duration::ZERO),
            Err(SessionError::Timeout)
        );
        let first = session_classify(session, &request, timeout)?;
        let second = session_classify(session, &request, timeout)?;
        assert_eq!(second, first);
        assert_eq!(session.requests_served(), 2);
        Ok(())
    })
    .unwrap();
    assert_eq!(pdf.stats().session_restart_count, 0);
}
