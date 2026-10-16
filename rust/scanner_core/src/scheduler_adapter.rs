//! Production adapters that wire the [`crate::scheduler::BudgetedContextScheduler`]
//! to the real store / parser / classifier / worker binaries (spec Solution:
//! parser/classifier executor and `CachePort` are local replaceable adapters).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    Diagnostic, DiagnosticStage, ErrorCode, NormalizedScannerSettings, Nullable, ParseStatus,
    ParseTransport,
};

use crate::budget_model::RouteKind;
use crate::classifier::ParserRoute;
use crate::parsers::light_text;
use crate::scheduler::{
    CachePort, CachePortError, ParseLookupOutcome, ParseRequest, ParseResult, ParserPort,
    RealClock, RunDeadlines,
};
use crate::store::{
    CacheWriteRecord, ClassificationCacheLookup, ClassificationCacheWriteRecord, InventoryRecord,
    RouteStackFingerprints, ScannerStore, StoreError,
};

/// Store-backed [`CachePort`]. One lazily validated connection is reused for
/// this run's cache operations; each write still commits its own transaction.
/// The mutex keeps SQLite access serialized independently of worker concurrency.
pub struct StoreCachePort {
    db_path: PathBuf,
    store: Mutex<Option<ScannerStore>>,
    route_stacks: RouteStackFingerprints,
    settings: NormalizedScannerSettings,
    deadlines: RunDeadlines,
    clock: RealClock,
}

impl StoreCachePort {
    pub fn new(
        db_path: PathBuf,
        route_stacks: RouteStackFingerprints,
        settings: NormalizedScannerSettings,
        deadlines: RunDeadlines,
        clock: RealClock,
    ) -> Self {
        Self {
            db_path,
            store: Mutex::new(None),
            route_stacks,
            settings,
            deadlines,
            clock,
        }
    }

    fn with_store<T>(
        &self,
        operation: impl FnOnce(&mut ScannerStore) -> Result<T, StoreError>,
    ) -> Result<T, CachePortError> {
        let mut slot = self.store.lock().map_err(|_| CachePortError::Store {
            detail: "cache connection lock is poisoned".to_string(),
        })?;
        if slot.is_none() {
            *slot = Some(ScannerStore::open_existing(&self.db_path).map_err(map_store_error)?);
        }
        let result = operation(slot.as_mut().expect("cache connection initialized"));
        // A failed operation must not carry connection-local failure state into
        // the next request. Successful operations retain the prepared statements.
        if result.is_err() {
            *slot = None;
        }
        result.map_err(map_store_error)
    }
}

fn map_store_error(error: StoreError) -> CachePortError {
    match error {
        StoreError::WorkDeadlineExhausted => CachePortError::DeadlineExhausted {
            detail: error.to_string(),
        },
        _ => CachePortError::Store {
            detail: error.to_string(),
        },
    }
}

impl CachePort for StoreCachePort {
    fn prepare_inventory(
        &self,
        scan_run_id: u64,
        now_ms: u64,
        records: &[InventoryRecord],
    ) -> Result<HashSet<String>, CachePortError> {
        let run_id = i64::try_from(scan_run_id).map_err(|_| CachePortError::InvalidKey {
            detail: "scan_run_id exceeds SQLite integer range".to_string(),
        })?;
        self.with_store(|store| {
            store.prepare_inventory_with_deadline(
                records,
                run_id,
                now_ms,
                self.deadlines,
                &self.clock,
            )
        })
    }

    fn lookup_parse(
        &self,
        file: &DiscoveredFileOut,
        route: RouteKind,
        inventory_existed_before: bool,
    ) -> Result<ParseLookupOutcome, CachePortError> {
        let parser_route = parser_route(route);
        let profile_hash = crate::store::cache::parse_profile_hash(
            1,
            self.route_stacks.for_route(parser_route),
            &self.settings,
        )
        .map_err(|message| CachePortError::InvalidKey { detail: message })?;
        let guard_kind =
            file.source_guard_kind
                .as_deref()
                .ok_or_else(|| CachePortError::InvalidKey {
                    detail: "source guard kind is missing".to_string(),
                })?;
        let guard_sha256 =
            file.source_guard_sha256
                .as_deref()
                .ok_or_else(|| CachePortError::InvalidKey {
                    detail: "source guard sha256 is missing".to_string(),
                })?;
        let lookup = self.with_store(|store| {
            store.lookup_cache(
                &file.file_identity,
                &file.source_version,
                guard_kind,
                guard_sha256,
                &profile_hash,
                inventory_existed_before,
            )
        })?;
        let lookup = lookup.decode_payload(crate::store::cache::PARSE_CACHE_PAYLOAD_VERSION);
        Ok(ParseLookupOutcome {
            parse_profile_hash: profile_hash,
            lookup,
        })
    }

