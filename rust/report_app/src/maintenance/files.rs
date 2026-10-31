use crate::{snapshots::reject_redirects, ReportError};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

pub fn read_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, ReportError> {
    reject_redirects(path, "MAINTENANCE_PATH_REDIRECTED")?;
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
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
pub fn read_json(path: &Path) -> Result<Value, ReportError> {
    Ok(serde_json::from_slice(&read_bytes(path, 8 * 1024 * 1024)?)?)
}
pub fn write_immutable(path: &Path, bytes: &[u8]) -> Result<(), ReportError> {
    reject_redirects(path, "MAINTENANCE_PATH_REDIRECTED")?;
    let parent = path
        .parent()
        .ok_or_else(|| ReportError::new("PATH_INVALID", "文件缺少父目录"))?;
    fs::create_dir_all(parent)?;
    if path.exists() {
        if read_bytes(path, bytes.len() as u64)? != bytes {
            return Err(ReportError::new(
                "MAINTENANCE_ARTIFACT_CHANGED",
                "已保存的维护材料发生变化",
            ));
        }
        return Ok(());
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).map_err(|_| {
        ReportError::new("MAINTENANCE_ARTIFACT_CONFLICT", "维护材料已被其他操作创建")
    })?;
    Ok(())
}
