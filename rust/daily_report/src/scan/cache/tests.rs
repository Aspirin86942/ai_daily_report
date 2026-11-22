use super::*;
use daily_parsers::contract::{
    ContentUnit, ContentUnitKind, ParsedContentAudit, ParserBackend, PdfPageObservation,
    SourceRange, SourceRangeUnit,
};

fn parsed_value<'a>(hits: &'a CacheHits, key: &CacheKey) -> &'a ParseValue {
    match hits.get(key).unwrap() {
        CacheValue::Parsed(value) => value,
        CacheValue::Negative(_) => panic!("expected a parsed result"),
    }
}

const TODAY: i64 = 1_800_000_000_000;
const TOMORROW: i64 = TODAY + 86_400_000;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn open(dir: &Path) -> ScanCache {
    ScanCache::open(dir, deadline()).unwrap()
}

fn key(file_id: &str) -> CacheKey {
    CacheKey {
        file_id: file_id.to_owned(),
        source_guard: "source-guard".into(),
        profile_hash: "a".repeat(64),
    }
}

fn window() -> PdfWindow {
    PdfWindow {
        max_pages: 3,
        policy_version: "pdf_text_presence_v2".into(),
        extract_max_pages: 2,
        selected_pages: None,
    }
}

fn observation(checked: &[u64], last_has_text: bool) -> PdfObservation {
    PdfObservation {
        page_count: 5,
        pages: PdfPageAudit {
            policy_version: window().policy_version,
            checked_pages: checked.to_vec(),
            observations: Some(
                checked
                    .iter()
                    .enumerate()
                    .map(|(index, page)| PdfPageObservation {
                        page: *page,
                        has_text: last_has_text && index + 1 == checked.len(),
                        has_image: None,
                    })
                    .collect(),
            ),
        },
    }
}

fn parsed(dir: &Path, key: CacheKey, content: &str, pdf: bool) -> CachedParse {
    let (backend, extension, unit, start) = if pdf {
        (
            ParserBackend::RustPdfiumV3,
            ".pdf",
            SourceRangeUnit::PdfPage,
            2,
        )
    } else {
        (
            ParserBackend::RustOfficeOxideV2,
            ".docx",
            SourceRangeUnit::Paragraph,
            1,
        )
    };
    let range = SourceRange::new(unit, start, start + 1, None);
    let chars = content.chars().count() as u64;
    CachedParse {
        result: ParseResult {
            file_path: dir
                .join(format!("{}{extension}", key.file_id))
                .to_string_lossy()
                .into_owned(),
            file_type: extension.into(),
            content: content.into(),
            evidence_audit: Some(ParsedContentAudit {
                read_ranges: Some(vec![range.clone()]),
                content_units: Some(vec![ContentUnit {
                    unit_id: "u1".into(),
                    kind: ContentUnitKind::Text,
                    text_start: 0,
                    text_end: chars,
                    ranges: vec![range],
                }]),
                usable_chars: Some(chars),
                ..ParsedContentAudit::default()
            }),
            parser_backend: backend,
            worker_lane: backend.lane(),
            truncated: false,
            duration_ms: 3,
            observed_source_version: key.source_guard.clone(),
        },
        query: CacheQuery {
            key,
            pdf_window: pdf.then(window),
        },
        source_sha256: "b".repeat(64),
        pdf_observation: pdf.then(|| observation(&[1, 2], true)),
    }
}

fn negative(key: CacheKey, evidence: PdfNegative) -> CachedNegative {
    CachedNegative {
        query: CacheQuery {
            key,
            pdf_window: Some(window()),
        },
        evidence,
    }
}

fn no_text() -> PdfNegative {
    PdfNegative::NoText(observation(&[1, 2, 5], false))
}

fn native_error(code: &str, message: &str, retryable: bool) -> WorkerDiagnostic {
    WorkerDiagnostic {
        error_code: code.into(),
        message: message.into(),
        retryable,
        stage: "parse".into(),
        file_path: None,
        backend: Some("rust_pdfium_v3".into()),
    }
}

fn snapshot(references: Vec<CacheReference>) -> CacheSnapshot {
    CacheSnapshot {
        key: "c".repeat(64),
        manifest: [
            1u64.to_le_bytes().as_slice(),
            br#"{"ordered":[1,2],"complete":true}"#,
        ]
        .concat(),
        references,
    }
}

