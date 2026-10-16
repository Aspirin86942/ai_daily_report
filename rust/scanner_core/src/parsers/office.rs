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
        required_backends: vec![
            "rust_office_oxide_v2".to_string(),
            "rust_xlsx_bounded_v2".to_string(),
            "rust_xls_calamine_v3".to_string(),
            "rust_legacy_office_v3".to_string(),
        ],
        required_extensions: vec![
            ".docx".to_string(),
            ".pptx".to_string(),
            ".xlsx".to_string(),
            ".xls".to_string(),
            ".doc".to_string(),
            ".ppt".to_string(),
        ],
    }
}
