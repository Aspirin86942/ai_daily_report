pub mod cli;
pub mod config;
pub mod evidence;
pub mod health;
pub mod infra;
pub mod maintenance;
pub mod model;
pub mod operations;
pub mod personal_recovery;
pub mod processing_cache;
pub mod publish;
pub mod reports;
pub mod run;
pub mod scan;
pub mod search;
pub mod snapshots;
pub mod store;
pub mod types;

#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ReportError {
    pub code: &'static str,
    pub message: String,
}

impl ReportError {
    pub fn retryable(&self) -> bool {
        matches!(
            self.code,
            // MODEL_CONNECTION_FAILED, MODEL_CONNECT_TIMEOUT, MODEL_RESPONSE_INTERRUPTED, and MODEL_TIMEOUT
            // are emitted dynamically by model::transport::TransportFailure::code().
            "REPORT_ALREADY_RUNNING"
                | "IO_FAILED"
                | "PROCESSING_READ_FAILED"
                | "MODEL_CONNECTION_FAILED"
                | "MODEL_CONNECT_TIMEOUT"
                | "MODEL_RESPONSE_INTERRUPTED"
                | "MODEL_TIMEOUT"
        )
    }
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for ReportError {
    fn from(value: std::io::Error) -> Self {
        Self::new("IO_FAILED", value.to_string())
    }
}

impl From<serde_json::Error> for ReportError {
    fn from(value: serde_json::Error) -> Self {
        Self::new("JSON_INVALID", value.to_string())
    }
}
