//! In-process scanner interface.
//!
//! The command-line process is a temporary adapter around this module.  New
//! callers should use [`Scanner`] and receive typed results instead of learning
//! the JSON/stdin process protocol.

use ai_daily_scanner_contract::{
    BuildContextRequest, ContextEnvelope, DoctorRequest, DoctorResponse,
};

use crate::run::{build_context_command, doctor_command, CommandOutput, EngineShellError};

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
    ) -> Result<ScannerOperation<ContextEnvelope>, EngineShellError> {
        ScannerOperation::from_command(build_context_command(request)?)
    }

    pub fn doctor(
        &self,
        request: &DoctorRequest,
    ) -> Result<ScannerOperation<DoctorResponse>, EngineShellError> {
        ScannerOperation::from_command(doctor_command(request)?)
    }
}
