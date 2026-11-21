use super::*;
use crate::config::normalize_scanner_settings;
use crate::store::schema::V3_DDL;
use ai_daily_scanner_contract::{ReportMode, ScannerSettings};

fn raw_profile() -> ScannerSettings {
    serde_json::from_value(serde_json::json!({})).expect("minimal settings")
}

fn normalized_settings(mode: ReportMode) -> NormalizedScannerSettings {
    let raw: ScannerSettings =
        serde_json::from_value(serde_json::json!({})).expect("minimal settings");
    normalize_scanner_settings(&raw, mode).expect("normalized settings")
}

#[test]
fn parse_profile_hash_changes_for_every_content_semantic_input() {
    let profile =
        normalize_scanner_settings(&raw_profile(), ReportMode::Daily).expect("normalized profile");
    let route = RouteStackFingerprint::text("build-a").unwrap();
    let base = parse_profile_hash(1, &route, &profile).expect("hash");

    let mut timeout = profile.clone();
    timeout.execution.file_timeout_ms += 1_000;
    let mut extension_timeout = profile.clone();
    extension_timeout
        .execution
        .file_timeout_by_extension_ms
        .insert(".txt".to_string(), 2_000);
    let mut guard = profile.clone();
    guard.execution.max_file_size_bytes += 1;
    let mut parse = profile.clone();
    parse.parse.text.max_chars += 1;
    let mut encoding = profile.clone();
    encoding.parse.text.csv_encodings.push("gb18030".into());
    let mut backend = profile.clone();
    backend.parse.office.primary_backend = "different_office_backend_v1".to_string();
    let mut legacy = profile.clone();
    legacy.parse.office.legacy_extensions_enabled = true;
    let mut pdf = profile.clone();
    pdf.parse.pdf.max_pages += 1;
    let changed_route = RouteStackFingerprint::text("build-b").unwrap();
    let changed_backend_route =
        RouteStackFingerprint::modern_office("build-a", "ai_daily_worker_v3", "office-build")
            .unwrap();

    assert_ne!(base, parse_profile_hash(1, &route, &timeout).unwrap());
    assert_ne!(
        base,
        parse_profile_hash(1, &route, &extension_timeout).unwrap()
    );
    assert_ne!(base, parse_profile_hash(1, &route, &guard).unwrap());
    assert_ne!(base, parse_profile_hash(1, &route, &parse).unwrap());
    assert_ne!(base, parse_profile_hash(1, &route, &encoding).unwrap());
    assert_ne!(base, parse_profile_hash(1, &route, &backend).unwrap());
    assert_ne!(base, parse_profile_hash(1, &route, &legacy).unwrap());
    assert_ne!(base, parse_profile_hash(1, &route, &pdf).unwrap());
    assert_ne!(
        base,
        parse_profile_hash(1, &changed_route, &profile).unwrap()
    );
    assert_ne!(
        base,
        parse_profile_hash(1, &changed_backend_route, &profile).unwrap()
    );
    assert!(parse_profile_hash(2, &route, &profile).is_err());
}

#[test]
fn parse_profile_fingerprint_includes_compiled_parser_content_identity() {
    let profile =
        normalize_scanner_settings(&raw_profile(), ReportMode::Daily).expect("normalized profile");
    let route = RouteStackFingerprint::text("worker-build").unwrap();
    let fingerprint: serde_json::Value = serde_json::from_slice(
        &parse_profile_fingerprint_json(1, &route, &profile).expect("fingerprint JSON"),
    )
    .expect("fingerprint value");

    assert_eq!(
        fingerprint["core_build_identity"],
        crate::PARSER_CONTENT_IDENTITY
    );
}

#[test]
fn route_stack_hash_includes_each_applicable_worker_contract_and_build() {
    let profile = normalized_settings(ReportMode::Daily);
    for make in [
        RouteStackFingerprint::modern_office,
        RouteStackFingerprint::pdf,
    ] {
        let base = make("engine", "ai_daily_worker_v3", "worker").unwrap();
        let hash = parse_profile_hash(1, &base, &profile).unwrap();
        for (engine, contract, worker) in [
            ("engine-2", "ai_daily_worker_v3", "worker"),
            ("engine", "changed-contract", "worker"),
            ("engine", "ai_daily_worker_v3", "changed-worker"),
        ] {
            assert_ne!(
                hash,
                parse_profile_hash(1, &make(engine, contract, worker).unwrap(), &profile).unwrap()
            );
        }
    }
}