fn lookup_snapshot(
    cache: &mut ScanCache,
    snapshot: &CacheSnapshot,
    hits: &CacheHits,
) -> Result<Option<u64>, ReportError> {
    let Some(stored) = cache.read_snapshot(&snapshot.key)? else {
        return Ok(None);
    };
    let expected = snapshot.references.iter().map(|reference| {
        (
            reference.key(),
            matches!(reference, CacheReference::Parse(_)),
        )
    });
    if stored.manifest.get(8..) == snapshot.manifest.get(8..)
        && cache.accept_snapshot(&snapshot.key, &stored, hits, expected)?
    {
        Ok(Some(stored.origin))
    } else {
        Ok(None)
    }
}

fn direct(dir: &Path) -> Connection {
    let connection = Connection::open(dir.join(CACHE_FILENAME)).unwrap();
    connection
        .set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )
        .unwrap();
    connection
}

fn wal_commits(dir: &Path) -> usize {
    let wal = fs::read(dir.join(format!("{CACHE_FILENAME}-wal"))).unwrap();
    assert!(wal.len() >= 32);
    let number = |bytes: &[u8]| u32::from_be_bytes(bytes.try_into().unwrap());
    assert!(matches!(number(&wal[..4]), 0x377f_0682 | 0x377f_0683));
    let size = number(&wal[8..12]) as usize;
    wal[32..]
        .chunks_exact(24 + size)
        .filter(|frame| frame[8..16] == wal[16..24] && number(&frame[4..8]) != 0)
        .count()
}

