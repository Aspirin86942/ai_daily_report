//! Development-only child used to exercise containment without an interpreter.
use ai_daily_scanner_core::process::{run_process, ProcessSpec};
use std::{
    io::{Read, Write},
    process::Command,
    time::Duration,
};
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
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
        Some("args") => println!("{}", serde_json::to_string(&args[1..]).unwrap()),
        Some("fast") => println!("fast"),
        Some("slow") => {
            std::thread::sleep(Duration::from_millis(1500));
            println!("slow");
        }
        Some("sleep") => std::thread::sleep(Duration::from_secs(30)),
        Some("flood") => {
            std::io::stdout()
                .write_all(&vec![b'x'; 1024 * 1024])
                .unwrap();
        }
        Some("memory") => {
            let size: usize = args[1].parse().unwrap();
            let mut memory = vec![0u8; size];
            for page in memory.chunks_mut(4096) {
                page[0] = 1;
            }
            std::hint::black_box(&memory);
            println!("{}", memory.len());
            std::thread::sleep(Duration::from_millis(150));
        }
        Some("spawn") => {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .arg("sleep")
                .spawn()
                .unwrap();
            std::fs::write(&args[1], child.id().to_string()).unwrap();
            child.wait().unwrap();
        }
        Some("contain") => {
            let mut spec =
                ProcessSpec::new(std::env::current_exe().unwrap(), Duration::from_secs(30));
            spec.args = vec!["spawn".into(), args[1].clone().into()];
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
