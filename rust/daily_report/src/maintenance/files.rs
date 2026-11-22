use crate::{snapshots::reject_redirects, ReportError};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};

pub fn read_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, ReportError> {
    reject_redirects(path, "MAINTENANCE_PATH_REDIRECTED")?;
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(ReportError::new(
            "MAINTENANCE_FILE_LIMIT",
            "维护材料超过读取上限",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(ReportError::new(
            "MAINTENANCE_FILE_LIMIT",
            "维护材料超过读取上限",
        ));
    }
    Ok(bytes)
}

pub(crate) fn write_synced(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}
