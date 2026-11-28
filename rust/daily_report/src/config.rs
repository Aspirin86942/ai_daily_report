use crate::ReportError;
mod evidence;
mod load;
mod validate;
use chrono::NaiveTime;
use daily_parsers::settings::ScannerSettings;
pub use evidence::ArchiveSettings;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub work_dir: PathBuf,
    pub data_dir: PathBuf,
    pub reports_dir: PathBuf,
    pub db_dir: PathBuf,
    pub log_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Llm {
    pub provider: String,
    pub model_id: String,
    pub base_url: String,
    pub temperature: f64,
    pub max_tokens: u64,
    pub facts_max_tokens: u64,
    pub max_retries: u32,
    pub request_timeout_seconds: f64,
    pub response_format: String,
    pub thinking_mode: Option<String>,
    pub thinking_parameter_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub max_request_utf8_bytes: usize,
    pub max_sources_per_batch: usize,
    pub max_batches: usize,
    pub context_window_tokens: Option<u64>,
    pub verified_max_output_tokens: Option<u64>,
    pub truncation_policy: String,
    pub max_strategy_changes: u32,
    #[serde(default = "default_invalid_response_retries")]
    pub invalid_response_retries: u32,
}

fn default_invalid_response_retries() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportSettings {
    #[serde(default, skip_serializing_if = "ReportProfile::is_evidence")]
    pub profile: ReportProfile,
    pub scope_id: String,
    pub root_alias: String,
    pub timezone: String,
    pub default_target: String,
    pub notes_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportProfile {
    #[default]
    Evidence,
    Personal,
}

impl ReportProfile {
    pub fn is_evidence(&self) -> bool {
        *self == Self::Evidence
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    #[serde(default = "default_model_workers")]
    pub model_workers: u8,
    pub missed_days_policy: String,
    pub run_budget_seconds: u64,
    pub lock_wait_seconds: f64,
}

fn default_model_workers() -> u8 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub input_snapshot_days: u64,
    pub terminal_run_days: u64,
    pub snapshot_max_mib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub task_name: String,
    pub local_time: String,
    pub logon_catchup: bool,
    pub wake_to_run: bool,
    pub allow_on_battery: bool,
    pub stop_on_battery: bool,
    pub restart_count: u32,
    pub restart_interval_minutes: u32,
    pub execution_limit_minutes: u32,
}

impl Schedule {
    pub fn due_time(&self) -> Result<NaiveTime, ReportError> {
        let time = NaiveTime::parse_from_str(&self.local_time, "%H:%M")
            .map_err(|_| invalid("schedule.local_time"))?;
        if time.format("%H:%M").to_string() != self.local_time {
            return Err(invalid("schedule.local_time"));
        }
        Ok(time)
    }
}

#[derive(Clone)]
pub struct AppConfig {
    pub program_dir: PathBuf,
    pub config_dir: PathBuf,
    pub paths: Paths,
    pub llm: Llm,
    pub report: ReportSettings,
    pub recovery: Recovery,
    pub retention: Retention,
    pub schedule: Schedule,
    pub scanner: ScannerSettings,
    pub archive: ArchiveSettings,
    pub index_db_path: PathBuf,
    pub pdf_worker_path: PathBuf,
    pub proxy_http: String,
    pub proxy_https: String,
    api_key: String,
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppConfig")
            .field("config_dir", &self.config_dir)
            .finish_non_exhaustive()
    }
}

fn invalid(field: &str) -> ReportError {
    ReportError::new("CONFIG_INVALID", format!("请检查配置项 {field}"))
}

impl AppConfig {
    pub fn reports_db_path(&self) -> PathBuf {
        self.paths.db_dir.join(crate::types::REPORTS_DB_FILENAME)
    }

    pub fn search_db_path(&self) -> PathBuf {
        self.paths.db_dir.join(crate::types::SEARCH_DB_FILENAME)
    }

    pub fn daily_cache_dir(&self) -> PathBuf {
        self.paths.data_dir.join(crate::types::DAILY_CACHE_DIR)
    }

    pub fn run_snapshots_dir(&self) -> PathBuf {
        self.paths.data_dir.join(crate::types::RUN_SNAPSHOTS_DIR)
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub(crate) fn scanner_config(&self) -> crate::scan::ScannerConfig {
        crate::scan::ScannerConfig {
            work_dir: self.paths.work_dir.clone(),
            scan_db_path: self.index_db_path.clone(),
            scanner_settings: self.scanner.clone(),
            pdf_worker_path: self.pdf_worker_path.clone(),
        }
    }

    pub fn personal_scan(&self) -> bool {
        self.report.profile == ReportProfile::Personal
    }
}
