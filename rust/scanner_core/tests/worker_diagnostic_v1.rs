//! Frozen worker diagnostic adapter seam (spec Part 7.1).
//!
//! `WorkerDiagnosticV1` is the only diagnostic type that may deserialize on the
//! `ai_daily_worker_v1` wire; the scanner adapter explicitly translates it to the
//! scanner-side extended `Diagnostic` at the process seam.

use ai_daily_scanner_contract::{
    DiagnosticStage, ErrorCode, Nullable, WorkerDiagnosticV1, WorkerDiagnosticV1ErrorCode,
    WorkerDiagnosticV1Stage,
};
use ai_daily_scanner_core::parsers::worker_diagnostic_to_scanner;

#[test]
fn frozen_worker_diagnostic_translates_to_scanner_diagnostic() {
    let worker = WorkerDiagnosticV1 {
        error_code: WorkerDiagnosticV1ErrorCode::ParserTimeout,
        message: "worker deadline exceeded".to_string(),
        retryable: true,
        stage: WorkerDiagnosticV1Stage::Parse,
        file_path: Nullable(Some(
            "C:\\scanner-fixtures\\工作 目录\\slow.pdf".to_string(),
        )),
        backend: Nullable(Some("pdf_text_v1".to_string())),
    };

    let translated = worker_diagnostic_to_scanner(&worker);

    assert_eq!(translated.error_code, ErrorCode::ParserTimeout);
    assert_eq!(translated.stage, DiagnosticStage::Parse);
    assert_eq!(translated.message, "worker deadline exceeded");
    assert!(translated.retryable);
    assert_eq!(
        translated.file_path,
        Nullable(Some(
            "C:\\scanner-fixtures\\工作 目录\\slow.pdf".to_string()
        ))
    );
    assert_eq!(
        translated.backend,
        Nullable(Some("pdf_text_v1".to_string()))
    );
}

#[test]
fn every_frozen_worker_error_code_maps_onto_scanner_error_code() {
    // The 25-code frozen set is exactly the scanner-side ErrorCode subset that is
    // legal on the old worker wire; each must translate losslessly.
    for code in [
        WorkerDiagnosticV1ErrorCode::InvalidRequest,
        WorkerDiagnosticV1ErrorCode::ContractVersionMismatch,
        WorkerDiagnosticV1ErrorCode::WorkDirNotFound,
        WorkerDiagnosticV1ErrorCode::WorkDirNotDirectory,
        WorkerDiagnosticV1ErrorCode::DiscoveryEntryUnreadable,
        WorkerDiagnosticV1ErrorCode::FileTooLarge,
        WorkerDiagnosticV1ErrorCode::ParserStartFailed,
        WorkerDiagnosticV1ErrorCode::ParserTimeout,
        WorkerDiagnosticV1ErrorCode::ParserInvalidPayload,
        WorkerDiagnosticV1ErrorCode::ParserFailed,
        WorkerDiagnosticV1ErrorCode::WorkerHandshakeFailed,
        WorkerDiagnosticV1ErrorCode::WorkerVersionMismatch,
        WorkerDiagnosticV1ErrorCode::WorkerBuildChanged,
        WorkerDiagnosticV1ErrorCode::SourceVersionChanged,
        WorkerDiagnosticV1ErrorCode::CacheOpenFailed,
        WorkerDiagnosticV1ErrorCode::CacheWriteFailed,
        WorkerDiagnosticV1ErrorCode::ScanAlreadyRunning,
        WorkerDiagnosticV1ErrorCode::RequestInProgress,
        WorkerDiagnosticV1ErrorCode::RequestIdConflict,
        WorkerDiagnosticV1ErrorCode::RunNotFound,
        WorkerDiagnosticV1ErrorCode::RunCorrupt,
        WorkerDiagnosticV1ErrorCode::ContextBudgetInvalid,
        WorkerDiagnosticV1ErrorCode::NotImplemented,
        WorkerDiagnosticV1ErrorCode::RustCoreCrashed,
        WorkerDiagnosticV1ErrorCode::InternalError,
    ] {
        let worker = WorkerDiagnosticV1 {
            error_code: code,
            message: "synthetic".to_string(),
            retryable: false,
            stage: WorkerDiagnosticV1Stage::Parse,
            file_path: Nullable(None),
            backend: Nullable(None),
        };
        let translated = worker_diagnostic_to_scanner(&worker);
        // The translated scanner-side code must serialize to the same wire text.
        assert_eq!(
            serde_json::to_string(&translated.error_code).unwrap(),
            serde_json::to_string(&code).unwrap(),
        );
    }
}