#[test]
fn actual_wal_commits_cold_same_day_and_cross_day() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let office = parsed(dir, key("office"), "Amount 123", false);
    let pdf = parsed(dir, key("pdf"), "Page 2", true);
    let blank = negative(key("blank"), no_text());
    let bad = negative(
        key("bad"),
        PdfNegative::Damaged(native_error("PARSER_FAILED", "FPDF_ERR_FORMAT", false)),
    );
    let protected = negative(
        key("password"),
        PdfNegative::PasswordProtected(native_error("PDF_PROTECTED", "FPDF_ERR_PASSWORD", false)),
    );
    let queries = [
        office.query.clone(),
        pdf.query.clone(),
        blank.query.clone(),
        bad.query.clone(),
        protected.query.clone(),
    ];
    let manifest = snapshot(vec![
        CacheReference::Parse(office.query.key.clone()),
        CacheReference::Negative(blank.query.key.clone()),
        CacheReference::Parse(pdf.query.key.clone()),
        CacheReference::Negative(bad.query.key.clone()),
        CacheReference::Negative(protected.query.key.clone()),
    ]);
    let first = open(dir);
    assert!(first.lookup(&queries).unwrap().entries.is_empty());
    first
        .commit_at(
            CacheBatch {
                parsed: vec![office, pdf],
                negatives: vec![blank, bad, protected],
                snapshot: Some(manifest.clone()),
                ..CacheBatch::default()
            },
            TODAY,
        )
        .unwrap();
    assert_eq!(wal_commits(dir), 1);
    ScanCache::check(dir, deadline()).unwrap();

    let mut warm = open(dir);
    assert!(warm
        .connection
        .as_ref()
        .unwrap()
        .is_readonly("main")
        .unwrap());
    let mut hits = warm.lookup(&queries[..2]).unwrap();
    hits.extend(warm.lookup(&queries[2..]).unwrap());
    assert_eq!(hits.entries.values().filter(|hit| hit.parsed).count(), 2);
    assert_eq!(hits.entries.values().filter(|hit| !hit.parsed).count(), 3);
    assert_eq!(
        parsed_value(&hits, &key("office")).source_sha256,
        "b".repeat(64)
    );
    assert_eq!(
        parsed_value(&hits, &key("pdf"))
            .observation
            .clone()
            .unwrap(),
        observation(&[1, 2], true)
    );
    assert_eq!(
        lookup_snapshot(&mut warm, &manifest, &hits).unwrap(),
        Some(1)
    );
    let mut partial = manifest.clone();
    partial.references.pop();
    assert_eq!(lookup_snapshot(&mut warm, &partial, &hits).unwrap(), None);
    warm.commit_at(
        CacheBatch {
            hits,
            ..CacheBatch::default()
        },
        TODAY,
    )
    .unwrap();
    assert_eq!(wal_commits(dir), 1);

    let mut next = open(dir);
    let hits = next.lookup(&queries).unwrap();
    assert_eq!(
        lookup_snapshot(&mut next, &manifest, &hits).unwrap(),
        Some(1)
    );
    next.commit_at(
        CacheBatch {
            hits,
            ..CacheBatch::default()
        },
        TOMORROW,
    )
    .unwrap();
    assert_eq!(wal_commits(dir), 2);
    let sql = direct(dir);
    for table in ["parse_cache", "pdf_negative_cache", "scan_snapshots"] {
        let minimum: i64 = sql
            .query_row(
                &format!("SELECT min(last_access_day) FROM {table}"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(minimum, TOMORROW.div_euclid(86_400_000));
    }
}

#[test]
fn scanner_build_context_uses_real_cache_and_rechecks_replaced_source() {
    use crate::scan::{ScanRequest, Scanner, ScannerConfig};
    use daily_parsers::settings::EngineStatus;
    use std::{fs::FileTimes, time::UNIX_EPOCH};

    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let cache = temp.path().join("cache");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&cache).unwrap();
    let report = cache.join("reports.sqlite3");
    fs::write(&report, b"separate synthetic report store").unwrap();
    let file = work.join("review.md");
    let json = work.join("status.json");
    let stamp = UNIX_EPOCH + Duration::new(1_795_113_000, 123_456_700);
    let write = |path: &Path, text: &str| {
        fs::write(path, text).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(stamp))
            .unwrap();
    };
    write(&file, "# Review\nApprove amount 125 first.\n");
    write(&json, r#"{"state":"pending"}"#);
    let scanner = Scanner::open(ScannerConfig {
        work_dir: work.clone(),
        scan_db_path: cache.join(CACHE_FILENAME),
        scanner_settings: serde_json::from_value(serde_json::json!({"max_workers": 1})).unwrap(),
        pdf_worker_path: temp.path().join("unused.exe"),
    })
    .unwrap();
    let utc = chrono::DateTime::<chrono::Utc>::from(stamp);
    let target = utc.with_timezone(&crate::types::shanghai()).date_naive();
    let request = ScanRequest {
        start_date: target,
        end_date: target,
        utc_window: None,
    };

    let cold = scanner.build_context(&request).unwrap();
    assert_eq!(cold.status, EngineStatus::Ok, "{:?}", cold.warnings);
    assert_eq!(cold.stats.parse_attempt_count, 2);
    let sql = direct(&cache);
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM parse_cache WHERE content_text <> ''",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(wal_commits(&cache), 1);
    let expected_modified =
        chrono::DateTime::<chrono::Utc>::from(fs::metadata(&file).unwrap().modified().unwrap());
    let report_source = |run: &crate::scan::ScanResult| {
        let source = run
            .files
            .iter()
            .find(|source| source.relative_path == "review.md")
            .unwrap();
        assert_eq!(source.modified_at_utc, expected_modified.to_rfc3339());
        (source.excerpt.text.clone(), source.raw_sha256.clone())
    };

    let warm = scanner.build_context(&request).unwrap();
    let metrics = &warm.stats;
    assert!(metrics.snapshot_hit);
    assert_eq!(metrics.parse_attempt_count, 0);
    assert_eq!(report_source(&warm), report_source(&cold));
    assert_eq!(wal_commits(&cache), 1);

    write(&file, "# Review\nApprove amount 225 first.\n");
    let updated = scanner.build_context(&request).unwrap();
    assert_eq!(updated.stats.parse_attempt_count, 1);
    let (text, source_sha256) = report_source(&updated);
    assert!(text.contains("225"));
    assert_ne!(source_sha256, report_source(&warm).1);
    assert_eq!(wal_commits(&cache), 2);
    assert_eq!(
        fs::read(report).unwrap(),
        b"separate synthetic report store"
    );
}

