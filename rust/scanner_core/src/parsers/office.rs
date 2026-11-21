use super::WorkerCommand;
use ai_daily_scanner_contract::AdapterPaths;
use ai_daily_worker_contract::WorkerKind;
use std::path::PathBuf;

pub fn worker_command(adapters: &AdapterPaths) -> WorkerCommand {
    WorkerCommand {
        program: PathBuf::from(&adapters.office_worker_path),
        base_args: Vec::new(),
        current_dir: None,
        expected_kind: WorkerKind::Office,
        required_backends: [
            "rust_office_oxide_v2",
            "rust_xlsx_bounded_v2",
            "rust_xls_calamine_v3",
            "rust_legacy_office_v3",
        ]
        .map(str::to_owned)
        .into(),
        required_extensions: [".docx", ".pptx", ".xlsx", ".xls", ".doc", ".ppt"]
            .map(str::to_owned)
            .into(),
    }
}
