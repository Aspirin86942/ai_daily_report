use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationCacheEntry {
    pub page_audit: Option<ai_daily_scanner_contract::PdfPageAudit>,
    pub status: String,
    pub page_count: u64,
    pub result_examined_pages: u64,
}

fn valid_classification(status: &str, pages: u64, examined: u64) -> bool {
    matches!(status, "text_in_parse_window" | "no_text_in_parse_window")
        && examined > 0
        && examined <= pages
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationCacheMissReason {
    NewFile,
    SourceVersionChanged,
    ClassifierIdentityChanged,
    EntryAbsentOrEvicted,
}

impl ClassificationCacheMissReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewFile => "new_file",
            Self::SourceVersionChanged => "source_version_changed",
            Self::ClassifierIdentityChanged => "classifier_identity_changed",
            Self::EntryAbsentOrEvicted => "entry_absent_or_evicted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassificationCacheLookup {
    Fresh(ClassificationCacheEntry),
    PdfOpenError(crate::parsers::classifier::PdfOpenFailure),
    Miss(ClassificationCacheMissReason),
}

impl crate::store::ScannerStore {
    /// Only the latest committed observation of this file can supply a failure.
    /// Retiring its run also retires the proof under the existing audit limits.
    pub(crate) fn pdf_open_failure(
        &self,
        identity: &str,
        key: &str,
        profile_hash: &str,
        classifier_build: &str,
    ) -> Result<Option<crate::parsers::classifier::PdfOpenFailure>, crate::store::StoreError> {
        use crate::parsers::classifier::PdfOpenFailure;
        let code: Option<String> = self
            .connection
            .query_row(
                "SELECT f.error_code FROM (
                    SELECT scan_run_id, error_code, error_retryable FROM scan_file_results
                    WHERE file_identity=?1 ORDER BY rowid DESC LIMIT 1
                ) f JOIN scan_runs r USING (scan_run_id)
                JOIN scan_file_execution x ON x.scan_run_id=f.scan_run_id AND x.file_identity=?1
                JOIN json_each(r.final_envelope_metadata_json, '$.pdf_open_failures') proof
                WHERE f.error_retryable=0 AND x.classification_status='error'
                  AND x.classifier_profile_hash=?3 AND x.classifier_build=?4
                  AND proof.key=?1 AND proof.value=?2",
                params![identity, key, profile_hash, classifier_build],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| crate::store::cache_lookup_read_error(error.into()))?;
        match code.as_deref() {
            None => Ok(None),
            Some("PARSER_FAILED") => Ok(Some(PdfOpenFailure::InvalidFormat)),
            Some("PDF_PROTECTED") => Ok(Some(PdfOpenFailure::PasswordRequired)),
            Some(_) => Err(crate::store::StoreError::RunCorrupt(
                "invalid cached PDF open error code".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationCacheWriteRecord {
    pub file_identity: String,
    pub source_version: String,
    pub source_guard_kind: String,
    pub source_guard_sha256: String,
    pub classifier_profile_hash: String,
    pub classifier_build: String,
    pub status: String,
    pub page_count: u64,
    pub result_examined_pages: u64,
}

impl ClassificationCacheWriteRecord {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !valid_cache_source(
            &self.file_identity,
            &self.source_version,
            &self.source_guard_kind,
            &self.source_guard_sha256,
        ) || !crate::store::inventory::is_sha256(&self.classifier_profile_hash)
            || !crate::store::inventory::is_sha256(&self.classifier_build)
            || !valid_classification(&self.status, self.page_count, self.result_examined_pages)
        {
            return Err("invalid classification cache record".to_string());
        }
        Ok(())
    }
}

/// `classifier_profile_hash` canonical payload（spec Part 3.2）：policy version
/// + `pdf_max_pages` + `pdf_classification_timeout_ms`；不含全局 quota/session。
#[derive(Serialize)]
struct ClassifierProfileFingerprint<'a> {
    policy_version: &'a str,
    pdf_max_pages: u64,
    pdf_classification_timeout_ms: u64,
}

pub fn classifier_profile_hash(profile: &NormalizedScannerSettings) -> Result<String, String> {
    profile.validate()?;
    let input = ClassifierProfileFingerprint {
        policy_version: &profile.classifier_policy_version,
        pdf_max_pages: profile.parse.pdf.max_pages,
        pdf_classification_timeout_ms: profile.pdf_classification_timeout_ms,
    };
    let canonical_json = serde_json::to_vec(&input).map_err(|error| error.to_string())?;
    Ok(domain_hash(CLASSIFIER_DOMAIN, &canonical_json))
}

/// 类型化 classification cache lookup。key 固定为
/// `file_identity + source_version + SourceGuardV2 + classifier_profile_hash +
/// classifier_build`（spec Part 3.2）。`inventory_existed_before` 表示
/// `prepare_inventory` 报告该 inventory 在本轮前已存在，用于区分
/// `entry_absent_or_evicted` 与 `new_file`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn lookup_classification_cache(
    connection: &Connection,
    file_identity: &str,
    source_version: &str,
    source_guard_kind: &str,
    source_guard_sha256: &str,
    classifier_profile_hash: &str,
    classifier_build: &str,
    inventory_existed_before: bool,
) -> Result<ClassificationCacheLookup, CacheLookupReadError> {
    let key = [
        file_identity,
        source_version,
        source_guard_kind,
        source_guard_sha256,
        classifier_profile_hash,
        classifier_build,
    ];
    let fresh = connection
        .query_row(
            "SELECT status, page_count, result_examined_pages
             FROM classification_cache
             WHERE file_identity=?1 AND source_version=?2
               AND source_guard_kind=?3 AND source_guard_sha256=?4
               AND classifier_profile_hash=?5 AND classifier_build=?6",
            key,
            |row| {
                Ok(ClassificationCacheEntry {
                    page_audit: None,
                    status: row.get(0)?,
                    page_count: row.get::<_, i64>(1)? as u64,
                    result_examined_pages: row.get::<_, i64>(2)? as u64,
                })
            },
        )
        .optional()?;
    if let Some(mut entry) = fresh {
        if !valid_classification(&entry.status, entry.page_count, entry.result_examined_pages) {
            return Err(CacheLookupReadError::Corrupt(
                "exact classification cache row failed integrity validation",
            ));
        }
        // The schema-3 cache row stays unchanged. Recover optional observations
        // from the originating run, bound to every immutable cache key field.
        let metadata: Option<String> = connection.query_row(
            "SELECT r.final_envelope_metadata_json FROM scan_file_execution x
             JOIN scan_runs r ON r.scan_run_id=x.scan_run_id
             JOIN context_runs c ON c.scan_run_id=r.scan_run_id
             JOIN context_artifact_files a ON a.artifact_id=c.artifact_id AND a.file_identity=x.file_identity
             WHERE x.file_identity=?1 AND a.source_version=?2
               AND a.source_guard_kind=?3 AND a.source_guard_sha256=?4
               AND x.classifier_profile_hash=?5 AND x.classifier_build=?6
               AND x.classification_cache_status='miss'
               AND r.final_envelope_metadata_json IS NOT NULL
             ORDER BY r.scan_run_id DESC LIMIT 1",
            key, |row| row.get(0)).optional()?.flatten();
        if let Some(metadata) = metadata {
            let value: serde_json::Value = serde_json::from_str(&metadata)
                .map_err(|_| CacheLookupReadError::Corrupt("invalid classifier page metadata"))?;
            if let Some(pages) = value
                .get("pdf_page_audits")
                .and_then(|v| v.get(file_identity))
            {
                let audit: ai_daily_scanner_contract::PdfPageAudit =
                    serde_json::from_value(pages.clone()).map_err(|_| {
                        CacheLookupReadError::Corrupt("invalid cached classifier page audit")
                    })?;
                if audit.validate().is_err()
                    || audit.checked_pages.len() as u64 != entry.result_examined_pages
                    || audit
                        .checked_pages
                        .iter()
                        .any(|page| *page > entry.page_count)
                {
                    return Err(CacheLookupReadError::Corrupt(
                        "cached page audit differs from classification result",
                    ));
                }
                entry.page_audit = Some(audit);
            }
        }
        return Ok(ClassificationCacheLookup::Fresh(entry));
    }

    let source = [
        file_identity,
        source_version,
        source_guard_kind,
        source_guard_sha256,
    ];
    let reason = match TABLE.cached_source(connection, source)? {
        CachedSource::Same => ClassificationCacheMissReason::ClassifierIdentityChanged,
        CachedSource::Changed => ClassificationCacheMissReason::SourceVersionChanged,
        CachedSource::Absent if inventory_existed_before => {
            ClassificationCacheMissReason::EntryAbsentOrEvicted
        }
        CachedSource::Absent => ClassificationCacheMissReason::NewFile,
    };
    Ok(ClassificationCacheLookup::Miss(reason))
}

/// 只写成功 text/no-text 缓存（spec Part 3.2：无负缓存）。entry_size_bytes
/// 是本 Store 计算的逻辑 payload 字节数；`last_accessed_bucket` 使用当前 UTC
/// 日期，`generation_rank` 默认 1。超过 128 MiB 硬上限时在同一 work-phase
/// cache transaction 内按唯一 tuple 淘汰；单 entry 超 cap → skipped。
#[cfg(test)]
pub(crate) fn write_success_classification_cache(
    transaction: &Transaction<'_>,
    cached_at_ms: i64,
    records: &[ClassificationCacheWriteRecord],
) -> rusqlite::Result<()> {
    write_success_classification_cache_with_check(transaction, cached_at_ms, records, &|| true)
        .map_err(CacheMutationError::into_sqlite)
}

pub(crate) fn write_success_classification_cache_with_check(
    transaction: &Transaction<'_>,
    cached_at_ms: i64,
    records: &[ClassificationCacheWriteRecord],
    deadline_open: &dyn Fn() -> bool,
) -> Result<(), CacheMutationError> {
    let bucket = date_bucket_for_ms(cached_at_ms);
    ensure_mutation_deadline(deadline_open)?;
    let mut statement = transaction.prepare_cached(
        "INSERT INTO classification_cache(
            file_identity, source_version, source_guard_kind, source_guard_sha256,
            classifier_profile_hash, classifier_build, status, page_count,
            result_examined_pages, cached_at_ms, entry_size_bytes, generation_rank,
            last_accessed_bucket
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12)
         ON CONFLICT(file_identity, source_version, source_guard_kind, source_guard_sha256,
                     classifier_profile_hash, classifier_build) DO UPDATE SET
            status=excluded.status,
            page_count=excluded.page_count,
            result_examined_pages=excluded.result_examined_pages,
            cached_at_ms=excluded.cached_at_ms,
            entry_size_bytes=excluded.entry_size_bytes,
            last_accessed_bucket=excluded.last_accessed_bucket",
    )?;
    for record in records {
        ensure_mutation_deadline(deadline_open)?;
        let entry_size = classification_entry_size_bytes(record);
        if entry_size > CLASSIFICATION_CACHE_MAX_BYTES {
            return Err(CacheMutationError::Record(format!(
                "classification cache entry exceeds the {CLASSIFICATION_CACHE_MAX_BYTES} byte hard cap"
            )));
        }
        statement.execute(params![
            record.file_identity,
            record.source_version,
            record.source_guard_kind,
            record.source_guard_sha256,
            record.classifier_profile_hash,
            record.classifier_build,
            record.status,
            record.page_count as i64,
            record.result_examined_pages as i64,
            cached_at_ms,
            entry_size,
            bucket,
        ])?;
    }
    TABLE.evict(
        transaction,
        CLASSIFICATION_CACHE_MAX_BYTES,
        [],
        deadline_open,
    )
}

