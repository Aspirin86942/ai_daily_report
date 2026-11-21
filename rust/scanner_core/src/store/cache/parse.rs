use super::*;

pub(crate) fn lookup_cache(
    connection: &Connection,
    file_identity: &str,
    source_version: &str,
    source_guard_kind: &str,
    source_guard_sha256: &str,
    parse_profile_hash: &str,
    inventory_existed_before: bool,
) -> rusqlite::Result<CacheLookup> {
    let fresh = connection
        .query_row(
            "SELECT content, content_sha256, parser_backend, worker_lane, truncated,
                    worker_contract_version, worker_version, worker_build
             FROM parse_cache
             WHERE file_identity=?1 AND source_version=?2
               AND source_guard_kind=?3 AND source_guard_sha256=?4
               AND parse_profile_hash=?5",
            params![
                file_identity,
                source_version,
                source_guard_kind,
                source_guard_sha256,
                parse_profile_hash
            ],
            |row| {
                Ok(CacheEntry {
                    evidence_audit: None,
                    content: row.get(0)?,
                    content_sha256: row.get(1)?,
                    parser_backend: row.get(2)?,
                    worker_lane: row.get(3)?,
                    truncated: row.get::<_, i64>(4)? != 0,
                    worker_contract_version: row.get(5)?,
                    worker_version: row.get(6)?,
                    worker_build: row.get(7)?,
                })
            },
        )
        .optional()?;
    if let Some(entry) = fresh {
        return Ok(if entry.is_valid() {
            CacheLookup::Fresh(entry)
        } else {
            CacheLookup::Miss(CacheMissReason::ErrorCache)
        });
    }
    let source = [
        file_identity,
        source_version,
        source_guard_kind,
        source_guard_sha256,
    ];
    let reason = match TABLE.cached_source(connection, source)? {
        CachedSource::Same => CacheMissReason::ParserIdentityChanged,
        CachedSource::Changed => CacheMissReason::SourceVersionChanged,
        CachedSource::Absent if inventory_existed_before => CacheMissReason::EntryAbsentOrEvicted,
        CachedSource::Absent => CacheMissReason::NewFile,
    };
    Ok(CacheLookup::Miss(reason))
}

/// spec Part 4 parse eviction `recompute_rank`：light_text=1、xlsx=2、office=2、
/// sharepoint=3、python_office=4、pdf=10。新 parser backend 未升级 retention
/// policy 时 fail closed 为 profile route invariant（返回 None）。
pub(crate) fn recompute_rank(parser_backend: &str) -> Option<i64> {
    match parser_backend {
        "light_text_v2" => Some(1),
        "rust_xlsx_bounded_v2" => Some(2),
        "rust_office_oxide_v2" => Some(2),
        "rust_legacy_office_v3" => Some(3),
        "rust_xls_calamine_v3" => Some(4),
        "rust_pdfium_v3" | crate::parsers::document::OCR_BACKEND => Some(10),
        _ => None,
    }
}

/// parse cache row 的逻辑 payload 字节数（spec Part 4：Store 计算，不接受 wire
/// 输入；本实现覆盖全部 text 列 + truncated 的 SQLite i64 表示）。
#[cfg(test)]
pub(crate) fn parse_entry_size_bytes(record: &CacheWriteRecord) -> i64 {
    stored_entry_size(record, &stored_content(record).0)
}

fn stored_entry_size(record: &CacheWriteRecord, stored: &str) -> i64 {
    let text_bytes: usize = [
        record.file_identity.as_str(),
        record.source_version.as_str(),
        record.source_guard_kind.as_str(),
        record.source_guard_sha256.as_str(),
        record.parse_profile_hash.as_str(),
        stored,
        record.content_sha256.as_str(),
        record.parser_backend.as_str(),
        record.worker_lane.as_str(),
        record.worker_contract_version.as_str(),
        record.worker_version.as_str(),
        record.worker_build.as_str(),
    ]
    .iter()
    .map(|value| value.len())
    .sum();
    (text_bytes + 8) as i64
}

#[cfg(test)]
pub(crate) fn write_success_cache(
    transaction: &Transaction<'_>,
    cached_at_ms: i64,
    records: &[CacheWriteRecord],
) -> rusqlite::Result<()> {
    write_success_cache_with_check(transaction, cached_at_ms, records, &|| true)
        .map_err(CacheMutationError::into_sqlite)
}

