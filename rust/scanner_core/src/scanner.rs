//! In-process scanner interface.
//!
//! The command-line process is a temporary adapter around this module.  New
//! callers should use [`Scanner`] and receive typed results instead of learning
//! the JSON/stdin process protocol.

use std::path::Path;

use ai_daily_scanner_contract::{
    BuildContextRequest, ContextEnvelope, DoctorRequest, DoctorResponse, InspectRunRequest,
    InspectRunResponseV2,
};
use serde::Serialize;

use crate::inspect::assemble_inspect_v2;
use crate::run::{build_context_command, doctor_command, CommandOutput, EngineShellError};
use crate::store::ScannerStore;

/// One context build and the complete evidence committed for that run.
#[derive(Debug, Serialize)]
pub struct ContextResult {
    pub envelope: ContextEnvelope,
    pub evidence: Option<InspectRunResponseV2>,
}

/// A completed scanner operation and its legacy command exit semantics.
#[derive(Debug)]
pub struct ScannerOperation<T> {
    pub value: T,
    pub exit_code: i32,
}

impl<T> ScannerOperation<T> {
    fn from_command(output: CommandOutput) -> Result<Self, EngineShellError>
    where
        T: serde::de::DeserializeOwned,
    {
        Ok(Self {
            value: serde_json::from_str(&output.json)?,
            exit_code: output.exit_code,
        })
    }
}

/// The scanner's sole in-process interface.
///
/// Configuration is still carried by the frozen request while the legacy CLI
/// adapter exists.  A later native-only step moves stable runtime configuration
/// into `Scanner::open` without changing this seam again for callers.
#[derive(Debug, Default)]
pub struct Scanner;

impl Scanner {
    pub fn build_context(
        &self,
        request: &BuildContextRequest,
    ) -> Result<ScannerOperation<ContextResult>, EngineShellError> {
        let operation: ScannerOperation<ContextEnvelope> =
            ScannerOperation::from_command(build_context_command(request)?)?;
        let evidence = match operation.value.scan_run_id.0 {
            Some(scan_run_id) => Some(load_run_evidence(request, scan_run_id)?),
            None => None,
        };
        Ok(ScannerOperation {
            value: ContextResult {
                envelope: operation.value,
                evidence,
            },
            exit_code: operation.exit_code,
        })
    }

    pub fn doctor(
        &self,
        request: &DoctorRequest,
    ) -> Result<ScannerOperation<DoctorResponse>, EngineShellError> {
        ScannerOperation::from_command(doctor_command(request)?)
    }
}

fn load_run_evidence(
    request: &BuildContextRequest,
    scan_run_id: u64,
) -> Result<InspectRunResponseV2, EngineShellError> {
    let inspect_request = InspectRunRequest {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        scan_db_path: request.scan_db_path.clone(),
        scan_run_id,
        include_content: false,
    };
    let mut store = ScannerStore::open_existing(Path::new(&request.scan_db_path))
        .map_err(|error| EngineShellError::Evidence(error.to_string()))?;
    let snapshot = store
        .inspect_run(scan_run_id, false)
        .map_err(|error| EngineShellError::Evidence(error.error.to_string()))?;
    assemble_inspect_v2(&inspect_request, &snapshot).map_err(EngineShellError::Evidence)
}
