use std::path::PathBuf;
use std::time::{Duration, Instant};

use ai_daily_scanner_contract::{
    AdapterPaths, FallbackBackend, OfficeParseProfile, WorkerBackend, WorkerKind,
    WorkerParseRequest, WorkerParseResponse,
};

use crate::fallback::{permits_office_fallback, ParseFailure};
use crate::session::WorkerPool;

use super::{next_request_id, WorkerCommand};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficeParseExecution {
    pub response: Option<WorkerParseResponse>,
    pub primary_failure: Option<ParseFailure>,
    pub final_failure: Option<ParseFailure>,
    pub fallback_backend: Option<WorkerBackend>,
    /// Backend of the last parser child that actually started. A fallback
    /// rejected during pre-dispatch validation must not replace primary
    /// provenance.
    pub last_started_backend: Option<WorkerBackend>,
    pub primary_duration_ms: u64,
    pub fallback_duration_ms: u64,
    /// Actual parser process attempts: primary once, plus one when the
    /// configured fallback process was started.
    pub attempt_count: u64,
    pub partial: bool,
}
pub fn worker_command(adapters: &AdapterPaths) -> WorkerCommand {
    WorkerCommand {
        program: PathBuf::from(&adapters.office_worker_path),
        base_args: Vec::new(),
        current_dir: None,
        expected_kind: WorkerKind::Office,
        required_backends: vec![
            "rust_office_oxide_v2".to_string(),
            "rust_xlsx_bounded_v2".to_string(),
        ],
        required_extensions: vec![
            ".docx".to_string(),
            ".pptx".to_string(),
            ".xlsx".to_string(),
        ],
    }
}

pub fn parse_with_pools(
    primary_pool: &WorkerPool,
    fallback_pool: Option<&WorkerPool>,
    request: &WorkerParseRequest,
    profile: &OfficeParseProfile,
) -> OfficeParseExecution {
    let deadline = Instant::now() + Duration::from_millis(request.remaining_timeout_ms);
    let primary =
        primary_pool.parse_worker(request, Duration::from_millis(request.remaining_timeout_ms));
    let (primary_result, primary_duration_ms, primary_attempt_count) = match primary {
        Ok(outcome) => {
            return OfficeParseExecution {
                response: Some(outcome.value),
                primary_failure: None,
                final_failure: None,
                fallback_backend: None,
                last_started_backend: (outcome.attempt_count > 0).then_some(request.backend),
                primary_duration_ms: outcome.duration_ms,
                fallback_duration_ms: 0,
                attempt_count: outcome.attempt_count,
                partial: false,
            };
        }
        Err(failure) => (failure.failure, failure.duration_ms, failure.attempt_count),
    };
    let fallback_allowed = permits_office_fallback(&primary_result, profile)
        && profile
            .fallback_order
            .contains(&FallbackBackend::PythonOfficeV2)
        && WorkerBackend::PythonOfficeV2.supports(&request.file_type);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let Some(fallback_pool) =
        fallback_pool.filter(|_| fallback_allowed && remaining >= Duration::from_millis(1))
    else {
        return OfficeParseExecution {
            response: None,
            final_failure: Some(primary_result),
            primary_failure: None,
            fallback_backend: None,
            last_started_backend: (primary_attempt_count > 0).then_some(request.backend),
            primary_duration_ms,
            fallback_duration_ms: 0,
            attempt_count: primary_attempt_count,
            partial: true,
        };
    };
    let mut fallback_request = request.clone();
    fallback_request.request_id = next_request_id();
    fallback_request.backend = WorkerBackend::PythonOfficeV2;
    fallback_request.remaining_timeout_ms = u64::try_from(remaining.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    match fallback_pool.parse_worker(&fallback_request, remaining) {
        Ok(outcome) => OfficeParseExecution {
            response: Some(outcome.value),
            primary_failure: Some(primary_result),
            final_failure: None,
            fallback_backend: Some(WorkerBackend::PythonOfficeV2),
            last_started_backend: Some(WorkerBackend::PythonOfficeV2),
            primary_duration_ms,
            fallback_duration_ms: outcome.duration_ms,
            attempt_count: primary_attempt_count.saturating_add(outcome.attempt_count),
            partial: true,
        },
        Err(failure) => OfficeParseExecution {
            response: None,
            primary_failure: Some(primary_result),
            final_failure: Some(failure.failure),
            fallback_backend: Some(WorkerBackend::PythonOfficeV2),
            last_started_backend: (failure.attempt_count > 0)
                .then_some(WorkerBackend::PythonOfficeV2)
                .or((primary_attempt_count > 0).then_some(request.backend)),
            primary_duration_ms,
            fallback_duration_ms: failure.duration_ms,
            attempt_count: primary_attempt_count.saturating_add(failure.attempt_count),
            partial: true,
        },
    }
}
