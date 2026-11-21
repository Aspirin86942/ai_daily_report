//! One hello frame followed by one NDJSON response per valid request envelope.
use crate::{
    WorkerDiagnostic, WorkerHello, WorkerKind, WorkerOperation, WorkerRequest, WorkerResponse,
    WorkerResponseStatus, CONTRACT, PROTOCOL_VERSION,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, Write};

pub type OperationResult = Result<Value, Box<WorkerDiagnostic>>;

/// An invalid payload returns None. A parser diagnostic is a completed operation.
pub fn handle_payload<I: DeserializeOwned, O: Serialize>(
    payload: Value,
    validate: fn(&I) -> Result<(), String>,
    handle: impl FnOnce(&I) -> Result<O, Box<WorkerDiagnostic>>,
) -> Option<OperationResult> {
    let input = serde_json::from_value(payload).ok()?;
    validate(&input).ok()?;
    Some(handle(&input).map(|output| serde_json::to_value(output).expect("typed worker result")))
}

pub fn request_error(kind: WorkerKind, code: &str) -> Box<WorkerDiagnostic> {
    Box::new(WorkerDiagnostic {
        error_code: code.into(),
        message: match kind {
            WorkerKind::Office => "worker request was rejected",
            WorkerKind::Pdf => "PDF worker request was rejected",
        }
        .into(),
        retryable: false,
        stage: "request".into(),
        file_path: None,
        backend: None,
    })
}

pub fn run<H>(
    hello: WorkerHello,
    parser_identity: &str,
    engine_build: &str,
    inputs: &str,
    open: impl FnOnce() -> Option<H>,
) -> i32
where
    H: Fn(WorkerOperation, Value) -> Option<OperationResult>,
{
    let kind = hello.worker_kind;
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["parser-build"] {
        let build = serde_json::json!({
            "parser_content_identity": parser_identity,
            "engine_build": engine_build,
            "inputs": serde_json::from_str::<Value>(inputs).expect("compiled parser inputs"),
        });
        return emit(kind, &build).map_or(1, |()| 0);
    }
    if args != ["hello"] && args != ["session"] {
        let name = match kind {
            WorkerKind::Office => "office",
            WorkerKind::Pdf => "pdf",
        };
        eprintln!("usage: ai-daily-{name}-parser <hello|session>");
        return 1;
    }
    let Some(handle) = open() else { return 1 };
    if emit(kind, &hello).is_err() {
        return 1;
    }
    if args == ["hello"] {
        return 0;
    }
    for line in io::stdin().lock().lines() {
        let Some(request) = line
            .ok()
            .and_then(|line| serde_json::from_str::<WorkerRequest>(&line).ok())
        else {
            return 2;
        };
        if request.validate().is_err() {
            return 2;
        }
        let operation = request.operation;
        let (outcome, close) = match handle(operation, request.payload) {
            Some(outcome) => (outcome, false),
            // Office archive rejections leave the session usable. PDF and OfficeParse
            // reject a bad payload with one response before exiting with status 2.
            None => (
                Err(request_error(kind, "INVALID_REQUEST")),
                kind == WorkerKind::Pdf || operation == WorkerOperation::OfficeParse,
            ),
        };
        let (status, result, error) = match outcome {
            Ok(value) => (WorkerResponseStatus::Ok, Some(value), None),
            Err(error) => (WorkerResponseStatus::Error, None, Some(*error)),
        };
        let response = WorkerResponse {
            contract: CONTRACT.into(),
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            operation,
            status,
            result,
            error,
        };
        if emit(kind, &response).is_err() {
            return 1;
        }
        if close {
            return 2;
        }
    }
    0
}

fn emit(kind: WorkerKind, payload: &impl Serialize) -> io::Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, payload)?;
    let newline = output.write_all(b"\n");
    if kind == WorkerKind::Pdf {
        newline?;
        output.flush()
    } else {
        // The Office CLI historically reports only errors writing the JSON itself.
        let _ = output.flush();
        Ok(())
    }
}
