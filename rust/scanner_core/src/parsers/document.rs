use std::ffi::OsString;
use std::path::PathBuf;

use ai_daily_scanner_contract::{
    AdapterPaths, WorkerKind, WorkerParseRequest, WorkerParseResponse,
};

use crate::fallback::ParseFailure;

use super::{execute_worker_request, RegisteredWorker, WorkerCommand};

pub fn worker_command(adapters: &AdapterPaths) -> WorkerCommand {
    WorkerCommand {
        program: PathBuf::from(&adapters.python_executable),
        base_args: vec![
            OsString::from("-m"),
            OsString::from(&adapters.python_document_worker_module),
        ],
        current_dir: Some(PathBuf::from(&adapters.python_module_root)),
        expected_kind: WorkerKind::PythonDocument,
        required_backends: vec![
            "pdf_text_v1".to_string(),
            "python_office_v1".to_string(),
            "python_sharepoint_text_v1".to_string(),
        ],
        required_extensions: vec![
            ".doc".to_string(),
            ".docx".to_string(),
            ".pdf".to_string(),
            ".ppt".to_string(),
            ".pptx".to_string(),
            ".xls".to_string(),
            ".xlsx".to_string(),
        ],
    }
}

pub fn parse(
    worker: &RegisteredWorker,
    request: &WorkerParseRequest,
) -> Result<WorkerParseResponse, ParseFailure> {
    execute_worker_request(worker, request)
}
