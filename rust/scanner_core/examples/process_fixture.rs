//! Development-only child used to exercise containment without an interpreter.
use ai_daily_scanner_core::process::{run_process, ProcessSpec};
use ai_daily_worker_contract::{
    WorkerHello, WorkerKind, WorkerOperation, CONTRACT, CONTRACT_VERSION, PROTOCOL_VERSION,
};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{GetLastError, SetLastError, ERROR_INVALID_HANDLE, ERROR_SUCCESS},
    System::Threading::DETACHED_PROCESS,
};

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|arg| arg.to_str()) {
        Some("echo") => {
            let mut data = Vec::new();
            std::io::stdin().read_to_end(&mut data).unwrap();
            std::io::stdout().write_all(&data).unwrap();
            std::io::stderr().write_all(b"audit").unwrap();
            std::process::exit(7);
        }
        Some("eof") => {
            let mut data = Vec::new();
            std::io::stdin().read_to_end(&mut data).unwrap();
            println!("{}", data.len());
        }
        Some("args") => {
            let values: Vec<_> = args[1..].iter().map(|arg| arg.to_str().unwrap()).collect();
            println!("{}", serde_json::to_string(&values).unwrap());
        }
        Some("rendezvous") => rendezvous(&args[1..]),
        Some("fast") => println!("fast"),
        Some("slow") => {
            std::thread::sleep(Duration::from_millis(1500));
            println!("slow");
        }
        Some("sleep") => {
            #[cfg(windows)]
            assert_no_console();
            std::thread::sleep(Duration::from_secs(30));
        }
        Some("flood") => {
            std::io::stdout()
                .write_all(&vec![b'x'; 1024 * 1024])
                .unwrap();
        }
        Some("memory") => {
            let size: usize = args[1].to_str().unwrap().parse().unwrap();
            let mut memory = vec![0u8; size];
            for page in memory.chunks_mut(4096) {
                page[0] = 1;
            }
            std::hint::black_box(&memory);
            println!("{}", memory.len());
            std::thread::sleep(Duration::from_millis(150));
        }
        Some("spawn") => {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.arg("sleep");
            #[cfg(windows)]
            command.creation_flags(DETACHED_PROCESS);
            let mut child = command.spawn().unwrap();
            std::fs::write(&args[1], child.id().to_string()).unwrap();
            child.wait().unwrap();
        }
        Some("contain") => {
            let mut spec =
                ProcessSpec::new(std::env::current_exe().unwrap(), Duration::from_secs(30));
            spec.args = vec!["spawn".into(), args[1].clone()];
            let _ = run_process(&spec);
        }
        Some("nested") => {
            let mut spec =
                ProcessSpec::new(std::env::current_exe().unwrap(), Duration::from_secs(3));
            spec.args = vec!["fast".into()];
            assert_eq!(run_process(&spec).unwrap().stdout, b"fast\n");
        }
        _ => std::process::exit(2),
    }
}

fn rendezvous(args: &[OsString]) {
    let [role, own_marker, peer_marker, operation] = args else {
        panic!("expected rendezvous <role> <own marker> <peer marker> hello");
    };
    assert_eq!(operation, "hello");
    let role = role.to_str().expect("worker role should be UTF-8");
    let (worker_kind, supported_operations) = match role {
        "office" => (WorkerKind::Office, vec![WorkerOperation::OfficeParse]),
        "pdf" => (
            WorkerKind::Pdf,
            vec![WorkerOperation::PdfClassify, WorkerOperation::PdfParse],
        ),
        _ => panic!("unexpected rendezvous role: {role}"),
    };

    #[cfg(windows)]
    assert_no_console();
    std::fs::write(own_marker, format!("{role}\n")).unwrap();
    let peer_marker = Path::new(peer_marker);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !peer_marker.is_file() {
        if Instant::now() >= deadline {
            eprintln!("{role} did not see peer marker {}", peer_marker.display());
            std::process::exit(7);
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let hello = WorkerHello {
        contract: CONTRACT.into(),
        protocol_version: PROTOCOL_VERSION,
        frame: "hello".into(),
        worker_contract_version: CONTRACT_VERSION.into(),
        worker_kind,
        worker_version: env!("CARGO_PKG_VERSION").into(),
        worker_build: "process-fixture".into(),
        supported_operations,
    };
    println!("{}", serde_json::to_string(&hello).unwrap());
}

#[cfg(windows)]
fn assert_no_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleProcessList(process_ids: *mut u32, process_count: u32) -> u32;
    }

    // A hidden console would still list at least one attached process.
    let mut pid = 0;
    let (count, last_error) = unsafe {
        SetLastError(ERROR_SUCCESS);
        let count = GetConsoleProcessList(&mut pid, 1);
        (count, GetLastError())
    };
    assert_eq!(
        (count, last_error),
        (0, ERROR_INVALID_HANDLE),
        "native fixture must have no attached console"
    );
}
