//! One hello frame followed by one NDJSON response per valid request envelope.
use crate::{
    archive::ArchiveArtifact, ClassifyResult, ParseResult, WorkerDiagnostic, WorkerHello,
    WorkerKind, WorkerOperation, WorkerRequest, WorkerResponseStatus, CONTRACT, PROTOCOL_VERSION,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, Write};

/// Includes the terminating newline, like the scanner's stdout capture limit.
pub const MAX_RESPONSE_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Encode the entire frame before touching the pipe. A failed encode leaves no
/// partial JSON on the wire and the allocation is reused by the next frame.
pub struct FrameBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl FrameBuffer {
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    pub fn encode(&mut self, payload: &impl Serialize) -> serde_json::Result<()> {
        self.bytes.clear();
        if let Err(error) = serde_json::to_writer(&mut *self, payload) {
            self.bytes.clear();
            return Err(error);
        }
        self.bytes.push(b'\n');
        Ok(())
    }

    pub fn write_to(&self, output: &mut impl Write) -> io::Result<()> {
        output.write_all(&self.bytes)?;
        output.flush()
    }
}

impl Write for FrameBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let needed = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_add(1);
        if needed > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "worker frame exceeds its byte limit",
            ));
        }
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .max(4096);
            self.bytes
                .reserve_exact(capacity.min(self.limit) - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum OperationOutput {
    Parse(Box<ParseResult>),
    Classify(ClassifyResult),
    Archive(ArchiveArtifact),
}

impl From<ParseResult> for OperationOutput {
    fn from(value: ParseResult) -> Self {
        Self::Parse(Box::new(value))
    }
}

impl From<ClassifyResult> for OperationOutput {
    fn from(value: ClassifyResult) -> Self {
        Self::Classify(value)
    }
}

impl From<ArchiveArtifact> for OperationOutput {
    fn from(value: ArchiveArtifact) -> Self {
        Self::Archive(value)
    }
}

pub type OperationResult = Result<OperationOutput, Box<WorkerDiagnostic>>;

/// An invalid payload returns None. A parser diagnostic is a completed operation.
pub fn handle_payload<I: DeserializeOwned, O: Into<OperationOutput>>(
    payload: Value,
    validate: fn(&I) -> Result<(), String>,
    handle: impl FnOnce(&I) -> Result<O, Box<WorkerDiagnostic>>,
) -> Option<OperationResult> {
    let input = serde_json::from_value(payload).ok()?;
    validate(&input).ok()?;
    Some(handle(&input).map(Into::into))
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
    let mut frame = FrameBuffer::new(MAX_RESPONSE_FRAME_BYTES);
    if args == ["parser-build"] {
        let build = serde_json::json!({
            "parser_content_identity": parser_identity,
            "engine_build": engine_build,
            "inputs": serde_json::from_str::<Value>(inputs).expect("compiled parser inputs"),
        });
        return emit(kind, &mut frame, &mut io::stdout().lock(), &build).map_or(1, |()| 0);
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
    if emit(kind, &mut frame, &mut io::stdout().lock(), &hello).is_err() {
        return 1;
    }
    if args == ["hello"] {
        return 0;
    }
    serve_session(
        kind,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &mut frame,
        handle,
    )
}

#[derive(Serialize)]
struct Response<'a> {
    contract: &'static str,
    protocol_version: u64,
    request_id: &'a str,
    operation: WorkerOperation,
    status: WorkerResponseStatus,
    result: Option<OperationOutput>,
    error: Option<Box<WorkerDiagnostic>>,
}

fn serve_session(
    kind: WorkerKind,
    input: &mut impl BufRead,
    output: &mut impl Write,
    frame: &mut FrameBuffer,
    handle: impl Fn(WorkerOperation, Value) -> Option<OperationResult>,
) -> i32 {
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) => return 0,
            Ok(_) => {}
            Err(_) => return 2,
        }
        // Keep the Value on input: v4 rejects duplicate envelope fields
        // and uses the last occurrence of a key within a payload.
        let Ok(request) = serde_json::from_str::<WorkerRequest>(&line) else {
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
            Err(error) => (WorkerResponseStatus::Error, None, Some(error)),
        };
        let response = Response {
            contract: CONTRACT,
            protocol_version: PROTOCOL_VERSION,
            request_id: &request.request_id,
            operation,
            status,
            result,
            error,
        };
        if emit(kind, frame, output, &response).is_err() {
            return 1;
        }
        if close {
            return 2;
        }
    }
}

fn emit(
    kind: WorkerKind,
    frame: &mut FrameBuffer,
    output: &mut impl Write,
    payload: &impl Serialize,
) -> io::Result<()> {
    frame.encode(payload)?;
    output.write_all(&frame.bytes)?;
    let flushed = output.flush();
    if kind == WorkerKind::Pdf {
        flushed
    } else {
        // Preserve the Office CLI's handling of flush errors.
        Ok(())
    }
}

#[cfg(test)]
mod tests;
