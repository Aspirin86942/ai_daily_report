//! A lazy PDF client owned by a single scan lane.

use daily_parsers::{
    pdf_wire::{
        open_pinned_pdfium, pinned_pdfium_sha256, read_frame, reader_sha256, write_frame,
        InspectAndExtract, PdfCommand, PdfReply, PdfRequestFrame, PdfResponse, PdfResponseFrame,
        PdfWorkerHello, MAX_REQUEST_FRAME, PDF_PROTOCOL_VERSION,
    },
    settings::NormalizedScannerSettings,
    OCR_AVAILABLE, PARSER_CONTENT_IDENTITY,
};
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum PdfClientError {
    #[error("PDF worker deadline exceeded")]
    Timeout,
    #[error("PDF worker exited during the exchange")]
    WorkerExited,
    #[error("cannot start PDF worker: {0}")]
    Start(#[source] io::Error),
    #[error("PDF worker protocol: {0}")]
    Protocol(String),
    #[error("PDFium unavailable: {0}")]
    PdfiumUnavailable(String),
    #[error("invalid PDF request: {0}")]
    InvalidRequest(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PdfWorkerIdentity {
    pub worker_sha256: String,
    pub pdfium_sha256: String,
}

pub fn pdf_worker_path() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("application executable directory missing"))?;
    let name = format!("ai-daily-pdf-worker{}", std::env::consts::EXE_SUFFIX);
    Ok(directory.join(name))
}

pub(crate) fn pdf_worker_identity(worker_path: &Path) -> Result<PdfWorkerIdentity, PdfClientError> {
    let path = worker_path.canonicalize().map_err(PdfClientError::Start)?;
    let worker_sha256 = reader_sha256(File::open(&path).map_err(PdfClientError::Start)?)
        .map_err(PdfClientError::Start)?;
    let directory = path
        .parent()
        .ok_or_else(|| PdfClientError::Protocol("worker executable directory missing".into()))?;
    open_pinned_pdfium(directory)
        .map_err(|error| PdfClientError::PdfiumUnavailable(error.to_string()))?;
    Ok(PdfWorkerIdentity {
        worker_sha256,
        pdfium_sha256: pinned_pdfium_sha256().into(),
    })
}

#[derive(Default)]
pub(super) struct PdfActivity {
    active: AtomicU64,
    peak: AtomicU64,
}

impl PdfActivity {
    pub(super) fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct PdfRequests {
    sent: AtomicU64,
    activity: Arc<PdfActivity>,
}

#[derive(Clone, Copy)]
struct PdfLimits {
    max_requests: u64,
    idle_ttl: Duration,
    memory_bytes: u64,
}

pub(crate) struct PdfClient {
    worker_path: PathBuf,
    running: Option<(PdfProcess, PdfWorkerHello)>,
    requests: Arc<PdfRequests>,
    limits: PdfLimits,
}

impl PdfClient {
    pub(super) fn new(
        worker_path: PathBuf,
        profile: &NormalizedScannerSettings,
        activity: Arc<PdfActivity>,
    ) -> Self {
        Self {
            worker_path,
            running: None,
            requests: Arc::new(PdfRequests {
                activity,
                ..PdfRequests::default()
            }),
            limits: PdfLimits {
                max_requests: profile.worker_max_requests,
                idle_ttl: Duration::from_millis(profile.worker_idle_ttl_ms),
                memory_bytes: profile.worker_rss_limit_bytes,
            },
        }
    }

    pub(crate) fn business_requests_sent(&self) -> u64 {
        self.requests.sent.load(Ordering::Relaxed)
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub(super) fn close(&mut self) {
        self.running = None;
    }

    pub(crate) fn check(&mut self, deadline: Instant) -> Result<PdfWorkerHello, PdfClientError> {
        remaining(deadline)?;
        if let Some((process, hello)) = &mut self.running {
            if process
                .child()
                .try_wait()
                .map_err(PdfClientError::Start)?
                .is_none()
                && !process.should_recycle(self.limits)
            {
                return Ok(hello.clone());
            }
            self.running = None;
        }

        if !self.worker_path.is_absolute() {
            return Err(PdfClientError::Start(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PDF worker path must be absolute",
            )));
        }
        let mut command = Command::new(&self.worker_path);
        command.arg("--stdio");
        let mut process = PdfProcess::spawn(command, Arc::clone(&self.requests))?;
        let hello = match process.exchange(0, PdfCommand::Hello, deadline)? {
            PdfReply::Hello(hello)
                if hello.pid == process.child().id()
                    && hello.parser_identity == PARSER_CONTENT_IDENTITY
                    && hello.ocr == OCR_AVAILABLE
                    && hello.pdfium_sha256 == pinned_pdfium_sha256() =>
            {
                hello
            }
            PdfReply::PdfiumUnavailable(error) => {
                return Err(PdfClientError::PdfiumUnavailable(error))
            }
            _ => {
                return Err(PdfClientError::Protocol(
                    "PDF worker identity mismatch".into(),
                ))
            }
        };
        process.last_activity = Instant::now();
        self.running = Some((process, hello.clone()));
        Ok(hello)
    }

    pub(crate) fn inspect_and_extract(
        &mut self,
        request: &InspectAndExtract,
        deadline: Instant,
    ) -> Result<PdfResponse, PdfClientError> {
        request.validate().map_err(PdfClientError::InvalidRequest)?;
        let deadline = deadline
            .min(Instant::now() + Duration::from_millis(request.parse.remaining_timeout_ms));
        self.check(deadline)?;
        let mut dispatched = request.clone();
        let budget = remaining(deadline)?;
        dispatched.parse.remaining_timeout_ms =
            u64::try_from(budget.as_millis()).unwrap_or(u64::MAX).max(1);
        let (process, _) = self.running.as_mut().expect("worker handshake completed");
        let id = process.next_id;
        process.next_id += 1;
        let response = process
            .exchange(
                id,
                PdfCommand::InspectAndExtract(Box::new(dispatched)),
                deadline,
            )
            .and_then(|reply| match reply {
                PdfReply::InspectAndExtract(response) => {
                    response
                        .validate_for(request)
                        .map_err(PdfClientError::Protocol)?;
                    remaining(deadline)?;
                    Ok(*response)
                }
                _ => Err(PdfClientError::Protocol("unexpected business reply".into())),
            });
        process.last_activity = Instant::now();
        if response.is_err() || process.should_recycle(self.limits) {
            // The next document can start a fresh process. Never resend this document here.
            self.running = None;
        }
        response
    }
}

type Exchange = (
    PdfRequestFrame,
    Sender<Result<PdfResponseFrame, PdfClientError>>,
);

struct PdfProcess {
    child: Option<Child>,
    frames: Sender<Exchange>,
    next_id: u64,
    last_activity: Instant,
    #[cfg(windows)]
    job: Option<std::os::windows::io::OwnedHandle>,
}

impl PdfProcess {
    fn spawn(mut command: Command, accounting: Arc<PdfRequests>) -> Result<Self, PdfClientError> {
        #[cfg(windows)]
        let job = {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

            command.creation_flags(CREATE_NO_WINDOW);
            kill_on_close_job().map_err(PdfClientError::Start)?
        };
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(PdfClientError::Start)?;
        let (frames, requests) = mpsc::channel();
        let mut process = Self {
            child: Some(child),
            frames,
            next_id: 1,
            last_activity: Instant::now(),
            #[cfg(windows)]
            job: None,
        };
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

            // The worker cannot start PDFium until the parent sends the first frame.
            if unsafe {
                AssignProcessToJobObject(job.as_raw_handle(), process.child().as_raw_handle())
            } == 0
            {
                return Err(PdfClientError::Start(io::Error::last_os_error()));
            }
            process.job = Some(job);
        }
        let child = process.child();
        let input = child.stdin.take().expect("piped PDF stdin");
        let output = child.stdout.take().expect("piped PDF stdout");
        thread::Builder::new()
            .name("pdf-worker-pipes".into())
            .spawn(move || serve_exchanges(input, output, requests, &accounting))
            .map_err(PdfClientError::Start)?;
        Ok(process)
    }

    fn should_recycle(&self, limits: PdfLimits) -> bool {
        self.next_id > limits.max_requests
            || self.last_activity.elapsed() >= limits.idle_ttl
            || self.over_memory(limits.memory_bytes)
    }

    #[cfg(windows)]
    fn over_memory(&self, bytes: u64) -> bool {
        use std::{mem::size_of_val, os::windows::io::AsRawHandle, ptr::null_mut};
        use windows_sys::Win32::System::JobObjects::{
            JobObjectExtendedLimitInformation, QueryInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        };
        let mut peak = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let ok = unsafe {
            QueryInformationJobObject(
                self.job.as_ref().expect("worker Job").as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw mut peak).cast(),
                size_of_val(&peak) as u32,
                null_mut(),
            )
        };
        // Recycle before reuse if the OS cannot attest to this Job's peak committed memory.
        ok == 0 || peak.PeakJobMemoryUsed as u64 >= bytes
    }

    #[cfg(not(windows))]
    fn over_memory(&self, _bytes: u64) -> bool {
        false
    }

    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("live PDF worker child")
    }

    fn exchange(
        &self,
        id: u64,
        command: PdfCommand,
        deadline: Instant,
    ) -> Result<PdfReply, PdfClientError> {
        remaining(deadline)?;
        let (sender, reply) = mpsc::channel();
        let frame = PdfRequestFrame {
            version: PDF_PROTOCOL_VERSION,
            id,
            command,
        };
        self.frames
            .send((frame, sender))
            .map_err(|_| PdfClientError::WorkerExited)?;
        let frame = reply
            .recv_timeout(remaining(deadline)?)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => PdfClientError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => PdfClientError::WorkerExited,
            })??;
        remaining(deadline)?;
        if frame.id != id || frame.version != PDF_PROTOCOL_VERSION {
            return Err(PdfClientError::Protocol("wrong reply ID or version".into()));
        }
        Ok(frame.reply)
    }
}

