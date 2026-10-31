//! Historical parser identity for retained archive queries.
pub use super::parse_identity::profile;
use super::{error, file_hash};
use crate::ReportError;
use ai_daily_worker_contract::archive::ArchiveArtifact;
use std::fs;
pub fn verify_artifact(
    dir: &std::path::Path,
    a: &ArchiveArtifact,
    hash: &str,
) -> Result<(), ReportError> {
    if a.version != 1 || a.raw_sha256 != hash || a.error.is_some() {
        return Err(error("ARCHIVE_EXTRACTION_INVALID"));
    }
    for (file, expected) in [
        ("body.md", &a.body_sha256),
        ("units.jsonl", &a.units_sha256),
        ("units.idx", &a.index_sha256),
    ] {
        let actual = file_hash(&dir.join(file))?;
        if actual != *expected {
            return Err(error("ARCHIVE_EXTRACTION_CORRUPT"));
        }
    }
    if fs::metadata(dir.join("body.md"))?.len() != a.body_bytes
        || fs::metadata(dir.join("units.jsonl"))?.len() != a.units_bytes
        || fs::metadata(dir.join("units.idx"))?.len() != a.units * 8
    {
        return Err(error("ARCHIVE_EXTRACTION_INVALID"));
    }
    Ok(())
}
