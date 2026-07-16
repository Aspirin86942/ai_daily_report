//! File inventory and per-run parser audit rows.

use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheMissReason, CacheStatus, Diagnostic, FileAudit, ParseStatus, Validate,
};
use rusqlite::{params, Transaction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryRecord {
    pub file_identity: String,
    pub absolute_path: String,
    pub relative_path: String,
    pub file_type: String,
    pub source_version: String,
    pub size_bytes: u64,
    pub mtime_ns: u64,
}

impl InventoryRecord {
    pub fn from_discovered(
        file: &DiscoveredFileOut,
        relative_path: String,
    ) -> Result<Self, String> {
        let (mtime_ns, source_size) = parse_source_version(&file.source_version)?;
        if source_size != file.size_bytes {
            return Err("discovery source version and size disagree".to_string());
        }
        if relative_path.is_empty() {
            return Err("inventory relative path must not be empty".to_string());
        }
        Ok(Self {
            file_identity: file.file_identity.clone(),
            absolute_path: file.path.clone(),
            relative_path,
            file_type: file.extension.clone(),
            source_version: file.source_version.clone(),
            size_bytes: file.size_bytes,
            mtime_ns,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.file_identity.is_empty()
            || self.file_identity.chars().count() > 4_096
            || !is_contract_absolute_path(&self.absolute_path)
            || !is_safe_relative_path(&self.relative_path)
            || !is_lowercase_extension(&self.file_type)
            || self.source_version.is_empty()
            || self.size_bytes > i64::MAX as u64
            || self.mtime_ns > i64::MAX as u64
        {
            return Err("invalid inventory record".to_string());
        }
        let (mtime_ns, size_bytes) = parse_source_version(&self.source_version)?;
        if mtime_ns != self.mtime_ns || size_bytes != self.size_bytes {
            return Err("inventory source version is inconsistent".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileResultRecord {
    pub file_identity: String,
    pub relative_path: String,
    pub source_version: String,
    pub parse_profile_hash: String,
    pub cache_status: CacheStatus,
    pub cache_miss_reason: CacheMissReason,
    pub parse_status: ParseStatus,
    pub parser_backend: String,
    pub worker_lane: AuditWorkerLane,
    pub truncated: bool,
    pub content_sha256: String,
    pub primary_duration_ms: u64,
    pub fallback_duration_ms: u64,
    pub parse_duration_ms: u64,
    pub failure_class: String,
    pub fallback_backend: String,
    pub fallback_reason_code: String,
    pub error: Option<Diagnostic>,
}

impl FileResultRecord {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let durations_fit = [
            self.primary_duration_ms,
            self.fallback_duration_ms,
            self.parse_duration_ms,
        ]
        .into_iter()
        .all(|value| value <= i64::MAX as u64);
        let cache_consistent = matches!(
            (self.cache_status, self.cache_miss_reason),
            (CacheStatus::Fresh, CacheMissReason::None)
                | (CacheStatus::Miss, CacheMissReason::NewFile)
                | (CacheStatus::Miss, CacheMissReason::ErrorCache)
                | (CacheStatus::Miss, CacheMissReason::SourceVersionChanged)
                | (CacheStatus::Miss, CacheMissReason::ParserProfileChanged)
        );
        let error_consistent = match self.parse_status {
            ParseStatus::Success => self.error.is_none(),
            ParseStatus::Error | ParseStatus::Timeout => self.error.is_some(),
            ParseStatus::NotParsed => true,
        };
        let audit = FileAudit {
            relative_path: self.relative_path.clone(),
            file_identity: self.file_identity.clone(),
            source_version: self.source_version.clone(),
            parse_status: self.parse_status,
            parser_backend: self.parser_backend.clone(),
            worker_lane: self.worker_lane,
            cache_status: self.cache_status,
            cache_miss_reason: self.cache_miss_reason,
            truncated: self.truncated,
            content_sha256: self.content_sha256.clone(),
            parse_duration_ms: self.parse_duration_ms,
            failure_class: self.failure_class.clone(),
            fallback_backend: self.fallback_backend.clone(),
            fallback_reason_code: self.fallback_reason_code.clone(),
        };
        if self.file_identity.is_empty()
            || self.relative_path.is_empty()
            || self.source_version.is_empty()
            || !is_sha256(&self.parse_profile_hash)
            || self.parser_backend.is_empty()
            || !is_sha256(&self.content_sha256)
            || !durations_fit
            || !cache_consistent
            || !error_consistent
            || audit.validate().is_err()
            || self
                .error
                .as_ref()
                .is_some_and(|diagnostic| diagnostic.validate().is_err())
        {
            return Err("invalid scan file result".to_string());
        }
        Ok(())
    }
}

fn is_contract_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    let drive_rooted = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    (value.starts_with('/') || value.starts_with("\\\\") || drive_rooted)
        && !value.contains('\0')
        && value.chars().count() <= 32_767
}

fn is_safe_relative_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    let drive_prefixed = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    !value.is_empty()
        && value.chars().count() <= 32_767
        && !value.starts_with('/')
        && !value.starts_with('\\')
        && !drive_prefixed
        && !value.contains('\0')
        && !value.split(['/', '\\']).any(|component| component == "..")
}

fn is_lowercase_extension(value: &str) -> bool {
    (2..=32).contains(&value.chars().count())
        && value.starts_with('.')
        && value.chars().skip(1).all(|character| {
            !character.is_ascii_uppercase() && !matches!(character, '\\' | '/' | ':' | '\0')
        })
}

pub(crate) fn upsert_inventory(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    seen_at_ms: i64,
    records: &[InventoryRecord],
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO file_inventory(
            file_identity, absolute_path, relative_path, file_type, source_version,
            size_bytes, mtime_ns, last_seen_run_id, last_seen_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(file_identity) DO UPDATE SET
            absolute_path=excluded.absolute_path,
            relative_path=excluded.relative_path,
            file_type=excluded.file_type,
            source_version=excluded.source_version,
            size_bytes=excluded.size_bytes,
            mtime_ns=excluded.mtime_ns,
            last_seen_run_id=excluded.last_seen_run_id,
            last_seen_at_ms=excluded.last_seen_at_ms",
    )?;
    for record in records {
        statement.execute(params![
            record.file_identity,
            record.absolute_path,
            record.relative_path,
            record.file_type,
            record.source_version,
            record.size_bytes as i64,
            record.mtime_ns as i64,
            scan_run_id,
            seen_at_ms,
        ])?;
    }
    Ok(())
}

pub(crate) fn insert_file_results(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    records: &[FileResultRecord],
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO scan_file_results(
            scan_run_id, file_identity, relative_path, source_version,
            parse_profile_hash, cache_status, cache_miss_reason, parse_status,
            parser_backend, worker_lane, truncated, content_sha256,
            primary_duration_ms, fallback_duration_ms, parse_duration_ms,
            failure_class, fallback_backend, fallback_reason_code,
            error_code, error_message, error_retryable, error_stage,
            error_file_path, error_backend
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
         )",
    )?;
    for record in records {
        let error_code = record
            .error
            .as_ref()
            .map(|value| enum_text(&value.error_code));
        let error_message = record.error.as_ref().map(|value| value.message.as_str());
        let error_retryable = record
            .error
            .as_ref()
            .map(|value| i64::from(value.retryable));
        let error_stage = record.error.as_ref().map(|value| enum_text(&value.stage));
        let error_file_path = record
            .error
            .as_ref()
            .and_then(|value| value.file_path.0.as_deref());
        let error_backend = record
            .error
            .as_ref()
            .and_then(|value| value.backend.0.as_deref());
        statement.execute(params![
            scan_run_id,
            record.file_identity,
            record.relative_path,
            record.source_version,
            record.parse_profile_hash,
            cache_status_text(record.cache_status),
            cache_miss_reason_text(record.cache_miss_reason),
            parse_status_text(record.parse_status),
            record.parser_backend,
            worker_lane_text(record.worker_lane),
            i64::from(record.truncated),
            record.content_sha256,
            record.primary_duration_ms as i64,
            record.fallback_duration_ms as i64,
            record.parse_duration_ms as i64,
            record.failure_class,
            record.fallback_backend,
            record.fallback_reason_code,
            error_code,
            error_message,
            error_retryable,
            error_stage,
            error_file_path,
            error_backend,
        ])?;
    }
    Ok(())
}

pub(crate) fn parse_source_version(value: &str) -> Result<(u64, u64), String> {
    let Some((mtime, size)) = value
        .strip_prefix("mtime_ns=")
        .and_then(|rest| rest.split_once(":size="))
    else {
        return Err("invalid source version".to_string());
    };
    let mtime_ns = mtime
        .parse::<u64>()
        .map_err(|_| "invalid source mtime".to_string())?;
    let size_bytes = size
        .parse::<u64>()
        .map_err(|_| "invalid source size".to_string())?;
    Ok((mtime_ns, size_bytes))
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub(crate) fn enum_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .expect("contract enum must serialize to text")
}

pub(crate) fn cache_status_text(value: CacheStatus) -> &'static str {
    match value {
        CacheStatus::Fresh => "fresh",
        CacheStatus::Miss => "miss",
    }
}

pub(crate) fn cache_miss_reason_text(value: CacheMissReason) -> &'static str {
    match value {
        CacheMissReason::None => "",
        CacheMissReason::NewFile => "new_file",
        CacheMissReason::ErrorCache => "error_cache",
        CacheMissReason::SourceVersionChanged => "source_version_changed",
        CacheMissReason::ParserProfileChanged => "parser_profile_changed",
    }
}

pub(crate) fn parse_status_text(value: ParseStatus) -> &'static str {
    match value {
        ParseStatus::Success => "success",
        ParseStatus::Error => "error",
        ParseStatus::Timeout => "timeout",
        ParseStatus::NotParsed => "not_parsed",
    }
}

pub(crate) fn worker_lane_text(value: AuditWorkerLane) -> &'static str {
    match value {
        AuditWorkerLane::RustCore => "rust_core",
        AuditWorkerLane::RustOfficeProcess => "rust_office_process",
        AuditWorkerLane::PythonDocumentProcess => "python_document_process",
        AuditWorkerLane::NotParsed => "not_parsed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_inventory_requires_source_size_to_agree() {
        let file = DiscoveredFileOut {
            file_identity: "identity".to_string(),
            path: "C:\\work\\file.txt".to_string(),
            extension: ".txt".to_string(),
            modified_at: "2026-07-16T00:00:00+08:00".to_string(),
            size_bytes: 8,
            source_version: "mtime_ns=123:size=9".to_string(),
        };

        assert!(InventoryRecord::from_discovered(&file, "file.txt".to_string()).is_err());
    }
}
