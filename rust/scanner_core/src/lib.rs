//! Windows-first scanner engine process shell.

pub mod admission;
pub mod artifact;
pub mod budget_model;
pub mod classifier;
pub mod compressor;
pub mod config;
pub mod context_audit;
pub mod decision;
pub mod fallback;
pub mod inspect;
pub mod metrics;
pub mod nominal;
pub mod parsers;
pub mod planner;
pub mod process;
mod run;
pub mod scheduler;
pub mod scheduler_adapter;
pub mod source_guard;
pub mod store;

#[cfg(windows)]
mod windows_job;

pub mod discovery {
    pub use ai_daily_discovery::*;
}

pub use run::{
    dispatch, dispatch_with_response_version, invalid_request_output, version_response,
    version_response_v2, CommandOutput, EngineShellError,
};
