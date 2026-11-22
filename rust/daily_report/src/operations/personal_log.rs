//! Bounded personal operation detail; historical evidence logs are untouched.
use crate::{snapshots::reject_redirects, store::locking::ReportLock, ReportError};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub(super) fn append(
    log_dir: &Path,
    mut event: Value,
    now: DateTime<Utc>,
) -> Result<(), ReportError> {
    let root = log_dir.join("personal");
    reject_redirects(&root, "LOG_PATH_REDIRECTED")?;
    let _lock = ReportLock::acquire(&root.join("operations"), "personal", Duration::from_secs(2))?;
    if let Some(message) = event["message"].as_str() {
        event["message"] = Value::String(message.chars().take(512).collect());
    }
    let mut line = serde_json::to_vec(&event)?;
    line.push(b'\n');
    if line.len() > 4096 {
        return Err(ReportError::new(
            "LOG_ENTRY_TOO_LARGE",
            "运行日志条目超过 4 KiB",
        ));
    }
    let cutoff = (now.with_timezone(&crate::types::shanghai()).date_naive()
        - chrono::Duration::days(30))
    .to_string();
    let current = format!(
        "scheduled-{}.jsonl",
        now.with_timezone(&crate::types::shanghai())
            .format("%Y-%m-%d")
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in fs::read_dir(&root)? {
        crate::processing_cache::check_deadline(deadline)?;
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(day) = name
            .strip_prefix("scheduled-")
            .and_then(|s| s.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_err() {
            continue;
        }
        reject_redirects(&entry.path(), "LOG_PATH_REDIRECTED")?;
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        if day < cutoff.as_str() {
            fs::remove_file(entry.path())?;
        } else {
            total = total.saturating_add(metadata.len());
            files.push((name, entry.path(), metadata.len()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, path, size) in files {
        if total + line.len() as u64 <= MAX_BYTES {
            break;
        }
        crate::processing_cache::check_deadline(deadline)?;
        fs::remove_file(path)?;
        total = total.saturating_sub(size);
    }
    let path = root.join(current);
    reject_redirects(&path, "LOG_PATH_REDIRECTED")?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&line)?;
    file.sync_all()?;
    Ok(())
}
