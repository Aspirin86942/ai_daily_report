pub mod archive;
pub mod cli;
pub mod config;
pub mod health;
pub mod maintenance;
pub mod model;
pub mod operations;
pub mod publish;
pub mod reports;
pub mod run;
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