pub(crate) fn write_success_cache_with_check(
    transaction: &Transaction<'_>,
    cached_at_ms: i64,
    records: &[CacheWriteRecord],
    deadline_open: &dyn Fn() -> bool,
) -> Result<(), CacheMutationError> {
    let bucket = date_bucket_for_ms(cached_at_ms);
    ensure_mutation_deadline(deadline_open)?;
    let mut statement = transaction.prepare_cached(
        "INSERT INTO parse_cache(
            file_identity, source_version, source_guard_kind, source_guard_sha256,
            parse_profile_hash, content, content_sha256, parser_backend, worker_lane,
            truncated, worker_contract_version, worker_version, worker_build, cached_at_ms,
            entry_size_bytes, generation_rank, last_accessed_bucket
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 1, ?16)
         ON CONFLICT(file_identity, source_version, source_guard_kind, source_guard_sha256,
                     parse_profile_hash) DO UPDATE SET
            content=excluded.content,
            content_sha256=excluded.content_sha256,
            parser_backend=excluded.parser_backend,
            worker_lane=excluded.worker_lane,
            truncated=excluded.truncated,
            worker_contract_version=excluded.worker_contract_version,
            worker_version=excluded.worker_version,
            worker_build=excluded.worker_build,
            cached_at_ms=excluded.cached_at_ms,
            entry_size_bytes=excluded.entry_size_bytes,
            last_accessed_bucket=excluded.last_accessed_bucket",
    )?;
    for record in records {
        ensure_mutation_deadline(deadline_open)?;
        // spec Part 4: 新 parser backend 未升级 retention policy → fail closed。
        if recompute_rank(&record.parser_backend).is_none() {
            return Err(over_cap_error(format!(
                "parse cache backend '{}' has no retention recompute_rank",
                record.parser_backend
            ))
            .into());
        }
        let (stored, stored_sha256) = stored_content(record);
        let entry_size = stored_entry_size(record, &stored);
        if entry_size > PARSE_CACHE_MAX_BYTES {
            // 单 entry 大于载体 cap → skipped receipt warning。
            return Err(over_cap_error(format!(
                "parse cache entry exceeds the {PARSE_CACHE_MAX_BYTES} byte hard cap"
            ))
            .into());
        }
        statement.execute(params![
            record.file_identity,
            record.source_version,
            record.source_guard_kind,
            record.source_guard_sha256,
            record.parse_profile_hash,
            stored,
            stored_sha256,
            record.parser_backend,
            record.worker_lane,
            i64::from(record.truncated),
            record.worker_contract_version,
            record.worker_version,
            record.worker_build,
            cached_at_ms,
            entry_size,
            bucket,
        ])?;
    }
    // 同一 work-phase cache transaction 内按唯一 tuple 淘汰到硬上限以下。
    TABLE.evict(
        transaction,
        PARSE_CACHE_MAX_BYTES,
        [crate::parsers::document::OCR_BACKEND],
        deadline_open,
    )
}

/// 按 spec Part 4 eviction tuple 升序删除 parse cache 行，直到
/// `sum(entry_size_bytes) <= cap`。`recompute_rank` 由 `parser_backend` 推导；
/// 未知 backend 排最后（防御：正常写入已 fail closed，因此只能来自历史数据）。
#[cfg(test)]
pub(super) fn evict_parse_cache(transaction: &Transaction<'_>, cap: i64) -> rusqlite::Result<()> {
    TABLE
        .evict(
            transaction,
            cap,
            [crate::parsers::document::OCR_BACKEND],
            &|| true,
        )
        .map_err(CacheMutationError::into_sqlite)
}

pub(super) const TABLE: CacheTable = CacheTable {
    name: "parse_cache",
    select_victim: "SELECT file_identity, source_version, source_guard_kind, source_guard_sha256,
                        parse_profile_hash, entry_size_bytes
                 FROM parse_cache
                 ORDER BY generation_rank ASC,
                          CASE parser_backend
                              WHEN 'light_text_v2' THEN 1
                              WHEN 'rust_xlsx_bounded_v2' THEN 2
                              WHEN 'rust_office_oxide_v2' THEN 2
                              WHEN 'rust_legacy_office_v3' THEN 3
                              WHEN 'rust_xls_calamine_v3' THEN 4
                              WHEN 'rust_pdfium_v3' THEN 10
                              WHEN ?1 THEN 10
                              ELSE 99
                          END ASC,
                          last_accessed_bucket ASC,
                          cached_at_ms ASC,
                          (length(file_identity) + length(source_version) +
                           length(source_guard_kind) + length(source_guard_sha256) +
                           length(parse_profile_hash)) ASC
                 LIMIT 1",
    delete_victim: "DELETE FROM parse_cache
             WHERE file_identity=?1 AND source_version=?2 AND source_guard_kind=?3
               AND source_guard_sha256=?4 AND parse_profile_hash=?5",
};
