use std::io::{self, BufRead, Write};

use ai_daily_office_parser::{parse_worker_request, worker_hello};
use ai_daily_scanner_contract::{Validate, WorkerParseRequest};
use ai_daily_worker_contract::{
    WorkerDiagnostic, WorkerOperation, WorkerRequest, WorkerResponse, WorkerResponseStatus,
    CONTRACT, PROTOCOL_VERSION,
};
use serde::Serialize;

fn main() {
    std::process::exit(dispatch());
}

fn dispatch() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["hello"] {
        return emit(&worker_hello()).map_or(1, |()| 0);
    }
    if args == ["session"] {
        return worker_session();
    }
    eprintln!("usage: ai-daily-office-parser <hello|session>");
    1
}

fn worker_session() -> i32 {
    if emit(&worker_hello()).is_err() {
        return 1;
    }
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let request = match line
            .map_err(|error| error.to_string())
            .and_then(|line| {
                serde_json::from_str::<WorkerRequest>(&line).map_err(|e| e.to_string())
            })
            .and_then(|request| request.validate().map(|()| request))
        {
            Ok(request) => request,
            Err(_) => return 2,
        };
        if request.operation != WorkerOperation::OfficeParse {
            if emit(&session_error(&request, "UNSUPPORTED_OPERATION", false)).is_err() {
                return 1;
            }
            continue;
        }
        let parse_request =
            match serde_json::from_value::<WorkerParseRequest>(request.payload.clone())
                .map_err(|error| error.to_string())
                .and_then(|request| request.validate().map(|()| request))
            {
                Ok(parse_request) if parse_request.request_id == request.request_id => {
                    parse_request
                }
                _ => {
                    if emit(&session_error(&request, "INVALID_REQUEST", false)).is_err() {
                        return 1;
                    }
                    return 2;
                }
            };
        let parsed = parse_worker_request(&parse_request);
        let result = serde_json::to_value(parsed).expect("parse response must serialize");
        let response = WorkerResponse {
            contract: CONTRACT.to_string(),
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            operation: request.operation,
            status: WorkerResponseStatus::Ok,
            result: Some(result),
            error: None,
        };
        if emit(&response).is_err() {
            return 1;
        }
    }
    0
}

fn session_error(request: &WorkerRequest, error_code: &str, retryable: bool) -> WorkerResponse {
    WorkerResponse {
        contract: CONTRACT.to_string(),
        protocol_version: PROTOCOL_VERSION,
        request_id: request.request_id.clone(),
        operation: request.operation,
        status: WorkerResponseStatus::Error,
        result: None,
        error: Some(WorkerDiagnostic {
            error_code: error_code.to_string(),
            message: "worker request was rejected".to_string(),
            retryable,
            stage: "request".to_string(),
            file_path: None,
            backend: None,
        }),
    }
}

fn emit<T: Serialize>(payload: &T) -> Result<(), serde_json::Error> {
    let stdout = io::stdout();
    let mut locked = stdout.lock();
    serde_json::to_writer(&mut locked, payload)?;
    let _ = locked.write_all(b"\n");
    let _ = locked.flush();
    Ok(())
}