// -----------------------------------------------------------------------
// classification cache（spec Part 3.2/4）
// -----------------------------------------------------------------------

fn fresh_v2_db() -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_in_memory().expect("in-memory db");
    connection.execute_batch(V3_DDL).expect("v3 schema builds");
    connection
}

fn insert_inventory(connection: &Connection, file_identity: &str, source_version: &str) {
    connection
        .execute(
            "INSERT INTO file_inventory(
                file_identity, absolute_path, relative_path, file_type,
                source_version, size_bytes, mtime_ns, last_seen_at_ms
             ) VALUES (?1, ?2, ?3, '.pdf', ?4, 1, 1, 1)",
            params![
                file_identity,
                format!("C:\\{file_identity}.pdf"),
                format!("{file_identity}.pdf"),
                source_version
            ],
        )
        .expect("inventory row inserts");
}

fn cache_record(
    file_identity: &str,
    source_version: &str,
    status: &str,
    classifier_build: &str,
) -> ClassificationCacheWriteRecord {
    ClassificationCacheWriteRecord {
        file_identity: file_identity.to_string(),
        source_version: source_version.to_string(),
        source_guard_kind: "windows_file_id_change_time_v1".to_string(),
        source_guard_sha256: "b".repeat(64),
        classifier_profile_hash: "c".repeat(64),
        classifier_build: classifier_build.to_string(),
        status: status.to_string(),
        page_count: 5,
        result_examined_pages: 5,
    }
}

#[test]
fn classifier_profile_hash_changes_for_every_classification_semantic_input() {
    let profile = normalized_settings(ReportMode::Daily);
    let base = classifier_profile_hash(&profile).expect("hash");

    let mut timeout = profile.clone();
    timeout.pdf_classification_timeout_ms += 1_000;
    let mut pages = profile.clone();
    pages.parse.pdf.max_pages += 1;
    let report_mode = normalized_settings(ReportMode::Monthly);

    assert_ne!(base, classifier_profile_hash(&timeout).unwrap());
    assert_ne!(base, classifier_profile_hash(&pages).unwrap());
    assert_ne!(base, classifier_profile_hash(&report_mode).unwrap());
    // 同一 profile 稳定
    assert_eq!(base, classifier_profile_hash(&profile).unwrap());
    // 与全局 quota/session 无关
    let mut quota = profile.clone();
    quota.max_total_pdf_classification_pages += 1;
    let mut session = profile.clone();
    session.worker_max_requests += 1;
    let mut deadline = profile.clone();
    deadline.total_deadline_ms += 1_000;
    assert_eq!(base, classifier_profile_hash(&quota).unwrap());
    assert_eq!(base, classifier_profile_hash(&session).unwrap());
    assert_eq!(base, classifier_profile_hash(&deadline).unwrap());
}

#[test]
fn classification_cache_write_then_lookup_is_fresh() {
    let mut connection = fresh_v2_db();
    insert_inventory(&connection, "fixture:a", "mtime_ns=1:size=2");
    let transaction = connection.transaction().expect("tx");
    write_success_classification_cache(
        &transaction,
        1_000,
        &[cache_record(
            "fixture:a",
            "mtime_ns=1:size=2",
            "text_in_parse_window",
            &"a".repeat(64),
        )],
    )
    .expect("classification cache write");
    transaction.commit().expect("commit");

    let lookup = lookup_classification_cache(
        &connection,
        "fixture:a",
        "mtime_ns=1:size=2",
        "windows_file_id_change_time_v1",
        &"b".repeat(64),
        &"c".repeat(64),
        &"a".repeat(64),
        false,
    )
    .expect("lookup");
    match lookup {
        ClassificationCacheLookup::Fresh(entry) => {
            assert_eq!(entry.status, "text_in_parse_window");
            assert_eq!(entry.page_count, 5);
            assert_eq!(entry.result_examined_pages, 5);
        }
        other => panic!("expected fresh, got {other:?}"),
    }
}