#[test]
fn all_hit_without_snapshot_stays_read_only_across_batch_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let records: Vec<_> = (0..520)
        .map(|index| parsed(dir, key(&index.to_string()), "bounded", false))
        .collect();
    let queries: Vec<_> = records.iter().map(|item| item.query.clone()).collect();
    open(dir)
        .commit_at(
            CacheBatch {
                parsed: records,
                ..CacheBatch::default()
            },
            TODAY,
        )
        .unwrap();
    assert_eq!(wal_commits(dir), 1);
    let mut warm = open(dir);
    let hits = warm.lookup(&queries).unwrap();
    assert_eq!(hits.entries.values().filter(|hit| hit.parsed).count(), 520);
    let snapshot = snapshot(vec![CacheReference::Parse(queries[0].key.clone())]);
    assert_eq!(lookup_snapshot(&mut warm, &snapshot, &hits).unwrap(), None);
    warm.commit_at(
        CacheBatch {
            hits,
            snapshot: Some(snapshot),
            ..CacheBatch::default()
        },
        TODAY,
    )
    .unwrap();
    assert_eq!(wal_commits(dir), 1);
    assert_eq!(
        direct(dir)
            .query_row("SELECT count(*) FROM scan_snapshots", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn identities_windows_and_corrupt_rows_are_misses() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let blank = negative(key("blank"), no_text());
    let item = parsed(dir, key("parse"), "Extracted text", false);
    let queries = [blank.query.clone(), item.query.clone()];
    let manifest = snapshot(vec![
        CacheReference::Parse(item.query.key.clone()),
        CacheReference::Negative(blank.query.key.clone()),
    ]);
    open(dir)
        .commit_at(
            CacheBatch {
                parsed: vec![item],
                negatives: vec![blank],
                snapshot: Some(manifest.clone()),
                ..CacheBatch::default()
            },
            TODAY,
        )
        .unwrap();

    let mut warm = open(dir);
    let mut different = queries.clone();
    different[0].pdf_window.as_mut().unwrap().max_pages += 1;
    different[1].key.source_guard += "-changed";
    let hits = warm.lookup(&different).unwrap();
    assert!(hits.entries.is_empty());
    different[1].key.source_guard = queries[1].key.source_guard.clone();
    different[1].key.profile_hash = "d".repeat(64);
    let hits = warm.lookup(&different).unwrap();
    assert!(hits.entries.is_empty());
    assert_eq!(lookup_snapshot(&mut warm, &manifest, &hits).unwrap(), None);
    drop(warm);

    let sql = direct(dir);
    sql.execute("UPDATE parse_cache SET content_sha256=?1", ["d".repeat(64)])
        .unwrap();
    sql.execute("UPDATE pdf_negative_cache SET payload=x'00'", [])
        .unwrap();
    drop(sql);
    let mut warm = open(dir);
    let hits = warm.lookup(&queries).unwrap();
    assert!(hits.entries.is_empty());
    assert_eq!(lookup_snapshot(&mut warm, &manifest, &hits).unwrap(), None);
}

#[test]
fn rejects_incomplete_observations_bad_ranges_and_unverified_references_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    for evidence in [
        PdfNegative::NoText(observation(&[1, 2], false)),
        PdfNegative::NoText(observation(&[1, 2, 5], true)),
        PdfNegative::Damaged(native_error("PARSER_TIMEOUT", "timed out", true)),
        PdfNegative::PasswordProtected(native_error("PDF_PROTECTED", "read failed", false)),
    ] {
        let error = open(dir)
            .commit_at(
                CacheBatch {
                    parsed: vec![parsed(dir, key("good"), "valid", false)],
                    negatives: vec![negative(key("bad"), evidence)],
                    ..CacheBatch::default()
                },
                TODAY,
            )
            .unwrap_err();
        assert_eq!(error.code, "SCAN_CACHE_INVALID");
        assert!(!dir.join(CACHE_FILENAME).exists());
    }
    let mut item = parsed(dir, key("pdf"), "Two", true);
    item.result
        .evidence_audit
        .as_mut()
        .unwrap()
        .read_ranges
        .as_mut()
        .unwrap()[0] = SourceRange::new(SourceRangeUnit::PdfPage, 3, 4, None);
    assert_eq!(
        open(dir)
            .commit_at(
                CacheBatch {
                    parsed: vec![item],
                    ..CacheBatch::default()
                },
                TODAY
            )
            .unwrap_err()
            .code,
        "SCAN_CACHE_INVALID"
    );
    let wrong = snapshot(vec![CacheReference::Parse(key("never-cached"))]);
    assert_eq!(
        open(dir)
            .commit_at(
                CacheBatch {
                    parsed: vec![parsed(dir, key("good"), "valid", false)],
                    snapshot: Some(wrong),
                    ..CacheBatch::default()
                },
                TODAY
            )
            .unwrap_err()
            .code,
        "SCAN_CACHE_INVALID"
    );
    assert!(!dir.join(CACHE_FILENAME).exists());
}

