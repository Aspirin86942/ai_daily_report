use crate::ReportError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Match the existing Python canonical JSON contract. Existing snapshots are
/// always hashed as original bytes and are never rewritten through this function.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, ReportError> {
    fn write(value: &Value, output: &mut String) -> Result<(), ReportError> {
        match value {
            Value::Object(values) => {
                output.push('{');
                let mut entries: Vec<_> = values.iter().collect();
                entries.sort_by_key(|(key, _)| *key);
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    output.push_str(&serde_json::to_string(key)?);
                    output.push(':');
                    write(value, output)?;
                }
                output.push('}');
            }
            Value::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    write(value, output)?;
                }
                output.push(']');
            }
            Value::Number(number) if number.is_f64() => {
                let number = number
                    .as_f64()
                    .ok_or_else(|| ReportError::new("JSON_INVALID", "浮点数超出范围"))?;
                let raw = serde_json::to_string(&number)?;
                let scientific = number != 0.0 && (number.abs() < 0.0001 || number.abs() >= 1e16);
                if scientific {
                    let exponent_form = format!("{number:e}");
                    let (mantissa, exponent) = exponent_form.split_once('e').unwrap();
                    let exponent: i32 = exponent.parse().unwrap();
                    output.push_str(&format!("{mantissa}e{exponent:+03}"));
                } else {
                    output.push_str(&raw);
                }
            }
            other => output.push_str(&serde_json::to_string(other)?),
        }
        Ok(())
    }
    let mut output = String::new();
    write(value, &mut output)?;
    Ok(output.into_bytes())
}

pub fn fingerprint(value: &Value) -> Result<String, ReportError> {
    Ok(digest(&canonical_json(value)?))
}