#[test]
fn classification_cache_miss_reason_tree_is_frozen() {
    let mut connection = fresh_v2_db();
    insert_inventory(&connection, "fixture:a", "mtime_ns=1:size=2");
    insert_inventory(&connection, "fixture:b", "mtime_ns=5:size=6");
    let transaction = connection.transaction().expect("tx");
    write_success_classification_cache(
        &transaction,
        1_000,
        &[cache_record(
            "fixture:a",
            "mtime_ns=1:size=2",
            "no_text_in_parse_window",
            &"a".repeat(64),
        )],
    )
    .expect("classification cache write");
    transaction.commit().expect("commit");

    // 同一 identity+source_version+guard，classifier build 变化
    let lookup = lookup_classification_cache(
        &connection,
        "fixture:a",
        "mtime_ns=1:size=2",
        "windows_file_id_change_time_v1",
        &"b".repeat(64),
        &"c".repeat(64),
        &"d".repeat(64),
        false,
    )
    .expect("lookup");
    assert_eq!(
        lookup,
        ClassificationCacheLookup::Miss(ClassificationCacheMissReason::ClassifierIdentityChanged)
    );

    // 同一 file_identity，source_version 变化
    let lookup = lookup_classification_cache(
        &connection,
        "fixture:a",
        "mtime_ns=9:size=2",
        "windows_file_id_change_time_v1",
        &"b".repeat(64),
        &"c".repeat(64),
        &"a".repeat(64),
        false,
    )
    .expect("lookup");
    assert_eq!(
        lookup,
        ClassificationCacheLookup::Miss(ClassificationCacheMissReason::SourceVersionChanged)
    );

    // 无任何缓存行的新文件
    let lookup = lookup_classification_cache(
        &connection,
        "fixture:b",
        "mtime_ns=5:size=6",
        "windows_file_id_change_time_v1",
        &"b".repeat(64),
        &"c".repeat(64),
        &"a".repeat(64),
        false,
    )
    .expect("lookup");
    assert_eq!(
        lookup,
        ClassificationCacheLookup::Miss(ClassificationCacheMissReason::NewFile)
    );

    // inventory 本轮前已存在 -> entry_absent_or_evicted
    let lookup = lookup_classification_cache(
        &connection,
        "fixture:b",
        "mtime_ns=5:size=6",
        "windows_file_id_change_time_v1",
        &"b".repeat(64),
        &"c".repeat(64),
        &"a".repeat(64),
        true,
    )
    .expect("lookup");
    assert_eq!(
        lookup,
        ClassificationCacheLookup::Miss(ClassificationCacheMissReason::EntryAbsentOrEvicted)
    );
}

#[test]
fn classification_cache_has_no_negative_cache() {
    // unknown/error 永不写分类缓存
    assert!(
        cache_record("fixture:a", "mtime_ns=1:size=2", "unknown", &"a".repeat(64))
            .validate()
            .is_err()
    );
    assert!(
        cache_record("fixture:a", "mtime_ns=1:size=2", "error", &"a".repeat(64))
            .validate()
            .is_err()
    );
    assert!(cache_record(
        "fixture:a",
        "mtime_ns=1:size=2",
        "text_in_parse_window",
        &"a".repeat(64)
    )
    .validate()
    .is_ok());
}

// -----------------------------------------------------------------------
// cache_retention_v1：parse cache 硬上限 + eviction tuple（spec Part 4）
// -----------------------------------------------------------------------

fn parse_record(identity: &str, source_version: &str, profile_hash: &str) -> CacheWriteRecord {
    CacheWriteRecord {
        evidence_audit: None,
        cache_payload_version: 0,
        file_identity: identity.to_string(),
        source_version: source_version.to_string(),
        source_guard_kind: "content_sha256_v1".to_string(),
        source_guard_sha256: "0".repeat(64),
        parse_profile_hash: profile_hash.to_string(),
        content: format!("content-{identity}"),
        content_sha256: sha256_hex(format!("content-{identity}").as_bytes()),
        parser_backend: "light_text_v2".to_string(),
        worker_lane: "rust_core".to_string(),
        truncated: false,
        worker_contract_version: "ai_daily_context_v1".to_string(),
        worker_version: crate::engine_version().to_string(),
        worker_build: crate::ENGINE_BUILD_IDENTITY.to_string(),
    }
}

