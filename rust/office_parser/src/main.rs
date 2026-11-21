use ai_daily_office_parser::{parse_archive_request, parse_worker_request, worker_hello};
use ai_daily_worker_contract::{
    archive::{ArchiveRequest, ArchiveTextRequest},
    stdio::{self, handle_payload, request_error, OperationResult},
    ParseRequest, WorkerKind, WorkerOperation,
};
use serde_json::Value;

fn main() {
    std::process::exit(stdio::run(
        worker_hello(),
        env!("AI_DAILY_PARSER_BUILD"),
        env!("AI_DAILY_ENGINE_BUILD"),
        include_str!(concat!(env!("OUT_DIR"), "/parser-build.json")),
        || Some(handle),
    ));
}

fn handle(operation: WorkerOperation, payload: Value) -> Option<OperationResult> {
    match operation {
        WorkerOperation::OfficeParse => {
            handle_payload(payload, ParseRequest::validate, parse_worker_request)
        }
        WorkerOperation::ArchiveOfficeParse => {
            handle_payload(payload, ArchiveRequest::validate, parse_archive_request)
        }
        WorkerOperation::ArchiveTextParse => {
            handle_payload(payload, ArchiveTextRequest::validate, |input| {
                ai_daily_text_parser::parse_archive_text(input)
                    .map_err(|code| request_error(WorkerKind::Office, code))
            })
        }
        _ => Some(Err(request_error(
            WorkerKind::Office,
            "UNSUPPORTED_OPERATION",
        ))),
    }
}
