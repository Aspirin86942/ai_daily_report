//! File inventory and per-run parser audit rows.

use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    validate_v2_parse_provenance, AuditWorkerLane, CacheMissReason, CacheStatus, Diagnostic,
    FileAudit, ParseCacheStatus, ParseStatus, ParseTransport, PdfClassificationAuditV1, Validate,
};
use rusqlite::{params, Transaction};

use crate::source_guard::{source_guard_kind_from_text, SourceGuardKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryRecord {
    pub file_identity: String,
    pub absolute_path: String,
    pub relative_path: String,
    pub file_type: String,
    pub source_version: String,
    pub size_bytes: u64,
    pub mtime_ns: u64,
    /// Engine-owned SourceGuardV2 wire kind; null only for pre-v2 inventory.
    pub source_guard_kind: Option<String>,
    /// Engine-owned SourceGuardV2 SHA-256; must be null when kind=unavailable.
    pub source_guard_sha256: Option<String>,
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
        if !valid_inventory_guard(&file.source_guard_kind, &file.source_guard_sha256) {
            return Err("discovery source guard violates the inventory invariant".to_string());
        }
        Ok(Self {
            file_identity: file.file_identity.clone(),
            absolute_path: file.path.clone(),
            relative_path,
            file_type: file.extension.clone(),
            source_version: file.source_version.clone(),
            size_bytes: file.size_bytes,
            mtime_ns,
            source_guard_kind: file.source_guard_kind.clone(),
            source_guard_sha256: file.source_guard_sha256.clone(),
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
        if !valid_inventory_guard(&self.source_guard_kind, &self.source_guard_sha256) {
            return Err("inventory source guard violates the invariant".to_string());
        }
        Ok(())
    }
}

/// Enforces the Plan 1 `file_inventory` CHECK: both null, or
/// kind=unavailable with a null hash, or an available kind with a 64-char
/// lowercase-hex SHA-256. Anything else fails closed.
fn valid_inventory_guard(kind: &Option<String>, hash: &Option<String>) -> bool {
    match (kind, hash) {
        (None, None) => true,
        (Some(kind), None) => {
            source_guard_kind_from_text(kind) == Some(SourceGuardKind::Unavailable)
        }
        (Some(kind), Some(hash)) => {
            is_sha256(hash)
                && matches!(
                    source_guard_kind_from_text(kind),
                    Some(
                        SourceGuardKind::WindowsFileIdChangeTimeV1
                            | SourceGuardKind::UnixInodeCtimeV1
                            | SourceGuardKind::ContentSha256V1
                    )
                )
        }
        _ => false,
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
    pub parse_transport: ParseTransport,
    pub parse_attempt_count: u64,
    pub pdf_classification: Option<PdfClassificationAuditV1>,
    pub error: Option<Diagnostic>,
}

impl FileResultRecord {
    #[cfg(test)]
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.validate_for_persistence(false)
    }

    pub(crate) fn validate_for_persistence(&self, snapshot_rows: bool) -> Result<(), String> {
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
                | (CacheStatus::Miss, CacheMissReason::SourceVersionChanged)
                | (CacheStatus::Miss, CacheMissReason::ParserIdentityChanged)
                | (CacheStatus::Miss, CacheMissReason::EntryAbsentOrEvicted)
        );
        let error_consistent = match self.parse_status {
            ParseStatus::Success => self.error.is_none(),
            ParseStatus::Error | ParseStatus::Timeout => self.error.is_some(),
            ParseStatus::NotParsed => self.error.is_none(),
        };
        let execution_consistent = if snapshot_rows {
            !matches!(self.parse_status, ParseStatus::Error | ParseStatus::Timeout)
                && self.parse_transport == ParseTransport::Snapshot
                && self.parse_attempt_count == 0
                && self.primary_duration_ms == 0
                && self.fallback_duration_ms == 0
                && self.parse_duration_ms == 0
        } else {
            match parse_cache_status_text(self) {
                "fresh" => {
                    self.parse_status == ParseStatus::Success
                        && self.parse_transport == ParseTransport::NotApplicable
                        && self.parse_attempt_count == 0
                        && self.primary_duration_ms == 0
                        && self.fallback_duration_ms == 0
                        && self.parse_duration_ms == 0
                }
                "miss" => {
                    matches!(
                        self.parse_status,
                        ParseStatus::Success | ParseStatus::Error | ParseStatus::Timeout
                    ) && matches!(
                        self.parse_transport,
                        ParseTransport::Session
                            | ParseTransport::OneShot
                            | ParseTransport::RustInProcess
                    ) && (1..=3).contains(&self.parse_attempt_count)
                }
                "not_applicable" => {
                    self.parse_transport == ParseTransport::NotApplicable
                        && self.parse_attempt_count == 0
                        && self.primary_duration_ms == 0
                        && self.fallback_duration_ms == 0
                        && self.parse_duration_ms == 0
                }
                _ => false,
            }
        };
        let parse_cache_status = if snapshot_rows {
            ParseCacheStatus::Snapshot
        } else {
            match parse_cache_status_text(self) {
                "fresh" => ParseCacheStatus::Fresh,
                "miss" => ParseCacheStatus::Miss,
                "not_applicable" => ParseCacheStatus::NotApplicable,
                _ => return Err("invalid scan file result".to_string()),
            }
        };
        let provenance_consistent = validate_v2_parse_provenance(
            self.parse_status,
            &self.parser_backend,
            self.worker_lane,
            parse_cache_status,
            self.pdf_classification
                .as_ref()
                .map(|classification| classification.status),
        )
        .is_ok();
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
            || self.parse_attempt_count > 3
            || !execution_consistent
            || !provenance_consistent
            || self
                .pdf_classification
                .as_ref()
                .is_some_and(|audit| audit.validate().is_err())
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
            size_bytes, mtime_ns, last_seen_run_id, last_seen_at_ms,
            source_guard_kind, source_guard_sha256
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(file_identity) DO UPDATE SET
            absolute_path=excluded.absolute_path,
            relative_path=excluded.relative_path,
            file_type=excluded.file_type,
            source_version=excluded.source_version,
            size_bytes=excluded.size_bytes,
            mtime_ns=excluded.mtime_ns,
            last_seen_run_id=excluded.last_seen_run_id,
            last_seen_at_ms=excluded.last_seen_at_ms,
            source_guard_kind=excluded.source_guard_kind,
            source_guard_sha256=excluded.source_guard_sha256",
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
            record.source_guard_kind,
            record.source_guard_sha256,
        ])?;
    }
    Ok(())
}