impl Drop for PdfProcess {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            // Closing the only job handle terminates the entire tree, including on parent exit.
            drop(job);
            return;
        }
        let _ = child.kill();
        #[cfg(not(windows))]
        let _ = thread::Builder::new().spawn(move || child.wait());
    }
}

fn serve_exchanges(
    mut input: impl io::Write,
    mut output: impl io::Read,
    requests: Receiver<Exchange>,
    accounting: &PdfRequests,
) {
    for (frame, reply) in requests {
        let response_limit = frame.command.response_frame_limit();
        let result = write_frame(&mut input, &frame, MAX_REQUEST_FRAME)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::InvalidData {
                    PdfClientError::Protocol(error.to_string())
                } else {
                    PdfClientError::WorkerExited
                }
            })
            .and_then(|()| {
                let activity = &accounting.activity;
                if frame.id != 0 {
                    accounting.sent.fetch_add(1, Ordering::Relaxed);
                    let active = activity.active.fetch_add(1, Ordering::Relaxed) + 1;
                    activity.peak.fetch_max(active, Ordering::Relaxed);
                }
                let result = read_frame(&mut output, response_limit)
                    .map_err(|error| PdfClientError::Protocol(error.to_string()))
                    .and_then(|frame| frame.ok_or(PdfClientError::WorkerExited));
                if frame.id != 0 {
                    activity.active.fetch_sub(1, Ordering::Relaxed);
                }
                result
            });
        let failed = result.is_err();
        if reply.send(result).is_err() || failed {
            break;
        }
    }
}

