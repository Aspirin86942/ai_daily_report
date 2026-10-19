//! Durable observed versions. This store never participates in model-cache retention.
pub mod cli;
pub mod collect;
pub mod config;
pub mod diff;
pub mod maintenance;
pub mod model_input;
pub mod parse;
pub mod queue;
pub mod recovery;
pub mod schedule;
pub mod stats;
pub mod storage;
pub mod store;

use crate::{snapshots::digest, ReportError};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};

pub fn file_hash(path: &Path) -> Result<String, ReportError> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(ai_daily_worker_contract::archive::hex(&hash.finalize()))
}

pub fn identity(parts: &[&str]) -> String {
    digest(&serde_json::to_vec(parts).expect("string array"))
}

pub fn error(code: &'static str) -> ReportError {
    ReportError::new(code, code)
}

pub fn local_day(now: chrono::DateTime<chrono::Utc>) -> String {
    (now + chrono::Duration::hours(8))
        .format("%Y-%m-%d")
        .to_string()
}

pub mod resources;
