//! Production adapters that wire the [`crate::scheduler::BudgetedContextScheduler`]
//! to the real store / parser / classifier / worker binaries (spec Solution:
//! parser/classifier executor and `CachePort` are local replaceable adapters).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    Diagnostic, DiagnosticStage, ErrorCode, NormalizedScannerSettings, Nullable, ParseStatus,
    ParseTransport,
};

use crate::budget_model::RouteKind;
use crate::fallback::{FailureClass, ParseFailure};
use crate::parsers::light_text;
use crate::scheduler::{
    CachePort, CachePortError, ParseLookupOutcome, ParseRequest, ParseResult, ParserPort,
    RealClock, RunDeadlines,
};
use crate::session::{WorkerPool, WorkerTransport};
use crate::store::{
    sha256_hex, CacheWriteRecord, ClassificationCacheLookup, ClassificationCacheWriteRecord,
    InventoryRecord, RouteStackFingerprints, ScannerStore, StoreError,
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
        StoreError::CacheRecord { detail } => CachePortError::Record { detail },
        _ => CachePortError::Store {
            detail: error.to_string(),
        },
    }
}

fn cache_guard(file: &DiscoveredFileOut) -> Result<(&str, &str), CachePortError> {
    let required = |field| CachePortError::InvalidKey {
        detail: format!("source guard {field} is missing"),
    };
    Ok((
        file.source_guard_kind
            .as_deref()
            .ok_or_else(|| required("kind"))?,
        file.source_guard_sha256
            .as_deref()
            .ok_or_else(|| required("sha256"))?,
    ))
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
        let base_hash = crate::store::cache::parse_profile_hash(
            1,
            self.route_stacks.for_route(route),
            &self.settings,
        )
        .map_err(|message| CachePortError::InvalidKey { detail: message })?;
        let profile_hash = match route {
            RouteKind::PdfImageOcr {
                page_count,
                max_pages,
            } => {
                let pdf = &self.settings.parse.pdf;
                let limits = crate::parsers::document::image_ocr_limits(page_count, max_pages, pdf);
                // base_hash covers the OCR implementation and pinned model hashes
                // via PARSER_CONTENT_IDENTITY. Bind the admitted request as well.
                let key = serde_json::to_vec(&(base_hash, limits)).map_err(|error| {
                    CachePortError::InvalidKey {
                        detail: error.to_string(),
                    }
                })?;
                crate::store::cache::domain_hash(b"pdf-image-ocr-v1\0", &key)
            }
            _ => base_hash,
        };
        let (guard_kind, guard_sha256) = cache_guard(file)?;
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
        pdf_open_failure_key: &str,
        inventory_existed_before: bool,
    ) -> Result<ClassificationCacheLookup, CachePortError> {
        let (guard_kind, guard_sha256) = cache_guard(file)?;
        let lookup = self.with_store(|store| {
            let lookup = store.lookup_classification_cache(
                &file.file_identity,
                &file.source_version,
                guard_kind,
                guard_sha256,
                classifier_profile_hash,
                classifier_build,
                inventory_existed_before,
            )?;
            if inventory_existed_before && matches!(lookup, ClassificationCacheLookup::Miss(_)) {
                if let Some(failure) = store.pdf_open_failure(
                    &file.file_identity,
                    pdf_open_failure_key,
                    classifier_profile_hash,
                    classifier_build,
                )? {
                    return Ok(ClassificationCacheLookup::PdfOpenError(failure));
                }
            }
            Ok(lookup)
        })?;
        // Partial runs have no artifact-file rows. Their successful classification
        // cache may retain counts without observations. Re-probe eligible-length
        // documents so a repaired model can be used on the next run.
        if self.settings.parse.pdf.image_ocr.is_some()
            && matches!(&lookup, ClassificationCacheLookup::Fresh(entry)
                if entry.status == "no_text_in_parse_window"
                    && entry.page_count == entry.result_examined_pages
                    && entry.page_audit.as_ref().and_then(|audit| audit.observations.as_ref()).is_none())
        {
            return Ok(ClassificationCacheLookup::Miss(
                crate::store::ClassificationCacheMissReason::EntryAbsentOrEvicted,
            ));
        }
        Ok(lookup)
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
/// route uses its registered worker-v4 pool.
pub struct ProductionParser {
    profile: NormalizedScannerSettings,
    pdf_pool: Arc<WorkerPool>,
    office_pool: Arc<WorkerPool>,
}

impl ProductionParser {
    pub fn new(
        profile: &NormalizedScannerSettings,
        pdf_pool: Arc<WorkerPool>,
        office_pool: Arc<WorkerPool>,
    ) -> Self {
        Self {
            profile: profile.clone(),
            pdf_pool,
            office_pool,
        }
    }

    fn parse_light_text(&self, request: &ParseRequest) -> ParseResult {
        let started = std::time::Instant::now();
        let file = &request.file;
        let source_matches = || {
            crate::parsers::current_source(&file.path)
                .map(|(version, _)| version == file.source_version)
        };
        let transport = ParseTransport::RustInProcess;
        let fail = |code, message: &str, retryable, elapsed: Option<u64>| {
            let failure = ParseFailure {
                class: if retryable {
                    FailureClass::EnvironmentUnavailable
                } else {
                    FailureClass::Deterministic
                },
                diagnostic: Diagnostic {
                    error_code: code,
                    message: message.into(),
                    retryable,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(Some(file.path.clone())),
                    backend: Nullable(Some(request.route.backend().into())),
                },
            };
            request.failed(
                failure,
                transport,
                u64::from(elapsed.is_some()),
                elapsed.unwrap_or(0),
            )
        };
        let changed = |elapsed| {
            fail(
                ErrorCode::SourceVersionChanged,
                "file source version changed before or during parsing",
                false,
                elapsed,
            )
        };
        let timeout = |message, elapsed| fail(ErrorCode::ParserTimeout, message, false, elapsed);
        match source_matches() {
            Ok(true) => {}
            Ok(false) => return changed(None),
            Err(()) => {
                return fail(
                    ErrorCode::ParserFailed,
                    "file metadata is unavailable",
                    true,
                    None,
                )
            }
        }
        if elapsed_ms(started) >= request.timeout_ms {
            return timeout("light text deadline exhausted before reading", None);
        }
        let parsed = match light_text::parse_light_text(
            Path::new(&file.path),
            &file.extension,
            &self.profile.parse.text,
            self.profile.execution.max_file_size_bytes,
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                use light_text::LightTextError::*;
                let code = if error == FileTooLarge {
                    ErrorCode::FileTooLarge
                } else {
                    ErrorCode::ParserFailed
                };
                let retryable = matches!(error, ReadFailed | MetadataFailed);
                let duration_ms = elapsed_ms(started);
                return fail(code, &error.to_string(), retryable, Some(duration_ms));
            }
        };
        if source_matches() != Ok(true) {
            return changed(Some(elapsed_ms(started)));
        }
        let hash = sha256_hex(parsed.content.as_bytes());
        let duration_ms = elapsed_ms(started);
        if duration_ms >= request.timeout_ms {
            return timeout("light text exceeded its deadline", Some(duration_ms));
        }
        ParseResult {
            content: parsed.content,
            evidence_audit: Some(parsed.evidence_audit),
            truncated: parsed.truncated,
            ..request.result(
                Some(request.route.backend()),
                hash,
                transport,
                1,
                duration_ms,
            )
        }
    }
}

