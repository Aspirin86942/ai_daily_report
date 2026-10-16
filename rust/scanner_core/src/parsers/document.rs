use super::WorkerCommand;
use ai_daily_scanner_contract::AdapterPaths;
use ai_daily_worker_contract::WorkerKind;
use std::path::PathBuf;

pub fn worker_command(adapters: &AdapterPaths) -> WorkerCommand {
    WorkerCommand {
        program: PathBuf::from(&adapters.pdf_worker_path),
        base_args: Vec::new(),
        current_dir: None,
        expected_kind: WorkerKind::Pdf,
        required_backends: vec!["rust_pdfium_v3".into()],
        required_extensions: vec![".pdf".into()],
    }
}
