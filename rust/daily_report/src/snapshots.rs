pub use crate::infra::fs::reject_redirects;
pub use crate::infra::json::{canonical_json, digest, fingerprint, valid_hash};
#[cfg(test)]
use crate::infra::json::{
    frozen_number, frozen_report_json, read_frozen_report_json, FROZEN_NUMBER,
};
use crate::{
    infra::json::{decode_frozen_json, valid_run_id},
    processing_cache::{write_immutable, write_reservation_bytes},
    ReportError,
};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use std::{
    fs::{self, File},
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex},
};

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

pub(crate) const RESPONSE_RESERVATION_BYTES: u64 = write_reservation_bytes(1024 * 1024);

impl SnapshotStore {
    /// Open saved snapshots without creating directories or pruning content.
    pub(crate) fn retained(root: PathBuf, max_bytes: u64) -> Result<Self, ReportError> {
        let root = std::path::absolute(root)?;
        reject_redirects(&root, "SNAPSHOT_PATH_REDIRECTED")?;
        Ok(Self {
            root,
            max_bytes,
            max_file_bytes: max_bytes.min(512 * 1024 * 1024),
            write_lock: Mutex::new(()),
            quota: None,
        })
    }
    pub fn new(root: PathBuf, max_bytes: u64) -> Result<Self, ReportError> {
        let mut store = Self::retained(root, max_bytes)?;
        fs::create_dir_all(&store.root)?;
        store.root = fs::canonicalize(store.root)?;
        Ok(store)
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
        if !valid_run_id(run) {
            return Err(invalid());
        }
        let stem = filename.strip_suffix(".json").ok_or_else(invalid)?;
        let (kind, digest) = stem.rsplit_once('-').ok_or_else(invalid)?;
        if !valid_hash(digest)
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
        write_immutable(
            &path,
            bytes,
            self.quota.as_ref(),
            "SNAPSHOT_PATH_REDIRECTED",
            |_| self.read_bytes(&reference).map(|_| ()),
        )?;
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
        let bytes = self.read_bytes(reference)?;
        let filename = reference
            .path
            .split_once('/')
            .map_or("", |(_, filename)| filename);
        if ["personal-input-", "personal-materials-", "model-plan-"]
            .iter()
            .any(|prefix| filename.starts_with(prefix))
        {
            #[derive(Deserialize)]
            struct Version {
                version: u32,
            }
            if serde_json::from_slice::<Version>(&bytes).is_ok_and(|v| {
                if filename.starts_with("model-plan-") {
                    matches!(v.version, 6..=8)
                } else {
                    matches!(v.version, 5..=7)
                }
            }) {
                return decode_frozen_json(serde_json::from_slice::<&RawValue>(&bytes)?, 0);
            }
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| ReportError::new("SNAPSHOT_JSON_INVALID", "快照 JSON 无效"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn historical_number_boundaries_keep_json_tokens_and_template_types() {
        for (raw, json, canonical, rendered, kind) in [
            (
                "18446744073709551615",
                "18446744073709551615",
                "18446744073709551615",
                "18446744073709551615",
                "number",
            ),
            (
                "18446744073709551616",
                "18446744073709551616",
                "18446744073709551616",
                "18446744073709551616",
                "string",
            ),
            (
                "18446744073709551617",
                "18446744073709551617",
                "18446744073709551617",
                "18446744073709551617",
                "string",
            ),
            (
                "-9223372036854775809",
                "-9223372036854775809",
                "-9223372036854775809",
                "-9223372036854775809",
                "string",
            ),
            ("0.12500", "0.12500", "0.125", "0.125", "number"),
            ("1E05", "1e+05", "100000.0", "100000.0", "number"),
            ("-0.00", "-0.00", "-0.0", "-0.0", "number"),
            ("-0", "0", "0", "0", "number"),
            ("null", "null", "null", "None", "none"),
            ("1E4096", "1e+4096", "1e+4096", "1e+4096", "string"),
            ("1E-07", "1e-07", "1e-07", "0.0000001", "number"),
        ] {
            let value = read_frozen_report_json(raw).unwrap();
            assert_eq!(frozen_report_json(&value).unwrap(), json, "{raw}");
            assert_eq!(
                canonical_json(&value).unwrap(),
                canonical.as_bytes(),
                "{raw}"
            );
            let template = crate::reports::template_value(&value);
            assert_eq!(template.to_string(), rendered, "{raw}");
            assert_eq!(template.kind().to_string(), kind, "{raw}");
        }
    }

    #[test]
    fn historical_user_objects_round_trip_without_becoming_number_tags() {
        let raw = r#"{"\u0000ai_daily_report::frozen_number":"18446744073709551617"}"#;
        let report = read_frozen_report_json(raw).unwrap();
        assert!(frozen_number(&report).is_none());
        assert_eq!(frozen_report_json(&report).unwrap(), raw);
        assert_eq!(canonical_json(&report).unwrap(), raw.as_bytes());
        let view = crate::reports::template_value(&report);
        assert_eq!(view.kind().to_string(), "map");
        let rendered = crate::reports::environment()
            .render_str(
                "{{ report[key] }}",
                minijinja::context! { report => view, key => FROZEN_NUMBER },
            )
            .unwrap();
        assert_eq!(rendered, "18446744073709551617");
    }

    #[test]
    fn historical_json_numbers_survive_snapshot_template_and_hash() {
        let directory = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(directory.path().to_path_buf(), 1024 * 1024).unwrap();
        let run = crate::types::new_run_id();
        let raw = br#"{"version":6,"request":{"giant":18446744073709551616001,"negative":-9223372036854775809,"decimal":0.12500,"exponent":1E-07,"negative_zero":-0.0,"bare_zero":-0,"none":null,"overflow":1E4096}}"#;
        let snapshot = store.put_bytes(&run, "model-plan", raw).unwrap();
        let value = store.read(&snapshot).unwrap();
        let expected = br#"{"request":{"bare_zero":0,"decimal":0.125,"exponent":1e-07,"giant":18446744073709551616001,"negative":-9223372036854775809,"negative_zero":-0.0,"none":null,"overflow":1e+4096},"version":6}"#;
        let canonical = canonical_json(&value).unwrap();
        assert_eq!(canonical, expected);
        assert_eq!(
            digest(&canonical),
            "ff873de204f63a747a452da825aceb302f4ade41aa26648a2fdf5411c051fb59"
        );
        assert_eq!(store.read_bytes(&snapshot).unwrap(), raw);

        let request = crate::reports::template_value(&value["request"]);
        let rendered = crate::reports::environment()
            .render_str(
                "{{v.giant}}|{{v.negative}}|{{v.decimal}}|{{v.exponent}}|{{v.negative_zero}}|{{v.bare_zero}}|{{v.none}}|{{v.overflow}}",
                minijinja::context! { v => request },
            )
            .unwrap();
        assert_eq!(
            rendered,
            "18446744073709551616001|-9223372036854775809|0.125|0.0000001|-0.0|0|None|1e+4096"
        );
        let serialized = frozen_report_json(&value).unwrap();
        assert!(serialized.contains("\"giant\":18446744073709551616001"));
        assert!(serialized.contains("\"decimal\":0.12500"));
        assert!(serialized.contains("\"overflow\":1e+4096"));
        assert_eq!(
            frozen_number(&read_frozen_report_json(&serialized).unwrap()["request"]["overflow"]),
            Some("1e+4096")
        );
        assert_eq!(
            canonical_json(
                &decode_frozen_json(serde_json::from_str(&serialized).unwrap(), 0).unwrap()
            )
            .unwrap(),
            expected
        );
    }

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