#[test]
fn replaces_stale_reference_even_when_key_is_reused_and_cleanup_prunes_it() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let item = parsed(dir, key("p"), "first", false);
    let query = item.query.clone();
    let manifest = snapshot(vec![CacheReference::Parse(query.key.clone())]);
    open(dir)
        .commit_at(
            CacheBatch {
                parsed: vec![item],
                snapshot: Some(manifest.clone()),
                ..CacheBatch::default()
            },
            TODAY,
        )
        .unwrap();

    let replacement = parsed(dir, query.key.clone(), "replacement", false);
    open(dir)
        .commit_at(
            CacheBatch {
                parsed: vec![replacement],
                ..CacheBatch::default()
            },
            TODAY + 100,
        )
        .unwrap();
    let mut cache = open(dir);
    let hits = cache.lookup(&[query]).unwrap();
    assert_eq!(parsed_value(&hits, &key("p")).result.content, "replacement");
    assert_eq!(lookup_snapshot(&mut cache, &manifest, &hits).unwrap(), None);
    cache.cleanup().unwrap();
    assert_eq!(
        direct(dir)
            .query_row("SELECT count(*) FROM scan_snapshots", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn does_not_modify_existing_incompatible_database() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let path = dir.join(CACHE_FILENAME);
    let old = Connection::open(&path).unwrap();
    old.execute_batch("PRAGMA user_version=3; CREATE TABLE preserved(value TEXT); INSERT INTO preserved VALUES ('do not change');").unwrap();
    drop(old);
    let before = fs::read(&path).unwrap();
    let error = ScanCache::open(dir, deadline()).err().unwrap();
    assert_eq!(error.code, "SCAN_CACHE_SCHEMA");
    assert_eq!(
        ScanCache::check(dir, deadline()).unwrap_err().code,
        "SCAN_CACHE_SCHEMA"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!dir.join(format!("{CACHE_FILENAME}-wal")).exists());

    let missing = dir.join("missing");
    ScanCache::check(&missing, deadline()).unwrap();
    assert!(!missing.exists());
}

#[test]
fn matching_version_and_application_id_do_not_bypass_schema_check() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    open(dir).initialize().unwrap();
    let sql = direct(dir);
    sql.execute("DROP INDEX parse_cache_written", []).unwrap();
    let before = fs::read(dir.join(format!("{CACHE_FILENAME}-wal"))).unwrap();
    assert_eq!(verify_schema(&sql).unwrap_err().code, "SCAN_CACHE_SCHEMA");
    assert_eq!(
        ScanCache::open(dir, deadline()).err().unwrap().code,
        "SCAN_CACHE_SCHEMA"
    );
    assert_eq!(
        fs::read(dir.join(format!("{CACHE_FILENAME}-wal"))).unwrap(),
        before
    );
}

#[test]
fn initializes_empty_schema_only_on_explicit_request() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    open(dir).commit_at(CacheBatch::default(), TODAY).unwrap();
    assert!(!dir.join(CACHE_FILENAME).exists());
    open(dir).initialize().unwrap();
    let sql = direct(dir);
    verify_schema(&sql).unwrap();
    let data: (i64, i64) = sql.query_row(
        "SELECT count(*),sum(strict AND wr) FROM pragma_table_list WHERE name NOT GLOB 'sqlite_*'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(data, (3, 3));
    assert_eq!(wal_commits(dir), 1);
}