/// 按 spec Part 4 eviction tuple 升序删除 classification cache 行，直到
/// `sum(entry_size_bytes) <= cap`。tuple 前缀为
/// `(generation_rank, result_examined_pages, last_accessed_bucket, cached_at_ms,
/// primary_key_bytes)`。
#[cfg(test)]
pub(super) fn evict_classification_cache(
    transaction: &Transaction<'_>,
    cap: i64,
) -> rusqlite::Result<()> {
    TABLE
        .evict(transaction, cap, [], &|| true)
        .map_err(CacheMutationError::into_sqlite)
}

pub(super) const TABLE: CacheTable = CacheTable {
    name: "classification_cache",
    select_victim: "SELECT file_identity, source_version, source_guard_kind, source_guard_sha256,
                        classifier_profile_hash, classifier_build, entry_size_bytes
                 FROM classification_cache
                 ORDER BY generation_rank ASC,
                          result_examined_pages ASC,
                          last_accessed_bucket ASC,
                          cached_at_ms ASC,
                          (length(file_identity) + length(source_version) +
                           length(source_guard_kind) + length(source_guard_sha256) +
                           length(classifier_profile_hash) + length(classifier_build)) ASC
                 LIMIT 1",
    delete_victim: "DELETE FROM classification_cache
             WHERE file_identity=?1 AND source_version=?2 AND source_guard_kind=?3
               AND source_guard_sha256=?4 AND classifier_profile_hash=?5 AND classifier_build=?6",
};

fn classification_entry_size_bytes(record: &ClassificationCacheWriteRecord) -> i64 {
    let text_bytes: usize = [
        record.file_identity.as_str(),
        record.source_version.as_str(),
        record.source_guard_kind.as_str(),
        record.source_guard_sha256.as_str(),
        record.classifier_profile_hash.as_str(),
        record.classifier_build.as_str(),
        record.status.as_str(),
    ]
    .iter()
    .map(|value| value.len())
    .sum();
    (text_bytes + 8 + 8) as i64
}
