use ai_daily_pdf_parser::{worker_hello, PdfReader};
use ai_daily_worker_contract::*;
use std::io::{self, BufRead, Write};

fn main() {
    std::process::exit(dispatch());
}

fn dispatch() -> i32 {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args != ["hello"] && args != ["session"] {
        eprintln!("usage: ai-daily-pdf-parser <hello|session>");
        return 1;
    }
    let program = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return 1,
    };
    let reader = match PdfReader::open(program.parent().expect("executable directory")) {
        Ok(reader) => reader,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    if emit(&worker_hello()).is_err() {
        return 1;
    }
    if args == ["hello"] {
        return 0;
    }
    for line in io::stdin().lock().lines() {
        let request: WorkerRequest =
            match line.ok().and_then(|line| serde_json::from_str(&line).ok()) {
                Some(value) => value,
                None => return 2,
            };
        if request.validate().is_err() {
            return 2;
        }
        let result = match request.operation {
            WorkerOperation::ArchivePdfParse => {
                serde_json::from_value::<archive::ArchiveRequest>(request.payload.clone())
                    .ok()
                    .filter(|v| v.validate().is_ok())
                    .map(|v| {
                        reader
                            .archive_request(&v)
                            .map(|a| serde_json::to_value(a).unwrap())
                    })
            }
            WorkerOperation::PdfParse => {
                serde_json::from_value::<ParseRequest>(request.payload.clone())
                    .ok()
                    .filter(|value| value.validate().is_ok())
                    .map(|value| {
                        reader
                            .parse_request(&value)
                            .map(|value| serde_json::to_value(value).expect("typed result"))
                    })
            }
            WorkerOperation::PdfClassify => {
                serde_json::from_value::<ClassifyRequest>(request.payload.clone())
                    .ok()
                    .filter(|value| value.validate().is_ok())
                    .map(|value| {
                        Ok(serde_json::to_value(reader.classify_request(&value))
                            .expect("typed result"))
                    })
            }
            _ => None,
        };
        let invalid = result.is_none();
        let result = result.unwrap_or_else(|| {
            Err(Box::new(WorkerDiagnostic {
                error_code: "INVALID_REQUEST".into(),
                message: "PDF worker request was rejected".into(),
                retryable: false,
                stage: "request".into(),
                file_path: None,
                backend: None,
            }))
        });
        let (status, result, error) = match result {
            Ok(value) => (WorkerResponseStatus::Ok, Some(value), None),
            Err(error) => (WorkerResponseStatus::Error, None, Some(*error)),
        };
        if emit(&WorkerResponse {
            contract: CONTRACT.into(),
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            operation: request.operation,
            status,
            result,
            error,
        })
        .is_err()
        {
            return 1;
        }
        if invalid {
            return 2;
        }
    }
    0
}

fn emit(value: &impl serde::Serialize) -> io::Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, value)?;
    output.write_all(b"\n")?;
    output.flush()
}
