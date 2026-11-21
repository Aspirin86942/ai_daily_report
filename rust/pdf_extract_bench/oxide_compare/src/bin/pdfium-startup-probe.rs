use pdfium_render::prelude::Pdfium;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args_os()
        .nth(1)
        .ok_or("expected DLL path or --noop")?;
    if arg == "--noop" {
        println!("{{}}");
        return Ok(());
    }
    let library = PathBuf::from(arg);
    let pin: serde_json::Value =
        serde_json::from_str(include_str!("../../../../build-inputs.json"))?;
    let start = Instant::now();
    let bytes = std::fs::read(&library)?;
    let read = Instant::now();
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if Some(digest.as_str()) != pin["pdfium"]["dll_sha256"].as_str() {
        return Err("incorrect PDFium DLL".into());
    }
    let verified = Instant::now();
    let bindings = Pdfium::bind_to_library(library)?;
    let bound = Instant::now();
    let _pdfium = Pdfium::new(bindings);
    let initialized = Instant::now();
    println!(
        "{}",
        json!({
            "read_us": (read - start).as_micros(),
            "sha256_us": (verified - read).as_micros(),
            "load_and_bind_us": (bound - verified).as_micros(),
            "init_us": (initialized - bound).as_micros(),
            "total_us": (initialized - start).as_micros()
        })
    );
    Ok(())
}
