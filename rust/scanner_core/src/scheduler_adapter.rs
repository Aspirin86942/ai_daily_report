//! Production adapters that wire the [`crate::scheduler::BudgetedContextScheduler`]
//! to the real store / parser / classifier / worker binaries (spec Solution:
//! parser/classifier executor and `CachePort` are local replaceable adapters).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    Diagnostic, DiagnosticStage, ErrorCode, NormalizedScannerProfileV1, Nullable, ParseStatus,
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

/// Store-backed [`CachePort`]. Each operation opens its own connection to the
/// scan database (WAL allows concurrent connections; the lease heartbeat uses
/// the same pattern). Only verified successful results are written.
pub struct StoreCachePort {
    db_path: PathBuf,
    route_stacks: RouteStackFingerprints,
    v1_profile: NormalizedScannerProfileV1,
    deadlines: RunDeadlines,
    clock: RealClock,
}

impl StoreCachePort {
    pub fn new(
        db_path: PathBuf,
        route_stacks: RouteStackFingerprints,
        v1_profile: NormalizedScannerProfileV1,
        deadlines: RunDeadlines,
        clock: RealClock,
    ) -> Self {
        Self {
            db_path,
            route_stacks,
            v1_profile,
            deadlines,
            clock,
        }
    }

    fn open(&self) -> Result<ScannerStore, CachePortError> {
        ScannerStore::open_existing(&self.db_path).map_err(map_store_error)
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
        let mut store = self.open()?;
        let run_id = i64::try_from(scan_run_id).map_err(|_| CachePortError::InvalidKey {
            detail: "scan_run_id exceeds SQLite integer range".to_string(),
        })?;
        store
            .prepare_inventory_with_deadline(records, run_id, now_ms, self.deadlines, &self.clock)
            .map_err(map_store_error)
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
            &self.v1_profile,
        )
        .map_err(|message| CachePortError::InvalidKey { detail: message })?;
        let store = self.open()?;
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
        let lookup = store
            .lookup_cache(
                &file.file_identity,
                &file.source_version,
                guard_kind,
                guard_sha256,
                &profile_hash,
                inventory_existed_before,
            )
            .map_err(|error| CachePortError::Store {
                detail: error.to_string(),
            })?;
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
        let store = self.open()?;
        store
            .lookup_classification_cache(
                &file.file_identity,
                &file.source_version,
                guard_kind,
                guard_sha256,
                classifier_profile_hash,
                classifier_build,
                inventory_existed_before,
            )
            .map_err(|error| CachePortError::Store {
                detail: error.to_string(),
            })
    }

    fn write_parse(&self, now_ms: u64, records: &[CacheWriteRecord]) -> Result<(), CachePortError> {
        let mut store = self.open()?;
        store
            .write_success_parse_cache_with_deadline(records, now_ms, self.deadlines, &self.clock)
            .map_err(map_store_error)
    }

    fn write_classification(
        &self,
        now_ms: u64,
        records: &[ClassificationCacheWriteRecord],
    ) -> Result<(), CachePortError> {
        let mut store = self.open()?;
        store
            .write_success_classification_cache_with_deadline(
                records,
                now_ms,
                self.deadlines,
                &self.clock,
            )
            .map_err(map_store_error)
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
        let mut store = self.open()?;
        store
            .touch_cache_access_with_deadline(
                now_ms,
                parse_hits,
                classification_hits,
                self.deadlines,
                &self.clock,
            )
            .map_err(map_store_error)
    }
}

/// Production parser adapter. Light text stays in-process; every Office/PDF
/// route crosses one of the two long-lived worker-v2 pools.
pub struct ProductionParser {
    profile: NormalizedScannerProfileV1,
    python_pool: Arc<crate::session::WorkerPool>,
    office_pool: Arc<crate::session::WorkerPool>,
}

impl ProductionParser {
    pub fn new(
        profile: &NormalizedScannerProfileV1,
        python_pool: Arc<crate::session::WorkerPool>,
        office_pool: Arc<crate::session::WorkerPool>,
    ) -> Self {
        Self {
            profile: profile.clone(),
            python_pool,
            office_pool,
        }
    }

