use ai_daily_pdf_parser::{worker_hello, PdfReader, PdfSession};
use ai_daily_worker_contract::{
    archive::ArchiveRequest,
    stdio::{self, handle_payload, OperationResult},
    ClassifyRequest, ParseRequest, WorkerOperation,
};
use serde_json::Value;
use std::cell::{OnceCell, RefCell};

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let reader = OnceCell::new();
    stdio::run(
        worker_hello(),
        env!("AI_DAILY_PARSER_BUILD"),
        env!("AI_DAILY_ENGINE_BUILD"),
        include_str!(concat!(env!("OUT_DIR"), "/parser-build.json")),
        || {
            let program = std::env::current_exe().ok()?;
            let pdf = PdfReader::open(program.parent().expect("executable directory"))
                .inspect_err(|error| eprintln!("{error}"))
                .ok()?;
            reader.set(pdf).ok()?;
            let session = RefCell::new(reader.get()?.session());
            Some(move |operation, payload| handle(&mut session.borrow_mut(), operation, payload))
        },
    )
}

fn handle(
    session: &mut PdfSession<'_>,
    operation: WorkerOperation,
    payload: Value,
) -> Option<OperationResult> {
    match operation {
        WorkerOperation::PdfParse => handle_payload(payload, ParseRequest::validate, |input| {
            session.parse_request(input)
        }),
        WorkerOperation::PdfClassify => {
            handle_payload(payload, ClassifyRequest::validate, |input| {
                Ok(session.classify_request(input))
            })
        }
        WorkerOperation::ArchivePdfParse => {
            handle_payload(payload, ArchiveRequest::validate, |input| {
                session.archive_request(input)
            })
        }
        _ => None,
    }
}