#[test]
fn evicts_by_access_day_under_combined_database_and_wal_budget() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let a = parsed(dir, key("older"), &"a".repeat(52_000), false);
    let b = parsed(dir, key("newer"), &"b".repeat(52_000), false);
    let q1 = a.query.clone();
    let q2 = b.query.clone();
    let manifest = snapshot(vec![CacheReference::Parse(b.query.key.clone())]);
    open(dir)
        .commit_at(
            CacheBatch {
                parsed: vec![b, a],
                snapshot: Some(manifest),
                ..CacheBatch::default()
            },
            TODAY,
        )
        .unwrap();
    let warm = open(dir);
    let hits = warm.lookup(std::slice::from_ref(&q1)).unwrap();
    warm.commit_at(
        CacheBatch {
            hits,
            ..CacheBatch::default()
        },
        TOMORROW,
    )
    .unwrap();

    let mut cache = open(dir);
    cache.max_bytes = 155_000;
    cache.cleanup().unwrap();
    let hits = cache.lookup(&[q1, q2]).unwrap();
    assert_eq!(hits.entries.keys().collect::<Vec<_>>(), vec![&key("older")]);
    assert!(storage::disk_len(&cache.path).unwrap() <= cache.max_bytes);
    assert_eq!(
        direct(dir)
            .query_row("SELECT count(*) FROM scan_snapshots", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn expired_holder_and_results_from_released_lock_cannot_commit() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let holder = open(dir);
    let hits = holder.lookup(&[]).unwrap();
    drop(holder);
    assert_eq!(
        open(dir)
            .commit_at(
                CacheBatch {
                    hits,
                    parsed: vec![parsed(dir, key("stale"), "stale", false)],
                    ..CacheBatch::default()
                },
                TODAY
            )
            .unwrap_err()
            .code,
        "SCAN_CACHE_INVALID"
    );

    let limit = Instant::now() + Duration::from_millis(100);
    let expired = ScanCache::open(dir, limit).unwrap();
    std::thread::sleep(limit.saturating_duration_since(Instant::now()) + Duration::from_millis(5));
    assert_eq!(
        expired
            .commit_at(
                CacheBatch {
                    parsed: vec![parsed(dir, key("expired"), "expired", false)],
                    ..CacheBatch::default()
                },
                TODAY
            )
            .unwrap_err()
            .code,
        "SCAN_CACHE_DEADLINE"
    );
    assert!(!dir.join(CACHE_FILENAME).exists());
    open(dir).initialize().unwrap();
}

#[test]
fn sqlite_writer_wait_uses_remaining_deadline_and_commits_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    open(dir).initialize().unwrap();
    let other = direct(dir);
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    let cache = ScanCache::open(dir, start + Duration::from_millis(180)).unwrap();
    let error = cache
        .commit_at(
            CacheBatch {
                parsed: vec![parsed(dir, key("blocked"), "pending", false)],
                ..CacheBatch::default()
            },
            TODAY,
        )
        .unwrap_err();
    assert!(matches!(
        error.code,
        "SCAN_CACHE_BUSY" | "SCAN_CACHE_DEADLINE"
    ));
    assert!(
        start.elapsed() >= Duration::from_millis(150) && start.elapsed() < Duration::from_secs(2)
    );
    assert_eq!(wal_commits(dir), 1);
    other.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        other
            .query_row("SELECT count(*) FROM parse_cache", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

const CHILD_ENV: &str = "AI_DAILY_SCAN_CACHE_TEST_CHILD";

#[test]
fn cache_lock_child() {
    let Some(dir) = std::env::var_os(CHILD_ENV).map(PathBuf::from) else {
        return;
    };
    let cache = open(&dir);
    assert!(cache.connection.is_some());
    fs::write(dir.join("ready"), b"ready").unwrap();
    let timeout = deadline();
    while Instant::now() < timeout {
        if dir.join("exit-request").exists() {
            // Exit while owning the OS lock, without running Rust destructors.
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("parent did not release independent process");
}

#[test]
fn independent_process_lock_counts_wait_and_recovers_without_deleting_lock_file() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    open(dir).initialize().unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg("scan::cache::tests::cache_lock_child")
        .env(CHILD_ENV, dir)
        .stdout(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().unwrap();
    let timeout = deadline();
    while !dir.join("ready").exists() && Instant::now() < timeout {
        assert!(child.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(dir.join("ready").exists());
    let start = Instant::now();
    let error = ScanCache::open(dir, start + Duration::from_millis(150))
        .err()
        .unwrap();
    assert_eq!(error.code, "REPORT_ALREADY_RUNNING");
    assert!(
        start.elapsed() >= Duration::from_millis(120) && start.elapsed() < Duration::from_secs(2)
    );
    fs::write(dir.join("exit-request"), b"exit").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(fs::read_dir(dir).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".lock")));
    let cache = open(dir);
    assert!(cache.connection.is_some());
}