    fn parse_with_worker_pool(
        &self,
        request: &ParseRequest,
        pool: &crate::session::WorkerPool,
    ) -> ParseResult {
        let worker_request = crate::parsers::worker_request(
            &request.file,
            parser_route(request.route),
            request.timeout_ms,
            &self.profile,
        );
        match pool.parse_worker(&worker_request, Duration::from_millis(request.timeout_ms)) {
            Ok(outcome) => {
                let duration_ms = outcome.duration_ms;
                let response = outcome.value;
                let content_sha256 = crate::store::sha256_hex(response.content.as_bytes());
                ParseResult {
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
            RouteKind::Pdf | RouteKind::PythonOffice | RouteKind::PythonSharepointText => {
                self.parse_with_worker_pool(request, &self.python_pool)
            }
            RouteKind::RustOffice | RouteKind::RustXlsx => {
                let worker_request = crate::parsers::worker_request(
                    &request.file,
                    parser_route(request.route),
                    request.timeout_ms,
                    &self.profile,
                );
                let execution = crate::parsers::office::parse_with_pools(
                    &self.office_pool,
                    Some(&self.python_pool),
                    &worker_request,
                    &self.profile.parse.office,
                );
                office_execution_result(execution, request)
            }
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

fn office_execution_result(
    execution: crate::parsers::office::OfficeParseExecution,
    request: &ParseRequest,
) -> ParseResult {
    let parser_started = execution.attempt_count > 0;
    let response = execution.response;
    let content = response
        .as_ref()
        .map_or_else(String::new, |value| value.content.clone());
    let truncated = response.as_ref().is_some_and(|value| value.truncated);
    let final_error = execution.final_failure;
    let parse_status = if final_error.is_none() {
        ParseStatus::Success
    } else if final_error
        .as_ref()
        .is_some_and(crate::fallback::ParseFailure::is_timeout)
    {
        ParseStatus::Timeout
    } else {
        ParseStatus::Error
    };
    let fallback_reason_code = execution
        .primary_failure
        .as_ref()
        .map(|failure| crate::store::inventory::enum_text(&failure.diagnostic.error_code))
        .unwrap_or_default();
    let warnings = if final_error.is_none() {
        execution
            .primary_failure
            .iter()
            .map(|failure| failure.diagnostic.clone())
            .collect()
    } else {
        Vec::new()
    };
    let failure_class = final_error
        .as_ref()
        .map(|failure| failure.class.as_str().to_string())
        .unwrap_or_default();
    let error = final_error.map(|failure| failure.diagnostic);
    let backend = response.as_ref().map_or_else(
        || {
            execution
                .last_started_backend
                .map_or("not_parsed", |backend| backend.as_str())
                .to_string()
        },
        |response| response.parser_backend.as_str().to_string(),
    );
    let worker_lane = response.as_ref().map_or_else(
        || {
            execution
                .last_started_backend
                .map_or("not_parsed", worker_lane_for_backend)
                .to_string()
        },
        |response| worker_lane_name(response.worker_lane).to_string(),
    );
    ParseResult {
        file_identity: request.file.file_identity.clone(),
        content_sha256: crate::store::sha256_hex(content.as_bytes()),
        content,
        parser_backend: if parser_started {
            backend
        } else {
            "not_parsed".to_string()
        },
        worker_lane: if parser_started {
            worker_lane
        } else {
            "not_parsed".to_string()
        },
        truncated,
        parse_status,
        error,
        warnings,
        failure_class,
        fallback_backend: execution
            .fallback_backend
            .map(|backend| backend.as_str().to_string())
            .unwrap_or_default(),
        fallback_reason_code,
        parse_transport: if parser_started {
            ParseTransport::Session
        } else {
            ParseTransport::NotApplicable
        },
        parse_attempt_count: execution.attempt_count,
        primary_duration_ms: execution.primary_duration_ms,
        fallback_duration_ms: execution.fallback_duration_ms,
        parse_duration_ms: execution
            .primary_duration_ms
            .saturating_add(execution.fallback_duration_ms),
    }
}

fn parser_route(route: RouteKind) -> ParserRoute {
    match route {
        RouteKind::LightText => ParserRoute::LightText,
        RouteKind::RustOffice => ParserRoute::RustOffice,
        RouteKind::RustXlsx => ParserRoute::RustXlsx,
        RouteKind::Pdf => ParserRoute::Pdf,
        RouteKind::PythonOffice => ParserRoute::PythonOffice,
        RouteKind::PythonSharepointText => ParserRoute::PythonSharepointText,
    }
}

fn worker_lane_for_backend(backend: ai_daily_scanner_contract::WorkerBackend) -> &'static str {
    worker_lane_name(backend.lane())
}

fn worker_lane_name(lane: ai_daily_scanner_contract::WorkerLane) -> &'static str {
    match lane {
        ai_daily_scanner_contract::WorkerLane::RustOfficeProcessV2 => "rust_office_process_v2",
        ai_daily_scanner_contract::WorkerLane::PythonDocumentProcessV2 => {
            "python_document_process_v2"
        }
    }
}