impl ParserPort for ProductionParser {
    fn parse(&self, request: &ParseRequest) -> ParseResult {
        let pool = match request.route {
            RouteKind::LightText => return self.parse_light_text(request),
            RouteKind::Pdf | RouteKind::PdfImageOcr { .. } => &self.pdf_pool,
            _ => &self.office_pool,
        };
        let worker_request = crate::parsers::worker_request(
            &request.file,
            request.route,
            request.timeout_ms,
            &self.profile,
            request.selected_pdf_pages.as_deref(),
        );
        match pool.parse_worker(&worker_request, Duration::from_millis(request.timeout_ms)) {
            Ok(outcome) => {
                let response = outcome.value;
                let backend = if matches!(request.route, RouteKind::PdfImageOcr { .. }) {
                    request.route.backend()
                } else {
                    response.parser_backend.as_str()
                };
                let result = request.result(
                    Some(backend),
                    sha256_hex(response.content.as_bytes()),
                    outcome.transport.into(),
                    outcome.attempt_count,
                    outcome.duration_ms,
                );
                ParseResult {
                    content: response.content,
                    evidence_audit: response.evidence_audit,
                    truncated: response.truncated,
                    ..result
                }
            }
            Err(failure) => request.failed(
                failure.failure,
                failure.transport.into(),
                failure.attempt_count,
                failure.duration_ms,
            ),
        }
    }
}