    fn lookup_classification(
        &self,
        file: &DiscoveredFileOut,
        classifier_profile_hash: &str,
        classifier_build: &str,
        inventory_existed_before: bool,
    ) -> Result<ClassificationCacheLookup, CachePortError> {
        let guard_kind =
            file.source_guard_kind
                .as_deref()
                .ok_or_else(|| CachePortError::InvalidKey {
                    detail: "source guard kind is missing".to_string(),
                })?;
        let guard_sha256 =
            file.source_guard_sha256
                .as_deref()
                .ok_or_else(|| CachePortError::InvalidKey {
                    detail: "source guard sha256 is missing".to_string(),
                })?;
        self.with_store(|store| {
            store.lookup_classification_cache(
                &file.file_identity,
                &file.source_version,
                guard_kind,
                guard_sha256,
                classifier_profile_hash,
                classifier_build,
                inventory_existed_before,
            )
        })
    }

    fn write_parse(&self, now_ms: u64, records: &[CacheWriteRecord]) -> Result<(), CachePortError> {
        self.with_store(|store| {
            store.write_success_parse_cache_with_deadline(
                records,
                now_ms,
                self.deadlines,
                &self.clock,
            )
        })
    }

    fn write_classification(
        &self,
        now_ms: u64,
        records: &[ClassificationCacheWriteRecord],
    ) -> Result<(), CachePortError> {
        self.with_store(|store| {
            store.write_success_classification_cache_with_deadline(
                records,
                now_ms,
                self.deadlines,
                &self.clock,
            )
        })
    }

    fn touch_access(
        &self,
        now_ms: u64,
        parse_hits: &[String],
        classification_hits: &[String],
    ) -> Result<(), CachePortError> {
        if parse_hits.is_empty() && classification_hits.is_empty() {
            return Ok(());
        }
        self.with_store(|store| {
            store.touch_cache_access_with_deadline(
                now_ms,
                parse_hits,
                classification_hits,
                self.deadlines,
                &self.clock,
            )
        })
    }
}

/// Production parser adapter. Light text stays in-process; every Office/PDF
/// route crosses one of the two long-lived worker-v3 pools.
pub struct ProductionParser {
    profile: NormalizedScannerSettings,
    pdf_pool: Arc<crate::session::WorkerPool>,
    office_pool: Arc<crate::session::WorkerPool>,
}

impl ProductionParser {
    pub fn new(
        profile: &NormalizedScannerSettings,
        pdf_pool: Arc<crate::session::WorkerPool>,
        office_pool: Arc<crate::session::WorkerPool>,
    ) -> Self {
        Self {
            profile: profile.clone(),
            pdf_pool,
            office_pool,
        }
    }

    fn parse_with_worker_pool(
        &self,
        request: &ParseRequest,
        pool: &crate::session::WorkerPool,
    ) -> ParseResult {
        let mut worker_request = crate::parsers::worker_request(
            &request.file,
            parser_route(request.route),
            request.timeout_ms,
            &self.profile,
        );
        if let ai_daily_worker_contract::ParserLimits::Pdf { selected_pages, .. } =
            &mut worker_request.parser_limits
        {
            *selected_pages = request.selected_pdf_pages.clone();
        }
        match pool.parse_worker(&worker_request, Duration::from_millis(request.timeout_ms)) {
            Ok(outcome) => {
                let duration_ms = outcome.duration_ms;
                let response = outcome.value;
                let content_sha256 = crate::store::sha256_hex(response.content.as_bytes());
                ParseResult {
                    evidence_audit: response.evidence_audit,
                    file_identity: request.file.file_identity.clone(),
                    content: response.content,
                    parser_backend: response.parser_backend.as_str().to_string(),
                    worker_lane: request.route.worker_lane().to_string(),
                    truncated: response.truncated,
                    content_sha256,
                    parse_status: ParseStatus::Success,
                    error: None,
                    warnings: Vec::new(),
                    failure_class: String::new(),
                    fallback_backend: String::new(),
                    fallback_reason_code: String::new(),
                    parse_transport: match outcome.transport {
                        crate::session::WorkerTransport::Session => ParseTransport::Session,
                        crate::session::WorkerTransport::NotApplicable => {
                            ParseTransport::NotApplicable
                        }
                    },
                    parse_attempt_count: outcome.attempt_count,
                    primary_duration_ms: duration_ms,
                    fallback_duration_ms: 0,
                    parse_duration_ms: duration_ms,
                }
            }
            Err(failure) => {
                let parse_status = if failure.failure.is_timeout() {
                    ParseStatus::Timeout
                } else {
                    ParseStatus::Error
                };
                let parser_started = failure.attempt_count > 0;
                ParseResult {
                    evidence_audit: None,
                    file_identity: request.file.file_identity.clone(),
                    content: String::new(),
                    parser_backend: if parser_started {
                        request.route.backend().to_string()
                    } else {
                        "not_parsed".to_string()
                    },
                    worker_lane: if parser_started {
                        request.route.worker_lane().to_string()
                    } else {
                        "not_parsed".to_string()
                    },
                    truncated: false,
                    content_sha256: crate::store::sha256_hex(b""),
                    parse_status,
                    error: Some(failure.failure.diagnostic),
                    warnings: Vec::new(),
                    failure_class: failure.failure.class.as_str().to_string(),
                    fallback_backend: String::new(),
                    fallback_reason_code: String::new(),
                    parse_transport: if parser_started {
                        match failure.transport {
                            crate::session::WorkerTransport::Session => ParseTransport::Session,
                            crate::session::WorkerTransport::NotApplicable => {
                                ParseTransport::NotApplicable
                            }
                        }
                    } else {
                        ParseTransport::NotApplicable
                    },
                    parse_attempt_count: failure.attempt_count,
                    primary_duration_ms: if parser_started {
                        failure.duration_ms
                    } else {
                        0
                    },
                    fallback_duration_ms: 0,
                    parse_duration_ms: if parser_started {
                        failure.duration_ms
                    } else {
                        0
                    },
                }
            }
        }
    }
}

