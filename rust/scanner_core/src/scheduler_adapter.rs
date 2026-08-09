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
use crate::parsers::{ParsedPayload, ParserScheduler, ScheduledFileParse, WorkerRegistry};
use crate::planner::PlanAction;
use crate::scheduler::{
    CachePort, CachePortError, ParseLookupOutcome, ParseRequest, ParseResult, ParserPort,
};
use crate::store::{
    ClassificationCacheLookup, ClassificationCacheWriteRecord, CacheWriteRecord, InventoryRecord,
    RouteStackFingerprints, ScannerStore,
};

/// Store-backed [`CachePort`]. Each operation opens its own connection to the
/// scan database (WAL allows concurrent connections; the lease heartbeat uses
/// the same pattern). Only verified successful results are written.
pub struct StoreCachePort {
    db_path: PathBuf,
    route_stacks: RouteStackFingerprints,
    v1_profile: NormalizedScannerProfileV1,
}

impl StoreCachePort {
    pub fn new(
        db_path: PathBuf,
        route_stacks: RouteStackFingerprints,
        v1_profile: NormalizedScannerProfileV1,
    ) -> Self {
        Self {
            db_path,
            route_stacks,
            v1_profile,
        }
    }

    fn open(&self) -> Result<ScannerStore, CachePortError> {
        ScannerStore::open_existing(&self.db_path).map_err(|error| CachePortError::Store {
            detail: error.to_string(),
        })
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
        let run_id = i64::try_from(scan_run_id)
            .map_err(|_| CachePortError::InvalidKey {
                detail: "scan_run_id exceeds SQLite integer range".to_string(),
            })?;
        store
            .prepare_inventory(records, run_id, now_ms)
            .map_err(|error| CachePortError::Store {
                detail: error.to_string(),
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
            &self.v1_profile,
        )
        .map_err(|message| CachePortError::InvalidKey {
            detail: message,
        })?;
        let store = self.open()?;
        let guard_kind = file
            .source_guard_kind
            .as_deref()
            .ok_or_else(|| CachePortError::InvalidKey {
                detail: "source guard kind is missing".to_string(),
            })?;
        let guard_sha256 = file
            .source_guard_sha256
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
        let guard_kind = file
            .source_guard_kind
            .as_deref()
            .ok_or_else(|| CachePortError::InvalidKey {
                detail: "source guard kind is missing".to_string(),
            })?;
        let guard_sha256 = file
            .source_guard_sha256
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
            .write_success_parse_cache(records, now_ms)
            .map_err(|error| CachePortError::Store {
                detail: error.to_string(),
            })
    }

    fn write_classification(
        &self,
        now_ms: u64,
        records: &[ClassificationCacheWriteRecord],
    ) -> Result<(), CachePortError> {
        let mut store = self.open()?;
        store
            .write_success_classification_cache(records, now_ms)
            .map_err(|error| CachePortError::Store {
                detail: error.to_string(),
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
        let mut store = self.open()?;
        store
            .touch_cache_access(now_ms, parse_hits, classification_hits)
            .map_err(|error| CachePortError::Store {
                detail: error.to_string(),
            })
    }
}

/// Parser adapter that delegates to the existing v1 [`ParserScheduler`].
pub struct ProductionParser {
    inner: ParserScheduler,
    profile: NormalizedScannerProfileV1,
    session: Option<Arc<crate::session::PythonSessionPool>>,
}

impl ProductionParser {
    pub fn new(
        profile: &NormalizedScannerProfileV1,
        workers: WorkerRegistry,
        session: Option<Arc<crate::session::PythonSessionPool>>,
    ) -> Self {
        Self {
            inner: ParserScheduler::from_registry(profile, workers),
            profile: profile.clone(),
            session,
        }
    }

    fn parse_pdf_with_session(
        &self,
        request: &ParseRequest,
        session: &crate::session::PythonSessionPool,
    ) -> ParseResult {
        let worker_request = crate::parsers::worker_request(
            &request.file,
            ParserRoute::Pdf,
            request.timeout_ms,
            &self.profile,
        );
        match session.parse_pdf(
            &worker_request,
            Duration::from_millis(request.timeout_ms),
        ) {
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
                        crate::session::PythonSessionTransport::Session => ParseTransport::Session,
                        crate::session::PythonSessionTransport::OneShot => ParseTransport::OneShot,
                        crate::session::PythonSessionTransport::NotApplicable => {
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
                            crate::session::PythonSessionTransport::Session => {
                                ParseTransport::Session
                            }
                            crate::session::PythonSessionTransport::OneShot => {
                                ParseTransport::OneShot
                            }
                            crate::session::PythonSessionTransport::NotApplicable => {
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
        if request.route == RouteKind::Pdf {
            if let Some(session) = &self.session {
                return self.parse_pdf_with_session(request, session);
            }
        }
        let planned = crate::planner::PlannedFile {
            file: request.file.clone(),
            action: PlanAction::Parse(parser_route(request.route)),
            timeout_ms: request.timeout_ms,
        };
        match self.inner.parse_planned_files(&[planned]) {
            Ok(mut parsed) => parsed
                .pop()
                .map(|result| to_parse_result(result, &request.file.file_identity))
                .unwrap_or_else(|| internal_parse_error(request)),
            Err(failure) => ParseResult {
                file_identity: request.file.file_identity.clone(),
                content: String::new(),
                parser_backend: "not_parsed".to_string(),
                worker_lane: "not_parsed".to_string(),
                truncated: false,
                content_sha256: crate::store::sha256_hex(b""),
                parse_status: if failure.is_timeout() {
                    ParseStatus::Timeout
                } else {
                    ParseStatus::Error
                },
                error: Some(failure.diagnostic),
                warnings: Vec::new(),
                failure_class: String::new(),
                fallback_backend: String::new(),
                fallback_reason_code: String::new(),
                parse_transport: ParseTransport::NotApplicable,
                parse_attempt_count: 0,
                primary_duration_ms: 0,
                fallback_duration_ms: 0,
                parse_duration_ms: 0,
            },
        }
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

fn to_parse_result(parsed: ScheduledFileParse, file_identity: &str) -> ParseResult {
    let (content, truncated) = match &parsed.payload {
        Some(ParsedPayload::LightText(payload)) => (payload.content.clone(), payload.truncated),
        Some(ParsedPayload::Worker(response)) => {
            (response.content.clone(), response.truncated)
        }
        None => (String::new(), false),
    };
    let content_sha256 = crate::store::sha256_hex(content.as_bytes());
    let error = parsed.error.as_ref().map(|failure| failure.diagnostic.clone());
    let parse_status = if error.is_none() {
        ParseStatus::Success
    } else if parsed
        .error
        .as_ref()
        .is_some_and(|failure| failure.is_timeout())
    {
        ParseStatus::Timeout
    } else {
        ParseStatus::Error
    };
    let failure_class = parsed
        .error
        .as_ref()
        .map(|failure| failure.class.as_str().to_string())
        .unwrap_or_default();
    let fallback_reason_code = parsed
        .primary_failure
        .as_ref()
        .map(|failure| crate::store::inventory::enum_text(&failure.diagnostic.error_code))
        .unwrap_or_default();
    let fallback_backend = parsed
        .fallback_backend
        .map(|backend| backend.as_str().to_string())
        .unwrap_or_default();
    // A primary parser failure that recovered via fallback is a degradation
    // warning (spec Part 5.2/2.2), never silently dropped.
    let warnings: Vec<Diagnostic> = if error.is_none() {
        parsed
            .primary_failure
            .iter()
            .map(|failure| failure.diagnostic.clone())
            .collect()
    } else {
        Vec::new()
    };
    let parser_started = parsed.attempt_count > 0;
    ParseResult {
        file_identity: file_identity.to_string(),
        content,
        parser_backend: if parser_started {
            parsed
                .parser_backend
                .clone()
                .unwrap_or_else(|| "not_parsed".to_string())
        } else {
            "not_parsed".to_string()
        },
        worker_lane: if parser_started {
            parsed
                .worker_lane
                .clone()
                .unwrap_or_else(|| "not_parsed".to_string())
        } else {
            "not_parsed".to_string()
        },
        truncated,
        content_sha256,
        parse_status,
        error,
        warnings,
        failure_class,
        fallback_backend,
        fallback_reason_code,
        parse_transport: if parser_started {
            match parsed.worker_lane.as_deref() {
                Some("rust_core") => ParseTransport::RustInProcess,
                Some("rust_office_process") | Some("python_document_process") => {
                    ParseTransport::OneShot
                }
                _ => ParseTransport::NotApplicable,
            }
        } else {
            ParseTransport::NotApplicable
        },
        parse_attempt_count: parsed.attempt_count,
        primary_duration_ms: if parser_started {
            parsed.primary_duration_ms
        } else {
            0
        },
        fallback_duration_ms: if parser_started {
            parsed.fallback_duration_ms
        } else {
            0
        },
        parse_duration_ms: if parser_started {
            parsed.total_duration_ms
        } else {
            0
        },
    }
}

fn internal_parse_error(request: &ParseRequest) -> ParseResult {
    ParseResult {
        file_identity: request.file.file_identity.clone(),
        content: String::new(),
        parser_backend: "not_parsed".to_string(),
        worker_lane: "not_parsed".to_string(),
        truncated: false,
        content_sha256: crate::store::sha256_hex(b""),
        parse_status: ParseStatus::Error,
        error: Some(Diagnostic {
            error_code: ErrorCode::InternalError,
            message: "parser scheduler returned no result".to_string(),
            retryable: true,
            stage: DiagnosticStage::Process,
            file_path: Nullable(Some(request.file.path.clone())),
            backend: Nullable(Some(request.route.backend().to_string())),
        }),
        warnings: Vec::new(),
        failure_class: String::new(),
        fallback_backend: String::new(),
        fallback_reason_code: String::new(),
        parse_transport: ParseTransport::NotApplicable,
        parse_attempt_count: 0,
        primary_duration_ms: 0,
        fallback_duration_ms: 0,
        parse_duration_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fallback::{FailureClass, ParseFailure};
    use crate::parsers::light_text::ParsedLightText;

    fn discovered_file() -> DiscoveredFileOut {
        DiscoveredFileOut {
            file_identity: "fixture:a".to_string(),
            path: "C:\\corpus\\a.md".to_string(),
            extension: ".md".to_string(),
            modified_at: "2026-08-09T00:00:00+08:00".to_string(),
            size_bytes: 1,
            source_version: "mtime_ns=1:size=1".to_string(),
            source_guard_kind: Some("windows_file_id_change_time_v1".to_string()),
            source_guard_sha256: Some("a".repeat(64)),
        }
    }

    #[test]
    fn to_parse_result_preserves_actual_attempts_and_durations() {
        let parsed = ScheduledFileParse {
            file: discovered_file(),
            payload: Some(ParsedPayload::LightText(ParsedLightText {
                content: "evidence".to_string(),
                parser_backend: "light_text_v1".to_string(),
                truncated: false,
                warnings: Vec::new(),
            })),
            primary_failure: None,
            error: None,
            fallback_backend: None,
            parser_backend: Some("light_text_v1".to_string()),
            worker_lane: Some("rust_core".to_string()),
            primary_duration_ms: 3,
            fallback_duration_ms: 4,
            total_duration_ms: 7,
            attempt_count: 2,
            partial: false,
        };

        let result = to_parse_result(parsed, "fixture:a");

        assert_eq!(result.parse_attempt_count, 2);
        assert_eq!(result.primary_duration_ms, 3);
        assert_eq!(result.fallback_duration_ms, 4);
        assert_eq!(result.parse_duration_ms, 7);
        assert_eq!(result.parse_transport, ParseTransport::RustInProcess);
    }

    #[test]
    fn to_parse_result_normalizes_pre_start_failure_to_zero_execution() {
        let parsed = ScheduledFileParse {
            file: discovered_file(),
            payload: None,
            primary_failure: None,
            error: Some(ParseFailure {
                class: FailureClass::EnvironmentUnavailable,
                diagnostic: Diagnostic {
                    error_code: ErrorCode::ParserStartFailed,
                    message: "not started".to_string(),
                    retryable: true,
                    stage: DiagnosticStage::Process,
                    file_path: Nullable(Some("C:\\corpus\\a.md".to_string())),
                    backend: Nullable(Some("light_text_v1".to_string())),
                },
            }),
            fallback_backend: None,
            parser_backend: Some("light_text_v1".to_string()),
            worker_lane: Some("rust_core".to_string()),
            primary_duration_ms: 9,
            fallback_duration_ms: 8,
            total_duration_ms: 17,
            attempt_count: 0,
            partial: true,
        };

        let result = to_parse_result(parsed, "fixture:a");

        assert_eq!(result.parser_backend, "not_parsed");
        assert_eq!(result.worker_lane, "not_parsed");
        assert_eq!(result.parse_transport, ParseTransport::NotApplicable);
        assert_eq!(result.parse_attempt_count, 0);
        assert_eq!(result.primary_duration_ms, 0);
        assert_eq!(result.fallback_duration_ms, 0);
        assert_eq!(result.parse_duration_ms, 0);
    }
}