pub(crate) fn insert_file_results(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    records: &[FileResultRecord],
    snapshot_rows: bool,
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO scan_file_results(
            scan_run_id, file_identity, relative_path, source_version,
            parse_profile_hash, cache_status, cache_miss_reason, parse_status,
            parser_backend, worker_lane, truncated, content_sha256,
            primary_duration_ms, fallback_duration_ms, parse_duration_ms,
            failure_class, fallback_backend, fallback_reason_code,
            error_code, error_message, error_retryable, error_stage,
            error_file_path, error_backend, parse_cache_status
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25
        )",
    )?;
    let mut execution_statement = transaction.prepare_cached(
        "INSERT INTO scan_file_execution_v2(
            scan_run_id, file_identity, parse_transport, parse_attempt_count,
            classification_status, classification_page_count,
            classification_cache_status, classification_cache_miss_reason,
            classification_result_examined_pages, classification_run_inspected_pages,
            classification_nominal_charged_pages, classification_duration_ms,
            classification_transport, classification_attempt_count,
            classifier_build, classifier_profile_hash
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
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
        // spec Part 5.2: a snapshot-hit current row is `snapshot`; otherwise the
        // full_v2 `parse_cache_status` is derived from the record's own
        // execution semantics (never copies a source run's miss/hit).
        let parse_cache_status = if snapshot_rows {
            "snapshot"
        } else {
            parse_cache_status_text(record)
        };
        // spec Part 5.2/5.3: `cache_miss_reason` is only valid when
        // `parse_cache_status=miss`; fresh / snapshot / not_applicable rows must
        // carry an empty reason. A not_applicable row (policy/semantic/runtime
        // NotParsed, classifier failure, no-text metadata) may have performed a
        // lookup (record.cache_status=miss), but the row's v2 parse provenance
        // is not_applicable, so the reason must not leak into the audit.
        let cache_miss_reason = if parse_cache_status == "miss" {
            cache_miss_reason_text(record.cache_miss_reason)
        } else {
            ""
        };
        statement.execute(params![
            scan_run_id,
            record.file_identity,
            record.relative_path,
            record.source_version,
            record.parse_profile_hash,
            cache_status_text(record.cache_status),
            cache_miss_reason,
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
            parse_cache_status,
        ])?;
        let parse_transport = if snapshot_rows {
            "snapshot".to_string()
        } else {
            enum_text(&record.parse_transport)
        };
        let parse_attempt_count = if snapshot_rows {
            0
        } else {
            record.parse_attempt_count as i64
        };
        let classification = record.pdf_classification.as_ref();
        let classification_status = classification.map(|value| enum_text(&value.status));
        let classification_page_count = classification
            .and_then(|value| value.page_count.0)
            .map(|value| value as i64);
        let classification_cache_status = classification
            .map(|value| persisted_classification_cache_status(value, snapshot_rows));
        let classification_cache_miss_reason = classification.map(|value| {
            if snapshot_rows {
                String::new()
            } else {
                value.classification_cache_miss_reason.clone()
            }
        });
        let classification_result_examined_pages = classification
            .and_then(|value| value.result_examined_pages.0)
            .map(|value| value as i64);
        let classification_run_inspected_pages = classification.map(|value| {
            if snapshot_rows {
                Some(0_i64)
            } else {
                value.run_inspected_pages.0.map(|pages| pages as i64)
            }
        }).flatten();
        let classification_nominal_charged_pages = classification
            .map(|value| value.nominal_charged_pages as i64);
        let classification_duration_ms = classification.map(|value| {
            if snapshot_rows {
                0_i64
            } else {
                value.duration_ms as i64
            }
        });
        let classification_transport = classification
            .map(|value| persisted_classification_transport(value, snapshot_rows));
        let classification_attempt_count = classification.map(|value| {
            if snapshot_rows {
                0_i64
            } else {
                value.attempt_count as i64
            }
        });
        execution_statement.execute(params![
            scan_run_id,
            record.file_identity,
            parse_transport,
            parse_attempt_count,
            classification_status,
            classification_page_count,
            classification_cache_status,
            classification_cache_miss_reason,
            classification_result_examined_pages,
            classification_run_inspected_pages,
            classification_nominal_charged_pages,
            classification_duration_ms,
            classification_transport,
            classification_attempt_count,
            classification.map(|value| value.classifier_build.as_str()),
            classification.map(|value| value.classifier_profile_hash.as_str()),
        ])?;
    }
    Ok(())
}

