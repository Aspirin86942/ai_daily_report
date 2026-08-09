use std::path::PathBuf;
use std::time::{Duration, Instant};

use ai_daily_scanner_contract::{
    AdapterPaths, FallbackBackend, OfficeParseProfile, WorkerBackend, WorkerKind,
    WorkerParseRequest, WorkerParseResponse,
};

use crate::fallback::{permits_office_fallback, ParseFailure};

use super::{execute_worker_request_observed, next_request_id, RegisteredWorker, WorkerCommand};

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
            "rust_office_oxide_v1".to_string(),
            "rust_xlsx_bounded_v1".to_string(),
        ],
        required_extensions: vec![
            ".docx".to_string(),
            ".pptx".to_string(),
            ".xlsx".to_string(),
        ],
    }
}

pub fn parse_with_fallback(
    primary_worker: &RegisteredWorker,
    fallback_worker: Option<&RegisteredWorker>,
    request: &WorkerParseRequest,
    profile: &OfficeParseProfile,
) -> OfficeParseExecution {
    let deadline = Instant::now() + Duration::from_millis(request.remaining_timeout_ms);
    let primary = execute_worker_request_observed(primary_worker, request);
    let primary_duration_ms = primary.duration_ms;
    let primary_attempt_count = primary.attempt_count;
    match primary.outcome {
        Ok(response) => OfficeParseExecution {
            response: Some(response),
            primary_failure: None,
            final_failure: None,
            fallback_backend: None,
            last_started_backend: (primary_attempt_count > 0).then_some(request.backend),
            primary_duration_ms,
            fallback_duration_ms: 0,
            attempt_count: primary_attempt_count,
            partial: false,
        },
        Err(primary_failure) => {
            let fallback_allowed = permits_office_fallback(&primary_failure, profile)
                && profile
                    .fallback_order
                    .contains(&FallbackBackend::PythonOfficeV1)
                && WorkerBackend::PythonOfficeV1.supports(&request.file_type);
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(fallback_worker) = fallback_worker
                .filter(|_| fallback_allowed && remaining >= Duration::from_millis(1))
            else {
                return OfficeParseExecution {
                    response: None,
                    final_failure: Some(primary_failure),
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
            fallback_request.backend = WorkerBackend::PythonOfficeV1;
            fallback_request.remaining_timeout_ms = u64::try_from(remaining.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            let fallback = execute_worker_request_observed(fallback_worker, &fallback_request);
            let fallback_duration_ms = fallback.duration_ms;
            let attempt_count = primary_attempt_count.saturating_add(fallback.attempt_count);
            let last_started_backend = if fallback.attempt_count > 0 {
                Some(WorkerBackend::PythonOfficeV1)
            } else if primary_attempt_count > 0 {
                Some(request.backend)
            } else {
                None
            };
            match fallback.outcome {
                Ok(response) => OfficeParseExecution {
                    response: Some(response),
                    primary_failure: Some(primary_failure),
                    final_failure: None,
                    fallback_backend: Some(WorkerBackend::PythonOfficeV1),
                    last_started_backend,
                    primary_duration_ms,
                    fallback_duration_ms,
                    attempt_count,
                    partial: true,
                },
                Err(final_failure) => OfficeParseExecution {
                    response: None,
                    primary_failure: Some(primary_failure),
                    final_failure: Some(final_failure),
                    fallback_backend: Some(WorkerBackend::PythonOfficeV1),
                    last_started_backend,
                    primary_duration_ms,
                    fallback_duration_ms,
                    attempt_count,
                    partial: true,
                },
            }
        }
    }
}