impl ParserPort for ProductionParser {
    fn parse(&self, request: &ParseRequest) -> ParseResult {
        match request.route {
            RouteKind::LightText => self.parse_light_text(request),
            RouteKind::Pdf => self.parse_with_worker_pool(request, &self.pdf_pool),
            _ => self.parse_with_worker_pool(request, &self.office_pool),
        }
    }
}

impl ProductionParser {
    fn parse_light_text(&self, request: &ParseRequest) -> ParseResult {
        let started = std::time::Instant::now();
        let path = PathBuf::from(&request.file.path);
        let observed_before = match crate::parsers::current_source(&request.file.path) {
            Ok((source_version, _)) if source_version == request.file.source_version => {
                source_version
            }
            Ok(_) => {
                return light_text_error(
                    request,
                    ErrorCode::SourceVersionChanged,
                    "file source version changed before or during parsing",
                    false,
                    0,
                    0,
                );
            }
            Err(()) => {
                return light_text_error(
                    request,
                    ErrorCode::ParserFailed,
                    "file metadata is unavailable",
                    true,
                    0,
                    0,
                );
            }
        };
        let parsed = match light_text::parse_light_text(
            &path,
            &request.file.extension,
            &self.profile.parse.text,
            self.profile.execution.max_file_size_bytes,
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                let retryable = matches!(
                    error,
                    light_text::LightTextError::ReadFailed
                        | light_text::LightTextError::MetadataFailed
                );
                let code = if error == light_text::LightTextError::FileTooLarge {
                    ErrorCode::FileTooLarge
                } else {
                    ErrorCode::ParserFailed
                };
                let duration_ms = elapsed_ms(started);
                return light_text_error(
                    request,
                    code,
                    &error.to_string(),
                    retryable,
                    1,
                    duration_ms,
                );
            }
        };
        if crate::parsers::current_source(&request.file.path)
            .map(|(source_version, _)| source_version)
            .as_deref()
            != Ok(observed_before.as_str())
        {
            let duration_ms = elapsed_ms(started);
            return light_text_error(
                request,
                ErrorCode::SourceVersionChanged,
                "file source version changed before or during parsing",
                false,
                1,
                duration_ms,
            );
        }
        let duration_ms = elapsed_ms(started);
        let content_sha256 = crate::store::sha256_hex(parsed.content.as_bytes());
        ParseResult {
            evidence_audit: Some(parsed.evidence_audit),
            file_identity: request.file.file_identity.clone(),
            content: parsed.content,
            parser_backend: "light_text_v2".to_string(),
            worker_lane: "rust_core".to_string(),
            truncated: parsed.truncated,
            content_sha256,
            parse_status: ParseStatus::Success,
            error: None,
            warnings: Vec::new(),
            failure_class: String::new(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ParseTransport::RustInProcess,
            parse_attempt_count: 1,
            primary_duration_ms: duration_ms,
            fallback_duration_ms: 0,
            parse_duration_ms: duration_ms,
        }
    }
}