fn remaining(deadline: Instant) -> Result<Duration, PdfClientError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(PdfClientError::Timeout)
}

#[cfg(windows)]
fn kill_on_close_job() -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::{
        mem::size_of_val,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr::null,
    };
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let raw = unsafe { CreateJobObjectW(null(), null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // This job handle is owned by the parent and cannot be inherited by the child.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use daily_parsers::{
        contract::{ParseRequest, ParserBackend, ParserLimits},
        discovery::build_source_version,
        pdf_wire::PdfNegativeKind,
    };
    use std::{
        collections::BTreeSet,
        os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
        os::windows::process::CommandExt,
        time::UNIX_EPOCH,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0},
        System::{
            JobObjects::IsProcessInJob,
            Threading::{OpenProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_SYNCHRONIZE},
        },
    };

    fn test_worker_path() -> PathBuf {
        let executable = std::env::current_exe().unwrap();
        executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("ai-daily-pdf-worker.exe")
    }

    fn test_client(path: PathBuf) -> PdfClient {
        use daily_parsers::settings::{ReportMode, ScannerSettings};
        let profile = crate::scan::config::normalize_scanner_settings(
            &ScannerSettings::default(),
            ReportMode::Daily,
        )
        .unwrap();
        PdfClient::new(path, &profile, Arc::default())
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[test]
    fn worker_reuses_one_process_and_background_startup_adds_no_console_windows() {
        let baseline = visible_console_windows();
        let worker_path = test_worker_path();
        let identity =
            pdf_worker_identity(&worker_path).expect("build the PDF worker and stage the DLL");
        assert_eq!(identity.pdfium_sha256, pinned_pdfium_sha256());
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/pdf_classifier/error_corrupt_01.pdf");
        let metadata = fixture.metadata().unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap();
        let request = InspectAndExtract::new(
            ParseRequest {
                file_path: fixture.to_string_lossy().into_owned(),
                file_type: ".pdf".into(),
                backend: ParserBackend::RustPdfiumV3,
                remaining_timeout_ms: 10_000,
                max_file_size_bytes: 1024 * 1024,
                expected_source_version: build_source_version(mtime.as_nanos(), metadata.len()),
                parser_limits: ParserLimits::Pdf {
                    max_pages: 2,
                    excerpt_max_chars: 500,
                    selected_pages: None,
                },
            },
            2,
            "pdf_text_presence_v2".into(),
            true,
        );
        let mut recycled = test_client(worker_path.clone());
        recycled.limits.max_requests = 1;
        let first = recycled.check(deadline()).unwrap().pid;
        recycled.inspect_and_extract(&request, deadline()).unwrap();
        assert_ne!(recycled.check(deadline()).unwrap().pid, first);
        assert_eq!(recycled.business_requests_sent(), 1);

        recycled.limits.idle_ttl = Duration::from_secs(1);
        let running = recycled.running.as_mut().unwrap();
        let before_idle = running.1.pid;
        running.0.last_activity -= recycled.limits.idle_ttl;
        assert_ne!(recycled.check(deadline()).unwrap().pid, before_idle);

        recycled.limits.max_requests = 128;
        recycled.limits.memory_bytes = 1;
        let before_memory = recycled.running.as_ref().unwrap().1.pid;
        let running = &recycled.running.as_ref().unwrap().0;
        assert!(running.over_memory(recycled.limits.memory_bytes));
        assert_ne!(recycled.check(deadline()).unwrap().pid, before_memory);
        drop(recycled);

        for _ in 0..20 {
            let mut client = test_client(worker_path.clone());
            let hello = client.check(deadline()).unwrap();
            assert_eq!(client.business_requests_sent(), 0);
            let process = &client.running.as_ref().unwrap().0;
            let handle = process
                .child
                .as_ref()
                .unwrap()
                .as_handle()
                .try_clone_to_owned()
                .unwrap();
            let mut contained = 0;
            assert_ne!(
                unsafe {
                    IsProcessInJob(
                        handle.as_raw_handle(),
                        process.job.as_ref().unwrap().as_raw_handle(),
                        &mut contained,
                    )
                },
                0
            );
            assert_ne!(contained, 0);
            for _ in 0..2 {
                let response = client.inspect_and_extract(&request, deadline()).unwrap();
                assert_eq!(response.native_document_opens, 1);
                assert_eq!(response.negative_kind(), Some(PdfNegativeKind::InvalidPdf));
            }
            assert_eq!(client.business_requests_sent(), 2);
            assert_eq!(client.check(deadline()).unwrap().pid, hello.pid);
            assert!(visible_console_windows().is_subset(&baseline));
            drop(client);
            assert_terminated(&handle);
        }
    }

    #[test]
    fn wait_for_partial_reply_timeout_and_worker_exit_are_distinct() {
        let mut timed = fake_worker("Start-Sleep -Seconds 30");
        let handle = timed.child().as_handle().try_clone_to_owned().unwrap();
        let short = Instant::now() + Duration::from_millis(250);
        assert!(matches!(
            timed.exchange(0, PdfCommand::Hello, short),
            Err(PdfClientError::Timeout)
        ));
        drop(timed);
        assert_terminated(&handle);

        let partial = fake_worker(
            "$out = [Console]::OpenStandardOutput(); $bytes = [byte[]](8,0,0,0,123); \
             $out.Write($bytes,0,$bytes.Length); $out.Flush()",
        );
        assert!(matches!(
            partial.exchange(0, PdfCommand::Hello, deadline()),
            Err(PdfClientError::Protocol(_))
        ));
        let exited = fake_worker("exit 71");
        assert!(matches!(
            exited.exchange(0, PdfCommand::Hello, deadline()),
            Err(PdfClientError::WorkerExited)
        ));
    }

    #[test]
    fn operating_system_reaps_worker_when_parent_exits_without_dropping_client() {
        const PID_FILE: &str = "AIDR_PDF_TEST_PARENT_PID_FILE";
        if let Some(pid_file) = std::env::var_os(PID_FILE) {
            let mut client = test_client(test_worker_path());
            let hello = client.check(deadline()).unwrap();
            std::fs::write(pid_file, hello.pid.to_string()).unwrap();
            std::process::exit(0);
        }
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("worker.pid");
        assert!(Command::new(std::env::current_exe().unwrap())
            .creation_flags(CREATE_NO_WINDOW)
            .env(PID_FILE, &pid_file)
            .args(["--exact", "scan::pdf::tests::operating_system_reaps_worker_when_parent_exits_without_dropping_client"])
            .status().unwrap().success());
        let worker_pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, worker_pid) };
        if raw.is_null() {
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(ERROR_INVALID_PARAMETER as i32)
            );
        } else {
            assert_terminated(&unsafe { OwnedHandle::from_raw_handle(raw) });
        }
    }

    fn fake_worker(action: &str) -> PdfProcess {
        // Consume the request before emitting a truncated answer or exiting.
        let script = format!(
            "$in = [Console]::OpenStandardInput(); $header = New-Object byte[] 4; \
             for ($i = 0; $i -lt 4; $i++) {{ $header[$i] = $in.ReadByte() }}; \
             $len = [BitConverter]::ToInt32($header, 0); \
             for ($i = 0; $i -lt $len; $i++) {{ $null = $in.ReadByte() }}; {action}"
        );
        let program = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut command = Command::new(program);
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ]);
        PdfProcess::spawn(command, Arc::default()).unwrap()
    }

    fn assert_terminated(handle: &OwnedHandle) {
        assert_eq!(
            unsafe { WaitForSingleObject(handle.as_raw_handle(), 5000) },
            WAIT_OBJECT_0
        );
    }

    fn visible_console_windows() -> BTreeSet<isize> {
        #[link(name = "user32")]
        extern "system" {
            fn EnumWindows(
                callback: unsafe extern "system" fn(isize, isize) -> i32,
                state: isize,
            ) -> i32;
            fn IsWindowVisible(window: isize) -> i32;
            fn GetClassNameW(window: isize, buffer: *mut u16, count: i32) -> i32;
        }
        unsafe extern "system" fn visit(window: isize, state: isize) -> i32 {
            let mut name = [0u16; 64];
            let count = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) };
            if count > 0
                && unsafe { IsWindowVisible(window) } != 0
                && name[..count as usize]
                    .iter()
                    .copied()
                    .eq("ConsoleWindowClass".encode_utf16())
            {
                unsafe { &mut *(state as *mut BTreeSet<isize>) }.insert(window);
            }
            1
        }
        let mut windows = BTreeSet::new();
        assert_ne!(
            unsafe { EnumWindows(visit, (&raw mut windows) as isize) },
            0
        );
        windows
    }
}