impl From<WorkerTransport> for ParseTransport {
    fn from(transport: WorkerTransport) -> Self {
        match transport {
            WorkerTransport::Session => Self::Session,
            WorkerTransport::NotApplicable => Self::NotApplicable,
        }
    }
}

impl ParseRequest {
    fn result(
        &self,
        backend: Option<&str>,
        content_sha256: String,
        transport: ParseTransport,
        attempt_count: u64,
        duration_ms: u64,
    ) -> ParseResult {
        // A pre-dispatch error has no executed backend, transport or elapsed parse time.
        let (backend, lane, parse_transport, duration_ms) = match backend {
            Some(backend) => (backend, self.route.worker_lane(), transport, duration_ms),
            None => ("not_parsed", "not_parsed", ParseTransport::NotApplicable, 0),
        };
        ParseResult {
            file_identity: self.file.file_identity.clone(),
            content: String::new(),
            evidence_audit: None,
            parser_backend: backend.into(),
            worker_lane: lane.into(),
            truncated: false,
            content_sha256,
            parse_status: ParseStatus::Success,
            error: None,
            warnings: Vec::new(),
            failure_class: String::new(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport,
            parse_attempt_count: attempt_count,
            primary_duration_ms: duration_ms,
            fallback_duration_ms: 0,
            parse_duration_ms: duration_ms,
        }
    }

    fn failed(
        &self,
        failure: ParseFailure,
        transport: ParseTransport,
        attempts: u64,
        duration_ms: u64,
    ) -> ParseResult {
        let backend = (attempts > 0).then(|| self.route.backend());
        ParseResult {
            parse_status: if failure.is_timeout() {
                ParseStatus::Timeout
            } else {
                ParseStatus::Error
            },
            failure_class: failure.class.as_str().into(),
            error: Some(failure.diagnostic),
            ..self.result(backend, sha256_hex(b""), transport, attempts, duration_ms)
        }
    }
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
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
    fn image_ocr_cache_binds_the_effective_pages_and_preserves_native_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scan_index_rust_v3.sqlite3");
        drop(ScannerStore::open(&path).unwrap());
        let mut port = cache_port(&path);
        let hash = |port: &StoreCachePort, route| {
            port.lookup_parse(&file(), route, false)
                .unwrap()
                .parse_profile_hash
        };
        let native_pdf = hash(&port, RouteKind::Pdf);
        let native_office = hash(&port, RouteKind::RustOffice);
        port.settings.parse.pdf.image_ocr = Some(ai_daily_scanner_contract::PdfImageOcrSettings {
            enabled: true,
            max_files: 1,
            max_pages: 3,
        });
        let selection = |page_count, max_pages| RouteKind::PdfImageOcr {
            page_count,
            max_pages,
        };
        let three = hash(&port, selection(4, 3));
        assert_ne!(native_pdf, three);
        assert_ne!(three, hash(&port, selection(4, 2)));
        assert_ne!(three, hash(&port, selection(5, 3)));
        port.settings
            .parse
            .pdf
            .image_ocr
            .as_mut()
            .unwrap()
            .max_files = 2;
        assert_eq!(three, hash(&port, selection(4, 3)));
        assert_eq!(native_pdf, hash(&port, RouteKind::Pdf));
        assert_eq!(native_office, hash(&port, RouteKind::RustOffice));
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