fn persisted_classification_cache_status(
    classification: &PdfClassificationAuditV1,
    snapshot_rows: bool,
) -> String {
    if snapshot_rows
        && classification.status
            != ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
    {
        "snapshot".to_string()
    } else {
        enum_text(&classification.classification_cache_status)
    }
}

fn persisted_classification_transport(
    classification: &PdfClassificationAuditV1,
    snapshot_rows: bool,
) -> String {
    if snapshot_rows
        && classification.status
            != ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
    {
        "snapshot".to_string()
    } else {
        enum_text(&classification.transport)
    }
}

/// spec Part 5.2 current-run `parse_cache_status`: `fresh` for an exact
/// parse-cache hit, `miss` for this round's body parse, `not_applicable` for
/// rows where no body parser ran (policy/semantic/runtime NotParsed, classifier
/// failure, pre-classification reject, no-text metadata-only).
fn parse_cache_status_text(record: &FileResultRecord) -> &'static str {
    match (record.parse_status, record.cache_status) {
        (ParseStatus::Success, CacheStatus::Fresh) => "fresh",
        // spec Part 5.2: a no-text PDF metadata-only draft never ran a body
        // parser, so its v2 parse provenance is not_applicable (not a miss,
        // and the cache_miss_reason stays empty per Part 4).
        (ParseStatus::Success, CacheStatus::Miss) if record.parser_backend == "pdf_metadata_v1" => {
            "not_applicable"
        }
        (ParseStatus::Success, CacheStatus::Miss) => "miss",
        (ParseStatus::Error | ParseStatus::Timeout, _) if record.parser_backend == "not_parsed" => {
            "not_applicable"
        }
        (ParseStatus::Error | ParseStatus::Timeout, _) => "miss",
        (ParseStatus::NotParsed, _) => "not_applicable",
    }
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
        CacheMissReason::ParserIdentityChanged => "parser_identity_changed",
        CacheMissReason::EntryAbsentOrEvicted => "entry_absent_or_evicted",
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

    fn valid_body_parse_result() -> FileResultRecord {
        FileResultRecord {
            file_identity: "fixture:evidence.txt".to_string(),
            relative_path: "evidence.txt".to_string(),
            source_version: "mtime_ns=1:size=1".to_string(),
            parse_profile_hash: "a".repeat(64),
            cache_status: CacheStatus::Miss,
            cache_miss_reason: CacheMissReason::NewFile,
            parse_status: ParseStatus::Success,
            parser_backend: "light_text_v1".to_string(),
            worker_lane: AuditWorkerLane::RustCore,
            truncated: false,
            content_sha256: "b".repeat(64),
            primary_duration_ms: 1,
            fallback_duration_ms: 0,
            parse_duration_ms: 1,
            failure_class: String::new(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ParseTransport::RustInProcess,
            parse_attempt_count: 1,
            pdf_classification: None,
            error: None,
        }
    }

    #[test]
    fn full_v2_persistence_rejects_legacy_miss_reasons_and_impossible_lanes() {
        let valid = valid_body_parse_result();
        valid
            .validate_for_persistence(false)
            .expect("baseline body parse result must persist");

        for reason in [
            CacheMissReason::ErrorCache,
            CacheMissReason::ParserProfileChanged,
        ] {
            let mut legacy = valid.clone();
            legacy.cache_miss_reason = reason;
            assert!(
                legacy.validate_for_persistence(false).is_err(),
                "legacy v1 miss reasons must not enter full-v2 audit rows"
            );
        }

        let mut no_body_parser = valid;
        no_body_parser.parser_backend = "not_parsed".to_string();
        no_body_parser.worker_lane = AuditWorkerLane::NotParsed;
        assert!(
            no_body_parser.validate_for_persistence(false).is_err(),
            "a parse miss must retain an actual body parser provenance"
        );
    }

    #[test]
    fn no_text_pdf_omitted_by_global_budget_is_valid_persistence_provenance() {
        let mut omitted = valid_body_parse_result();
        omitted.parse_status = ParseStatus::NotParsed;
        omitted.parser_backend = "not_parsed".to_string();
        omitted.worker_lane = AuditWorkerLane::NotParsed;
        omitted.primary_duration_ms = 0;
        omitted.parse_duration_ms = 0;
        omitted.parse_transport = ParseTransport::NotApplicable;
        omitted.parse_attempt_count = 0;
        omitted.pdf_classification = Some(PdfClassificationAuditV1 {
            status: ai_daily_scanner_contract::PdfClassificationStatus::NoTextInParseWindow,
            page_count: ai_daily_scanner_contract::Nullable(Some(2)),
            classification_cache_status:
                ai_daily_scanner_contract::ClassificationCacheStatus::Miss,
            classification_cache_miss_reason: "new_file".to_string(),
            result_examined_pages: ai_daily_scanner_contract::Nullable(Some(2)),
            run_inspected_pages: ai_daily_scanner_contract::Nullable(Some(2)),
            nominal_charged_pages: 2,
            duration_ms: 1,
            transport: ai_daily_scanner_contract::ClassificationTransport::OneShot,
            attempt_count: 1,
            classifier_build: "c".repeat(64),
            classifier_profile_hash: "d".repeat(64),
        });

        omitted
            .validate_for_persistence(false)
            .expect("a classified no-text PDF may be omitted when its metadata section exceeds the global context budget");
    }

    #[test]
    fn snapshot_budget_exclusion_preserves_zero_execution_classification_shape() {
        let classification = PdfClassificationAuditV1 {
            status: ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget,
            page_count: ai_daily_scanner_contract::Nullable(None),
            classification_cache_status:
                ai_daily_scanner_contract::ClassificationCacheStatus::NotEligible,
            classification_cache_miss_reason: String::new(),
            result_examined_pages: ai_daily_scanner_contract::Nullable(Some(0)),
            run_inspected_pages: ai_daily_scanner_contract::Nullable(Some(0)),
            nominal_charged_pages: 0,
            duration_ms: 0,
            transport: ai_daily_scanner_contract::ClassificationTransport::NotApplicable,
            attempt_count: 0,
            classifier_build: "a".repeat(64),
            classifier_profile_hash: "b".repeat(64),
        };

        assert_eq!(
            persisted_classification_cache_status(&classification, true),
            "not_eligible"
        );
        assert_eq!(
            persisted_classification_transport(&classification, true),
            "not_applicable"
        );
    }

    #[test]
    fn discovered_inventory_requires_source_size_to_agree() {
        let file = DiscoveredFileOut {
            file_identity: "identity".to_string(),
            path: "C:\\work\\file.txt".to_string(),
            extension: ".txt".to_string(),
            modified_at: "2026-07-16T00:00:00+08:00".to_string(),
            size_bytes: 8,
            source_version: "mtime_ns=123:size=9".to_string(),
            source_guard_kind: None,
            source_guard_sha256: None,
        };

        assert!(InventoryRecord::from_discovered(&file, "file.txt".to_string()).is_err());
    }

    #[test]
    fn discovered_inventory_carries_source_guard() {
        let file = DiscoveredFileOut {
            file_identity: "identity".to_string(),
            path: "C:\\work\\file.txt".to_string(),
            extension: ".txt".to_string(),
            modified_at: "2026-07-16T00:00:00+08:00".to_string(),
            size_bytes: 8,
            source_version: "mtime_ns=123:size=8".to_string(),
            source_guard_kind: Some("content_sha256_v1".to_string()),
            source_guard_sha256: Some("a".repeat(64)),
        };

        let record = InventoryRecord::from_discovered(&file, "file.txt".to_string())
            .expect("guard-carrying discovery must build an inventory record");
        assert_eq!(
            record.source_guard_kind.as_deref(),
            Some("content_sha256_v1")
        );
        assert_eq!(
            record.source_guard_sha256.as_deref(),
            Some("a".repeat(64).as_str())
        );
        record
            .validate()
            .expect("record must satisfy the guard invariant");
    }

    #[test]
    fn discovered_inventory_rejects_invalid_source_guard() {
        let mut file = DiscoveredFileOut {
            file_identity: "identity".to_string(),
            path: "C:\\work\\file.txt".to_string(),
            extension: ".txt".to_string(),
            modified_at: "2026-07-16T00:00:00+08:00".to_string(),
            size_bytes: 8,
            source_version: "mtime_ns=123:size=8".to_string(),
            source_guard_kind: Some("content_sha256_v1".to_string()),
            source_guard_sha256: None,
        };
        assert!(
            InventoryRecord::from_discovered(&file, "file.txt".to_string()).is_err(),
            "available kind without a hash must be rejected"
        );

        file.source_guard_kind = Some("unavailable".to_string());
        file.source_guard_sha256 = Some("a".repeat(64));
        assert!(
            InventoryRecord::from_discovered(&file, "file.txt".to_string()).is_err(),
            "unavailable kind with a hash must be rejected"
        );

        file.source_guard_kind = Some("unavailable".to_string());
        file.source_guard_sha256 = None;
        let record = InventoryRecord::from_discovered(&file, "file.txt".to_string())
            .expect("unavailable guard with a null hash is valid");
        record
            .validate()
            .expect("record must satisfy the guard invariant");
    }
}