pub fn reject_redirects(path: &Path, code: &'static str) -> Result<(), ReportError> {
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(info) => {
                #[cfg(windows)]
                let redirect = {
                    use std::os::windows::fs::MetadataExt;
                    info.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let redirect = info.file_type().is_symlink();
                if redirect {
                    return Err(ReportError::new(code, "路径包含链接或重定向"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRef {
    pub path: String,
    pub sha256: String,
}

pub struct SnapshotStore {
    pub root: PathBuf,
    max_bytes: u64,
    max_file_bytes: u64,
    write_lock: Mutex<()>,
    quota: Option<Arc<crate::processing_cache::CacheQuota>>,
}

pub(crate) const RESPONSE_RESERVATION_BYTES: u64 = 2 * 1024 * 1024;

impl SnapshotStore {
    pub fn new(root: PathBuf, max_bytes: u64) -> Result<Self, ReportError> {
        let root = std::path::absolute(root)?;
        reject_redirects(&root, "SNAPSHOT_PATH_REDIRECTED")?;
        fs::create_dir_all(&root)?;
        Ok(Self {
            root: fs::canonicalize(root)?,
            max_bytes,
            max_file_bytes: max_bytes.min(512 * 1024 * 1024),
            write_lock: Mutex::new(()),
            quota: None,
        })
    }
    pub fn with_quota(
        root: PathBuf,
        max_bytes: u64,
        quota: Arc<crate::processing_cache::CacheQuota>,
    ) -> Result<Self, ReportError> {
        let mut store = Self::new(root, max_bytes)?;
        store.quota = Some(quota);
        Ok(store)
    }

    pub(crate) fn reserve_response(
        &self,
    ) -> Result<Option<crate::processing_cache::CacheReservation>, ReportError> {
        // HttpTransport caps response bytes at 1 MiB; snapshot publication briefly
        // has two logical paths. Hold this capacity before authorizing a request.
        self.quota
            .as_ref()
            .map(|q| q.reserve(RESPONSE_RESERVATION_BYTES))
            .transpose()
    }

    fn resolve(&self, relative: &str) -> Result<PathBuf, ReportError> {
        let invalid = || ReportError::new("SNAPSHOT_PATH_INVALID", "无效快照路径");
        let (run, filename) = relative.split_once('/').ok_or_else(invalid)?;
        if !hex(run, 32) {
            return Err(invalid());
        }
        let stem = filename.strip_suffix(".json").ok_or_else(invalid)?;
        let (kind, digest) = stem.rsplit_once('-').ok_or_else(invalid)?;
        if !hex(digest, 64)
            || !kind.starts_with(|ch: char| ch.is_ascii_lowercase())
            || !kind.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            })
        {
            return Err(invalid());
        }
        let path = self.root.join(run).join(filename);
        reject_redirects(&path, "SNAPSHOT_PATH_REDIRECTED")?;
        Ok(path)
    }

    pub fn total_bytes(&self) -> Result<u64, ReportError> {
        let mut total = 0u64;
        for entry in walkdir::WalkDir::new(&self.root).follow_links(false) {
            let entry =
                entry.map_err(|_| ReportError::new("SNAPSHOT_READ_FAILED", "无法统计快照目录"))?;
            reject_redirects(entry.path(), "SNAPSHOT_PATH_REDIRECTED")?;
            if entry.file_type().is_file() {
                total += entry
                    .metadata()
                    .map_err(|_| ReportError::new("SNAPSHOT_READ_FAILED", "无法读取快照大小"))?
                    .len();
            }
        }
        Ok(total)
    }

    pub fn put(&self, run: &str, kind: &str, value: &Value) -> Result<SnapshotRef, ReportError> {
        self.put_bytes(run, kind, &canonical_json(value)?)
    }

    pub fn put_bytes(
        &self,
        run: &str,
        kind: &str,
        bytes: &[u8],
    ) -> Result<SnapshotRef, ReportError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| ReportError::new("SNAPSHOT_WRITE_FAILED", "快照写入锁不可用"))?;
        let sha256 = digest(bytes);
        let reference = SnapshotRef {
            path: format!("{run}/{kind}-{sha256}.json"),
            sha256,
        };
        let path = self.resolve(&reference.path)?;
        if path.exists() {
            self.read_bytes(&reference)?;
            return Ok(reference);
        }
        if bytes.len() as u64 > self.max_file_bytes
            || self.quota.is_none()
                && self.total_bytes()?.saturating_add(bytes.len() as u64) > self.max_bytes
        {
            return Err(ReportError::new(
                "SNAPSHOT_CAPACITY_EXCEEDED",
                "快照容量达到上限",
            ));
        }
        fs::create_dir_all(path.parent().unwrap())?;
        self.resolve(&reference.path)?;
        let mut reservation = self
            .quota
            .as_ref()
            .map(|quota| quota.reserve((bytes.len() as u64).saturating_mul(2)))
            .transpose()?;
        let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        // Register before writing content. Empty, interrupted files fail closed on reconciliation.
        if let Some(reservation) = &mut reservation {
            reservation.track_existing_empty(temporary.path())?;
            reservation.track(&path)?;
        }
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        match fs::hard_link(temporary.path(), &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.read_bytes(&reference)?;
            }
            Err(error) => return Err(error.into()),
        }
        drop(temporary);
        if let Some(reservation) = reservation {
            reservation.commit(&[path])?;
        }
        Ok(reference)
    }

    pub fn read_bytes(&self, reference: &SnapshotRef) -> Result<Vec<u8>, ReportError> {
        let path = self.resolve(&reference.path)?;
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|_| ReportError::new("SNAPSHOT_MISSING", "快照文件不存在或无法读取"))?
            .take(self.max_file_bytes + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > self.max_file_bytes || digest(&bytes) != reference.sha256 {
            return Err(ReportError::new(
                "SNAPSHOT_HASH_MISMATCH",
                "快照原字节校验失败",
            ));
        }
        Ok(bytes)
    }

    pub fn read(&self, reference: &SnapshotRef) -> Result<Value, ReportError> {
        serde_json::from_slice(&self.read_bytes(reference)?)
            .map_err(|_| ReportError::new("SNAPSHOT_JSON_INVALID", "快照 JSON 无效"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn large_input_round_trips_without_bypassing_total_capacity_or_hashes() {
        let directory = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(directory.path().to_path_buf(), 16 * 1024 * 1024).unwrap();
        let run = crate::types::new_run_id();
        let input = json!({"text": "a".repeat(9 * 1024 * 1024)});
        let reference = store.put(&run, "input", &input).unwrap();
        assert_eq!(store.read(&reference).unwrap(), input);
        assert_eq!(
            store.put(&run, "another-input", &input).unwrap_err().code,
            "SNAPSHOT_CAPACITY_EXCEEDED"
        );
        fs::write(store.root.join(&reference.path), b"{}").unwrap();
        assert!(store.read(&reference).is_err());
    }
}
