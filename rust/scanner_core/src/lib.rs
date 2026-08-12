//! Windows-first scanner engine process shell.

pub mod admission;
pub mod artifact;
pub mod budget_model;
pub mod classifier;
pub mod compressor;
pub mod config;
pub mod context_audit;
pub mod deadline;
pub mod decision;
mod evidence;
pub mod fallback;
mod identity;
pub mod metrics;
pub mod nominal;
pub mod parsers;
pub mod planner;
pub mod process;
mod run;
mod scanner;
pub mod scheduler;
pub mod scheduler_adapter;
pub mod session;
pub mod source_guard;
pub mod store;

#[cfg(windows)]
mod windows_job;

pub mod discovery {
    pub use ai_daily_discovery::*;
}

pub use scanner::{
    ContextResult, ScanRequest, Scanner, ScannerConfig, ScannerError, ScannerOperation,
};

pub const ENGINE_BUILD_IDENTITY: &str = env!("AI_DAILY_ENGINE_BUILD");

pub fn engine_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