#[test]
fn parse_cache_evicts_oldest_by_tuple_to_respect_the_cap() {
    let mut connection = fresh_v2_db();
    for (identity, version) in [
        ("fixture:a", "mtime_ns=1:size=2"),
        ("fixture:b", "mtime_ns=5:size=6"),
        ("fixture:c", "mtime_ns=9:size=10"),
    ] {
        insert_inventory(&connection, identity, version);
    }
    let transaction = connection.transaction().expect("tx");
    // 三条 record 的 entry_size 各约数百字节，cap=1000 应只保留最新一条。
    write_success_cache(
        &transaction,
        100,
        &[parse_record(
            "fixture:a",
            "mtime_ns=1:size=2",
            "a".repeat(64).as_str(),
        )],
    )
    .expect("write a");
    write_success_cache(
        &transaction,
        200,
        &[parse_record(
            "fixture:b",
            "mtime_ns=5:size=6",
            "b".repeat(64).as_str(),
        )],
    )
    .expect("write b");
    write_success_cache(
        &transaction,
        300,
        &[parse_record(
            "fixture:c",
            "mtime_ns=9:size=10",
            "c".repeat(64).as_str(),
        )],
    )
    .expect("write c");
    // 直接用小 cap 触发 eviction；tuple 前缀相同 → 最老 cached_at_ms 先删。
    evict_parse_cache(&transaction, 1_000).expect("evict");
    let remaining: i64 = transaction
        .query_row("SELECT count(*) FROM parse_cache", [], |row| row.get(0))
        .expect("count");
    let total: i64 = transaction
        .query_row(
            "SELECT COALESCE(SUM(entry_size_bytes), 0) FROM parse_cache",
            [],
            |row| row.get(0),
        )
        .expect("sum");
    assert!(
        (1..=2).contains(&remaining),
        "cap eviction must leave 1-2 rows"
    );
    assert!(total <= 1_000, "cap eviction must keep total <= cap");
    let newest_identity: String = transaction
        .query_row(
            "SELECT file_identity FROM parse_cache ORDER BY cached_at_ms DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("newest");
    assert_eq!(newest_identity, "fixture:c");
    transaction.commit().expect("commit");
}

#[test]
fn parse_cache_single_entry_over_cap_is_skipped() {
    let mut connection = fresh_v2_db();
    insert_inventory(&connection, "fixture:a", "mtime_ns=1:size=2");
    let transaction = connection.transaction().expect("tx");
    let mut record = parse_record("fixture:a", "mtime_ns=1:size=2", &"a".repeat(64));
    record.content = "x".repeat(PARSE_CACHE_MAX_BYTES as usize + 1);
    record.content_sha256 = sha256_hex(record.content.as_bytes());
    let error = write_success_cache(&transaction, 100, &[record]).expect_err("over cap");
    assert!(
        error.to_string().contains("hard cap"),
        "over-cap entry must be skipped with a cap warning, got {error}"
    );
    transaction.commit().expect("commit");
}

#[test]
fn parse_cache_unknown_backend_fails_closed() {
    let mut connection = fresh_v2_db();
    insert_inventory(&connection, "fixture:a", "mtime_ns=1:size=2");
    let transaction = connection.transaction().expect("tx");
    let mut record = parse_record("fixture:a", "mtime_ns=1:size=2", &"a".repeat(64));
    record.parser_backend = "future_backend_v9".to_string();
    assert!(
        write_success_cache(&transaction, 100, &[record]).is_err(),
        "new backend without a retention recompute_rank must fail closed"
    );
    transaction.commit().expect("commit");
}

#[test]
fn classification_cache_evicts_least_recently_examined_first() {
    let mut connection = fresh_v2_db();
    for (identity, version) in [
        ("fixture:a", "mtime_ns=1:size=2"),
        ("fixture:b", "mtime_ns=5:size=6"),
    ] {
        insert_inventory(&connection, identity, version);
    }
    let transaction = connection.transaction().expect("tx");
    let mut few_pages = cache_record(
        "fixture:a",
        "mtime_ns=1:size=2",
        "text_in_parse_window",
        &"a".repeat(64),
    );
    few_pages.result_examined_pages = 1;
    let many_pages = cache_record(
        "fixture:b",
        "mtime_ns=5:size=6",
        "no_text_in_parse_window",
        &"a".repeat(64),
    );
    write_success_classification_cache(&transaction, 100, &[few_pages]).expect("write a");
    write_success_classification_cache(&transaction, 200, &[many_pages]).expect("write b");
    // cap=500：只容纳一条 → result_examined_pages 更少的 a 先淘汰。
    evict_classification_cache(&transaction, 500).expect("evict");
    let remaining_identity: String = transaction
        .query_row(
            "SELECT file_identity FROM classification_cache",
            [],
            |row| row.get(0),
        )
        .expect("remaining");
    assert_eq!(remaining_identity, "fixture:b");
    transaction.commit().expect("commit");
}

#[test]
fn date_bucket_for_ms_is_utc_date() {
    // 2020-01-01T00:00:00Z = 1577836800s；2020-01-02 边界 = +86400s。
    assert_eq!(date_bucket_for_ms(1_577_836_800_000), "2020-01-01");
    assert_eq!(date_bucket_for_ms(1_577_923_200_000 - 1), "2020-01-01");
    assert_eq!(date_bucket_for_ms(1_577_923_200_000), "2020-01-02");
}

#[test]
fn snapshot_cache_batch_uses_every_key_field_and_authenticates_each_entry_once() {
    use ai_daily_worker_contract::{
        AuditTextBuilder, ContentUnitKind, SourceRange, SourceRangeUnit,
    };

    let mut connection = fresh_v2_db();
    let version = "mtime_ns=1:size=2";
    let profile = "a".repeat(64);
    let records = ["fixture:b", "fixture:a"].map(|identity| {
        insert_inventory(&connection, identity, version);
        let mut record = parse_record(identity, version, &profile);
        let mut builder = AuditTextBuilder::new(200, "text");
        let range = SourceRange::new(SourceRangeUnit::TextChar, 0, identity.len() as u64, None);
        builder.read(range.clone());
        builder.push(identity, ContentUnitKind::Paragraph, vec![range]);
        let (text, audit, _) = builder.finish();
        record.content_sha256 = sha256_hex(text.as_bytes());
        record.content = text;
        record.cache_payload_version = 1;
        record.evidence_audit = Some(audit);
        record
    });
    let transaction = connection.transaction().unwrap();
    write_success_cache(&transaction, 1, &records).unwrap();
    transaction.commit().unwrap();
    let keys = records.each_ref().map(|record| {
        [
            record.file_identity.as_str(),
            record.source_version.as_str(),
            record.source_guard_kind.as_str(),
            record.source_guard_sha256.as_str(),
            record.parse_profile_hash.as_str(),
        ]
    });

    TEST_OUTER_CACHE_VALIDATIONS.with(|count| count.set(0));
    let entries = lookup_snapshot_cache(&connection, &keys).unwrap();
    assert_eq!(
        TEST_OUTER_CACHE_VALIDATIONS.with(|count| count.get()),
        records.len()
    );
    for record in &records {
        let entry = &entries[&record.file_identity];
        assert_eq!(entry.content_sha256, record.content_sha256);
        assert_eq!(entry.content, record.content);
        assert_eq!(entry.evidence_audit, record.evidence_audit);
    }

    let other_guard = "b".repeat(64);
    let other_profile = "c".repeat(64);
    for (column, other_value) in [
        "fixture:missing",
        "mtime_ns=2:size=2",
        "windows_file_id_change_time_v1",
        &other_guard,
        &other_profile,
    ]
    .into_iter()
    .enumerate()
    {
        let mut different = keys;
        different[0][column] = other_value;
        assert!(
            lookup_snapshot_cache(&connection, &different).is_none(),
            "key column {column}"
        );
    }

    for record in &records {
        let (stored, outer_digest) = stored_content(record);
        // Appending whitespace preserves the decoded JSON and invalidates only its outer digest.
        connection
            .execute(
                "UPDATE parse_cache SET content = content || ' ' WHERE file_identity = ?1",
                [&record.file_identity],
            )
            .unwrap();
        assert!(lookup_snapshot_cache(&connection, &keys).is_none());
        let invalid_inner = stored.replace(&record.content_sha256, &other_guard);
        connection
            .execute(
                "UPDATE parse_cache SET content = ?1, content_sha256 = ?2 WHERE file_identity = ?3",
                params![
                    invalid_inner,
                    sha256_hex(invalid_inner.as_bytes()),
                    record.file_identity
                ],
            )
            .unwrap();
        assert!(lookup_snapshot_cache(&connection, &keys).is_none());
        connection
            .execute(
                "UPDATE parse_cache SET content = ?1, content_sha256 = ?2 WHERE file_identity = ?3",
                params![stored, outer_digest, record.file_identity],
            )
            .unwrap();
    }
}

#[test]
fn versioned_payload_preserves_positions_and_counts_storage_bytes() {
    use ai_daily_worker_contract::{
        AuditTextBuilder, ContentUnitKind, SourceRange, SourceRangeUnit,
    };
    let mut record = parse_record("fixture:a", "mtime_ns=1:size=2", &"a".repeat(64));
    let mut builder = AuditTextBuilder::new(200, "xlsx");
    let range = SourceRange::new(SourceRangeUnit::SheetRow, 7, 8, Some("amounts".into()));
    builder.read(range.clone());
    builder.push("1234.56 CNY", ContentUnitKind::TableRow, vec![range]);
    let (text, audit, _) = builder.finish();
    record.content = text.clone();
    record.content_sha256 = sha256_hex(text.as_bytes());
    record.cache_payload_version = 1;
    record.evidence_audit = Some(audit.clone());
    let (stored, stored_sha256) = stored_content(&record);
    assert!(parse_entry_size_bytes(&record) > text.len() as i64 + 256);
    let entry = CacheEntry {
        content: stored.clone(),
        content_sha256: stored_sha256,
        evidence_audit: None,
        parser_backend: record.parser_backend,
        worker_lane: record.worker_lane,
        truncated: false,
        worker_contract_version: record.worker_contract_version,
        worker_version: record.worker_version,
        worker_build: record.worker_build,
    };
    let CacheLookup::Fresh(decoded) = CacheLookup::Fresh(entry.clone()).decode_payload(1) else {
        panic!("valid cache");
    };
    assert_eq!(decoded.content, text);
    assert_eq!(decoded.content_sha256, sha256_hex(text.as_bytes()));
    assert_eq!(decoded.evidence_audit, Some(audit));
    let mut damaged_outer = entry.clone();
    damaged_outer.content.push(' ');
    assert_eq!(
        CacheLookup::Fresh(damaged_outer).decode_payload(1),
        CacheLookup::Miss(CacheMissReason::EntryAbsentOrEvicted)
    );
    let mut damaged: serde_json::Value = serde_json::from_str(&stored).unwrap();
    damaged.as_object_mut().unwrap().remove("evidence_audit");
    let mut lost_metadata = entry.clone();
    lost_metadata.content = damaged.to_string();
    lost_metadata.content_sha256 = sha256_hex(lost_metadata.content.as_bytes());
    assert!(matches!(
        CacheLookup::Fresh(lost_metadata).decode_payload(1),
        CacheLookup::Miss(_)
    ));
    let mut invalid_logical_hash = entry.clone();
    invalid_logical_hash.content = stored.replace("1234.56", "9999.99");
    invalid_logical_hash.content_sha256 = sha256_hex(invalid_logical_hash.content.as_bytes());
    assert!(matches!(
        CacheLookup::Fresh(invalid_logical_hash).decode_payload(1),
        CacheLookup::Miss(_)
    ));
    let CacheLookup::Fresh(legacy_business_json) = CacheLookup::Fresh(entry).decode_payload(0)
    else {
        panic!("legacy plaintext");
    };
    assert_eq!(legacy_business_json.content, stored);
    assert!(legacy_business_json.evidence_audit.is_none());
}
