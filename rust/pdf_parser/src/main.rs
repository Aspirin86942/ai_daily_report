use ai_daily_pdf_parser::{worker_hello, PdfReader};
use ai_daily_worker_contract::{
    archive::ArchiveRequest,
    stdio::{self, handle_payload, OperationResult},
    ClassifyRequest, ParseRequest, WorkerOperation,
};
use serde_json::Value;

fn main() {
    std::process::exit(stdio::run(
        worker_hello(),
        env!("AI_DAILY_PARSER_BUILD"),
        env!("AI_DAILY_ENGINE_BUILD"),
        include_str!(concat!(env!("OUT_DIR"), "/parser-build.json")),
        || {
            let program = std::env::current_exe().ok()?;
            let reader = PdfReader::open(program.parent().expect("executable directory"))
                .inspect_err(|error| eprintln!("{error}"))
                .ok()?;
            Some(move |operation, payload| handle(&reader, operation, payload))
        },
    ));
}

fn handle(
    reader: &PdfReader,
    operation: WorkerOperation,
    payload: Value,
) -> Option<OperationResult> {
    match operation {
        WorkerOperation::PdfParse => handle_payload(payload, ParseRequest::validate, |input| {
            reader.parse_request(input)
        }),
        WorkerOperation::PdfClassify => {
            handle_payload(payload, ClassifyRequest::validate, |input| {
                Ok(reader.classify_request(input))
            })
        }
        WorkerOperation::ArchivePdfParse => {
            handle_payload(payload, ArchiveRequest::validate, |input| {
                reader.archive_request(input)
            })
        }
        _ => None,
    }
}
