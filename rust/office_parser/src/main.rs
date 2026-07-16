use std::io::{self, Read, Write};

use ai_daily_office_parser::{
    parse_office_file, parse_worker_request, worker_version_response, OfficeParseRequest,
};
use ai_daily_scanner_contract::{
    Diagnostic, DiagnosticStage, ErrorCode, Nullable, TransportErrorResponse, Validate,
    WorkerParseRequest, WorkerStatus,
};
use serde::Serialize;

fn main() {
    std::process::exit(dispatch());
}

fn dispatch() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["version"] {
        return emit(&worker_version_response()).map_or(1, |()| 0);
    }
    if args == ["parse"] {
        return strict_worker_parse();
    }
    if args.is_empty() {
        return legacy_parse();
    }
    eprintln!("usage: ai-daily-office-parser [version|parse]");
    1
}

fn strict_worker_parse() -> i32 {
    let mut input = Vec::new();
    let request = match io::stdin()
        .read_to_end(&mut input)
        .map_err(|error| error.to_string())
        .and_then(|_| {
            serde_json::from_slice::<WorkerParseRequest>(&input).map_err(|e| e.to_string())
        })
        .and_then(|request| request.validate().map(|()| request))
    {
        Ok(request) => request,
        Err(_) => {
            let _ = emit(&invalid_request_response());
            return 2;
        }
    };
    let response = parse_worker_request(&request);
    let exit_code = if response.status == WorkerStatus::Ok {
        0
    } else {
        1
    };
    if emit(&response).is_err() {
        return 1;
    }
    exit_code
}

fn legacy_parse() -> i32 {
    let mut input = String::new();
    let result = io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| error.to_string())
        .and_then(|_| {
            serde_json::from_str::<OfficeParseRequest>(&input).map_err(|e| e.to_string())
        });
    let request = match result {
        Ok(request) => request,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    emit(&parse_office_file(&request)).map_or(1, |()| 0)
}

fn invalid_request_response() -> TransportErrorResponse {
    TransportErrorResponse {
        contract: "ai_daily_transport".to_string(),
        protocol_version: 1,
        status: "error".to_string(),
        error: Diagnostic {
            error_code: ErrorCode::InvalidRequest,
            message: "stdin is not a valid worker request".to_string(),
            retryable: false,
            stage: DiagnosticStage::Request,
            file_path: Nullable(None),
            backend: Nullable(None),
        },
    }
}

fn emit<T: Serialize>(payload: &T) -> Result<(), serde_json::Error> {
    let stdout = io::stdout();
    let mut locked = stdout.lock();
    serde_json::to_writer(&mut locked, payload)?;
    let _ = locked.write_all(b"\n");
    Ok(())
}
