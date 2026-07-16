use std::path::PathBuf;
use std::time::{Duration, Instant};

use ai_daily_scanner_contract::{
    AdapterPaths, FallbackBackend, OfficeParseProfile, WorkerBackend, WorkerKind,
    WorkerParseRequest, WorkerParseResponse,
};

use crate::fallback::{permits_office_fallback, ParseFailure};

use super::{execute_worker_request, next_request_id, RegisteredWorker, WorkerCommand};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficeParseExecution {
    pub response: Option<WorkerParseResponse>,
    pub primary_failure: Option<ParseFailure>,
    pub final_failure: Option<ParseFailure>,
    pub fallback_backend: Option<WorkerBackend>,
    pub primary_duration_ms: u64,
    pub fallback_duration_ms: u64,
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
    let primary_started = Instant::now();
    match execute_worker_request(primary_worker, request) {
        Ok(response) => OfficeParseExecution {
            response: Some(response),
            primary_failure: None,
            final_failure: None,
            fallback_backend: None,
            primary_duration_ms: elapsed_ms(primary_started),
            fallback_duration_ms: 0,
            partial: false,
        },
        Err(primary_failure) => {
            let primary_duration_ms = elapsed_ms(primary_started);
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
                    primary_duration_ms,
                    fallback_duration_ms: 0,
                    partial: true,
                };
            };

            let mut fallback_request = request.clone();
            fallback_request.request_id = next_request_id();
            fallback_request.backend = WorkerBackend::PythonOfficeV1;
            fallback_request.remaining_timeout_ms = u64::try_from(remaining.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            let fallback_started = Instant::now();
            match execute_worker_request(fallback_worker, &fallback_request) {
                Ok(response) => OfficeParseExecution {
                    response: Some(response),
                    primary_failure: Some(primary_failure),
                    final_failure: None,
                    fallback_backend: Some(WorkerBackend::PythonOfficeV1),
                    primary_duration_ms,
                    fallback_duration_ms: elapsed_ms(fallback_started),
                    partial: true,
                },
                Err(final_failure) => OfficeParseExecution {
                    response: None,
                    primary_failure: Some(primary_failure),
                    final_failure: Some(final_failure),
                    fallback_backend: Some(WorkerBackend::PythonOfficeV1),
                    primary_duration_ms,
                    fallback_duration_ms: elapsed_ms(fallback_started),
                    partial: true,
                },
            }
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