fn light_text_error(
    request: &ParseRequest,
    error_code: ErrorCode,
    message: &str,
    retryable: bool,
    attempt_count: u64,
    duration_ms: u64,
) -> ParseResult {
    ParseResult {
        evidence_audit: None,
        file_identity: request.file.file_identity.clone(),
        content: String::new(),
        parser_backend: if attempt_count == 0 {
            "not_parsed".to_string()
        } else {
            "light_text_v2".to_string()
        },
        worker_lane: if attempt_count == 0 {
            "not_parsed".to_string()
        } else {
            "rust_core".to_string()
        },
        truncated: false,
        content_sha256: crate::store::sha256_hex(b""),
        parse_status: ParseStatus::Error,
        error: Some(Diagnostic {
            error_code,
            message: message.to_string(),
            retryable,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some(request.file.path.clone())),
            backend: Nullable(Some("light_text_v2".to_string())),
        }),
        warnings: Vec::new(),
        failure_class: if retryable {
            "environment_unavailable".to_string()
        } else {
            "deterministic".to_string()
        },
        fallback_backend: String::new(),
        fallback_reason_code: String::new(),
        parse_transport: if attempt_count == 0 {
            ParseTransport::NotApplicable
        } else {
            ParseTransport::RustInProcess
        },
        parse_attempt_count: attempt_count,
        primary_duration_ms: duration_ms,
        fallback_duration_ms: 0,
        parse_duration_ms: duration_ms,
    }
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn parser_route(route: RouteKind) -> ParserRoute {
    match route {
        RouteKind::LightText => ParserRoute::LightText,
        RouteKind::RustOffice => ParserRoute::RustOffice,
        RouteKind::RustXlsx => ParserRoute::RustXlsx,
        RouteKind::Pdf => ParserRoute::Pdf,
        RouteKind::RustXls => ParserRoute::RustXls,
        RouteKind::RustLegacyOffice => ParserRoute::RustLegacyOffice,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CacheLookup, RouteStackFingerprint};
    use ai_daily_scanner_contract::{CacheMissReason, ReportMode, ScannerSettings};
    use std::path::Path;

    fn cache_port(path: &Path) -> StoreCachePort {
        let clock = RealClock::new();
        let settings: ScannerSettings = serde_json::from_str("{}").unwrap();
        StoreCachePort::new(
            path.to_path_buf(),
            RouteStackFingerprints {
                text_like: RouteStackFingerprint::text("test-engine").unwrap(),
                modern_office: RouteStackFingerprint::modern_office(
                    "test-engine",
                    "ai_daily_worker_v3",
                    "test-office",
                )
                .unwrap(),
                pdf: RouteStackFingerprint::pdf("test-engine", "ai_daily_worker_v3", "test-pdf")
                    .unwrap(),
            },
            crate::config::normalize_scanner_settings(&settings, ReportMode::Daily).unwrap(),
            RunDeadlines::derive(30_000, &clock).unwrap(),
            clock,
        )
    }

    fn file() -> DiscoveredFileOut {
        DiscoveredFileOut {
            file_identity: "test-file".to_string(),
            path: "C:/fixture/test.txt".to_string(),
            extension: ".txt".to_string(),
            modified_at: "2026-10-17T00:00:00".to_string(),
            size_bytes: 1,
            source_version: "mtime_ns=1:size=1".to_string(),
            source_guard_kind: Some("content_sha256_v1".to_string()),
            source_guard_sha256: Some("a".repeat(64)),
        }
    }

    fn assert_miss(port: &StoreCachePort) {
        let result = port
            .lookup_parse(&file(), RouteKind::LightText, false)
            .unwrap();
        assert!(matches!(
            result.lookup,
            CacheLookup::Miss(CacheMissReason::NewFile)
        ));
    }

    #[test]
    fn missing_cache_database_is_not_created_and_can_be_retried() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scan_index_rust_v3.sqlite3");
        let port = cache_port(&path);
        assert!(port
            .lookup_parse(&file(), RouteKind::LightText, false)
            .is_err());
        assert!(!path.exists());
        drop(ScannerStore::open(&path).unwrap());
        assert_miss(&port);
        assert_miss(&port);
    }

    #[test]
    fn cache_connection_rejects_legacy_schema_and_later_table_loss() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scan_index_rust_v3.sqlite3");
        let database = rusqlite::Connection::open(&path).unwrap();
        database.pragma_update(None, "user_version", 2).unwrap();
        assert!(cache_port(&path)
            .lookup_parse(&file(), RouteKind::LightText, false)
            .is_err());

        let current_directory = tempfile::tempdir().unwrap();
        let current_path = current_directory.path().join("scan_index_rust_v3.sqlite3");
        drop(ScannerStore::open(&current_path).unwrap());
        let port = cache_port(&current_path);
        assert_miss(&port);
        let external = rusqlite::Connection::open(&current_path).unwrap();
        external.execute_batch("DROP TABLE parse_cache").unwrap();
        assert!(port
            .lookup_parse(&file(), RouteKind::LightText, false)
            .is_err());
    }

    #[test]
    fn concurrent_cache_callers_share_serialized_sqlite_access() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scan_index_rust_v3.sqlite3");
        drop(ScannerStore::open(&path).unwrap());
        let port = Arc::new(cache_port(&path));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let port = Arc::clone(&port);
                std::thread::spawn(move || {
                    for _ in 0..8 {
                        assert_miss(&port);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
}
