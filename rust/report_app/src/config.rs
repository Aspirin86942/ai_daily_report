use crate::{types::shanghai, ReportError};
use ai_daily_scanner_contract::{ReportMode, ScannerSettings};
use chrono::{NaiveTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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

impl Llm {
    pub fn facts_output_tokens(&self) -> u64 {
        self.facts_max_tokens.min(
            self.verified_max_output_tokens
                .unwrap_or(self.facts_max_tokens),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportSettings {
    pub scope_id: String,
    pub root_alias: String,
    pub timezone: String,
    pub default_target: String,
    pub notes_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub rate_version: String,
    pub currency: String,
    pub input_per_million_microunits: u64,
    pub output_per_million_microunits: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    #[serde(default = "default_model_workers")]
    pub model_workers: u8,
    pub enforce_model_budgets: bool,
    pub missed_days_policy: String,
    pub max_calls_per_run: u64,
    pub max_calls_per_calendar_day: u64,
    pub run_budget_seconds: u64,
    pub lock_wait_seconds: f64,
    pub max_tokens_per_run: Option<u64>,
    pub max_tokens_per_calendar_day: Option<u64>,
    pub max_cost_per_run_microunits: Option<u64>,
    pub max_cost_per_calendar_day_microunits: Option<u64>,
    pub model_pricing: Option<ModelPricing>,
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
    pub archive: crate::archive::config::ArchiveSettings,
    pub index_db_path: PathBuf,
    pub office_worker_path: PathBuf,
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

fn section<T: DeserializeOwned>(settings: &Value, name: &str) -> Result<T, ReportError> {
    serde_json::from_value(settings[name].clone()).map_err(|_| invalid(name))
}

fn normalize_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key.to_lowercase(), normalize_keys(value)))
                .collect(),
        ),
        other => other,
    }
}

fn merge(base: &mut Value, incoming: Value) {
    match (base, incoming) {
        (Value::Object(base), Value::Object(mut incoming)) => {
            if incoming.remove("dynaconf_merge") == Some(Value::Bool(false)) {
                base.clear();
            }
            for (key, value) in incoming {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (Value::Array(base), Value::Array(incoming)) => base.extend(incoming),
        (base, incoming) => *base = incoming,
    }
}

fn set_leaf(root: &mut Value, path: &[&str], value: Value) -> Result<(), ReportError> {
    if path.is_empty() {
        *root = value;
        return Ok(());
    }
    if root.is_null() {
        *root = json!({});
    }
    let map = root
        .as_object_mut()
        .ok_or_else(|| invalid("环境变量覆盖路径"))?;
    set_leaf(map.entry(path[0]).or_insert(Value::Null), &path[1..], value)
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

// Fixed location and scalar syntax only; process environment always wins.
fn read_dotenv(path: &Path) -> Result<BTreeMap<String, String>, ReportError> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(invalid(".env")),
    };
    let mut values = BTreeMap::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=').ok_or_else(|| invalid(".env"))?;
        let key = key.trim();
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(invalid(".env"));
        }
        let value = value.trim();
        let value = if value.starts_with('"') {
            serde_json::from_str::<String>(value).map_err(|_| invalid(".env"))?
        } else if value.starts_with('\'') {
            value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .ok_or_else(|| invalid(".env"))?
                .to_owned()
        } else {
            value.split(" #").next().unwrap().trim_end().to_owned()
        };
        if value.contains("${") {
            return Err(invalid(".env 变量展开不受支持"));
        }
        values.insert(key.to_owned(), value);
    }
    Ok(values)
}

impl AppConfig {
    pub fn load(program_dir: &Path, requested_dir: Option<&Path>) -> Result<Self, ReportError> {
        Self::load_with_env(program_dir, requested_dir, &std::env::vars().collect())
    }

    /// The environment is an input, which keeps tests independent of process-wide variables.
    pub fn load_with_env(
        program_dir: &Path,
        requested_dir: Option<&Path>,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, ReportError> {
        let program_dir = std::fs::canonicalize(program_dir)?;
        let config_dir = requested_dir
            .map(Path::to_owned)
            .or_else(|| {
                env.get("DAILY_REPORT_CONFIG_DIR")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| program_dir.join("config"));
        let config_dir = std::fs::canonicalize(config_dir).map_err(|_| invalid("--config-dir"))?;
        let mut effective_env = read_dotenv(&config_dir.parent().unwrap().join(".env"))?;
        effective_env.extend(env.clone());
        let env = &effective_env;
        let mut raw = json!({});
        let mut found = false;
        for name in ["settings.yaml", "settings.windows.yaml", ".secrets.yaml"] {
            let path = config_dir.join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    found |= name != ".secrets.yaml";
                    // YAML diagnostics can include source lines; never echo a secret file's contents.
                    let value: Value = serde_saphyr::from_str(text.trim_start_matches('\u{feff}'))
                        .map_err(|_| {
                            ReportError::new(
                                "CONFIG_YAML_INVALID",
                                format!("无法读取 {name} 的 YAML 结构"),
                            )
                        })?;
                    if !value.is_object() {
                        return Err(invalid(name));
                    }
                    merge(&mut raw, normalize_keys(value));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !found {
            return Err(ReportError::new(
                "CONFIG_MISSING",
                "配置目录缺少 settings.yaml 或 settings.windows.yaml",
            ));
        }
        for (name, text) in env {
            let Some(name) = name.strip_prefix("DAILY_REPORT_") else {
                continue;
            };
            let name = name.to_lowercase();
            let path: Vec<_> = name.split("__").collect();
            if ![
                "paths",
                "llm",
                "api",
                "report",
                "recovery",
                "retention",
                "schedule",
                "scanner",
                "archive",
                "proxy",
            ]
            .contains(&path[0])
            {
                continue;
            }
            let value = serde_saphyr::from_str::<Value>(text)
                .unwrap_or_else(|_| Value::String(text.clone()));
            set_leaf(&mut raw, &path, value)?;
        }
        for key in raw.as_object().unwrap().keys() {
            if ![
                "app",
                "paths",
                "llm",
                "api",
                "proxy",
                "report",
                "recovery",
                "retention",
                "schedule",
                "scanner",
                "archive",
            ]
            .contains(&key.as_str())
            {
                return Err(invalid(key));
            }
        }
        let mut settings: Value = serde_json::from_str(include_str!("config.defaults.json"))?;
        merge(&mut settings, raw);
        let legacy_key = settings["llm"]
            .as_object_mut()
            .ok_or_else(|| invalid("llm"))?
            .remove("deepseek_api_key");
        let provider = settings["llm"]["provider"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        settings["llm"]["provider"] = Value::String(provider.clone());
        let credential_name = if provider == "deepseek" {
            "DEEPSEEK_API_KEY"
        } else {
            "OPENAI_API_KEY"
        };
        let api_key = env
            .get(credential_name)
            .filter(|key| !key.trim().is_empty())
            .cloned()
            .or_else(|| {
                settings["api"][credential_name.to_lowercase()]
                    .as_str()
                    .map(str::to_owned)
            })
            .or_else(|| {
                if provider == "deepseek" {
                    legacy_key.and_then(|value| value.as_str().map(str::to_owned))
                } else {
                    None
                }
            })
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut paths: Paths = section(&settings, "paths")?;
        let mut report: ReportSettings = section(&settings, "report")?;
        for path in [
            &mut paths.work_dir,
            &mut paths.data_dir,
            &mut paths.reports_dir,
            &mut paths.db_dir,
            &mut paths.log_dir,
            &mut report.notes_dir,
        ] {
            if path.as_os_str().is_empty() {
                return Err(invalid("paths"));
            }
            *path = absolute(&program_dir, path);
        }
        if let Some(install) = env
            .get("DAILY_REPORT_INSTALL_ROOT")
            .filter(|value| !value.trim().is_empty())
        {
            let install = Path::new(install);
            if !install.is_absolute() {
                return Err(invalid("DAILY_REPORT_INSTALL_ROOT"));
            }
            let shared = std::fs::canonicalize(install.join("shared"))
                .map_err(|_| invalid("DAILY_REPORT_INSTALL_ROOT"))?;
            if !config_dir.starts_with(&shared) {
                return Err(invalid("DAILY_REPORT_CONFIG_DIR"));
            }
            for (name, destination) in [
                ("DATA_DIR", &mut paths.data_dir),
                ("REPORTS_DIR", &mut paths.reports_dir),
                ("DB_DIR", &mut paths.db_dir),
                ("LOG_DIR", &mut paths.log_dir),
            ] {
                let key = format!("DAILY_REPORT_{name}");
                let path = Path::new(env.get(&key).ok_or_else(|| invalid(&key))?);
                if !path.is_absolute() {
                    return Err(invalid(&key));
                }
                let path = std::fs::canonicalize(path).map_err(|_| invalid(&key))?;
                if !path.is_dir() || !path.starts_with(&shared) {
                    return Err(invalid(&key));
                }
                *destination = path;
            }
        }
        let mut scanner = settings["scanner"].clone();
        let scanner_map = scanner.as_object_mut().ok_or_else(|| invalid("scanner"))?;
        for retired in [
            "python_executable",
            "python_worker_path",
            "fallback_after_timeout",
            "fallback_order",
            "fallback_enabled",
        ] {
            if scanner_map.contains_key(retired) {
                return Err(ReportError::new(
                    "CONFIG_RETIRED_OPTION",
                    format!("请移除 scanner.{retired}"),
                ));
            }
        }
        let index = scanner_map
            .remove("index_db_path")
            .and_then(|value| value.as_str().map(PathBuf::from))
            .unwrap_or_else(|| paths.db_dir.join("scan_index_rust_v3.sqlite3"));
        let office = scanner_map
            .remove("office_worker_path")
            .and_then(|value| value.as_str().map(PathBuf::from))
            .unwrap_or_else(|| program_dir.join("ai-daily-office-parser.exe"));
        let pdf = scanner_map
            .remove("pdf_worker_path")
            .and_then(|value| value.as_str().map(PathBuf::from))
            .unwrap_or_else(|| program_dir.join("ai-daily-pdf-parser.exe"));
        let scanner: ScannerSettings =
            serde_json::from_value(scanner).map_err(|_| invalid("scanner"))?;
        let mut config = Self {
            program_dir: program_dir.clone(),
            config_dir,
            paths,
            report,
            scanner,
            archive: serde_json::from_value(
                settings
                    .get("archive")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            )?,
            llm: section(&settings, "llm")?,
            recovery: section(&settings, "recovery")?,
            retention: section(&settings, "retention")?,
            schedule: section(&settings, "schedule")?,
            index_db_path: absolute(&program_dir, &index),
            office_worker_path: absolute(&program_dir, &office),
            pdf_worker_path: absolute(&program_dir, &pdf),
            proxy_http: settings["proxy"]["http_proxy"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            proxy_https: settings["proxy"]["https_proxy"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            api_key,
        };
        for path in config
            .archive
            .roots
            .iter_mut()
            .chain(&mut config.archive.model_roots)
        {
            *path = absolute(&program_dir, path);
        }
        config.validate()?;
        config.archive.validate()?;
        Ok(config)
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn fingerprint(&self, recovery: bool) -> Result<String, ReportError> {
        let mut llm = serde_json::to_value(&self.llm)?;
        if recovery {
            for key in [
                "max_retries",
                "invalid_response_retries",
                "request_timeout_seconds",
                "context_window_tokens",
                "verified_max_output_tokens",
            ] {
                llm.as_object_mut().unwrap().remove(key);
            }
        }
        crate::snapshots::fingerprint(
            &json!({"version":3,"report":self.report,"llm":llm,"scanner":self.scanner,"work_dir":self.paths.work_dir}),
        )
    }

    pub fn endpoint(&self) -> String {
        let base = if !self.llm.base_url.trim().is_empty() {
            self.llm.base_url.trim()
        } else if self.llm.provider == "deepseek" {
            "https://api.deepseek.com"
        } else {
            "https://api.openai.com/v1"
        };
        format!("{}/chat/completions", base.trim_end_matches('/'))
    }

    pub fn validate(&self) -> Result<(), ReportError> {
        if !(1..=4).contains(&self.recovery.model_workers) {
            return Err(invalid("recovery.model_workers"));
        }
        let llm = &self.llm;
        if !["deepseek", "openai"].contains(&llm.provider.as_str())
            || llm.model_id.trim().is_empty()
        {
            return Err(invalid("llm.provider/model_id"));
        }
        if !(1..=3).contains(&llm.max_retries)
            || !(0.0..=2.0).contains(&llm.temperature)
            || llm.max_tokens == 0
            || llm.facts_max_tokens == 0
        {
            return Err(invalid("llm.max_retries/temperature/max_tokens"));
        }
        if !llm.request_timeout_seconds.is_finite()
            || llm.request_timeout_seconds <= 0.0
            || llm.request_timeout_seconds > 900.0
        {
            return Err(invalid("llm.request_timeout_seconds"));
        }
        if llm.max_request_utf8_bytes < 2048
            || llm.max_sources_per_batch == 0
            || llm.max_batches == 0
            || llm.response_format != "json_object"
        {
            return Err(invalid("llm 请求限制"));
        }
        if llm
            .thinking_mode
            .as_ref()
            .is_some_and(|value| !["enabled", "disabled"].contains(&value.as_str()))
            || llm
                .thinking_parameter_location
                .as_ref()
                .is_some_and(|value| {
                    !["top_level", "extra_body"].contains(&value.as_str())
                        || llm.thinking_mode.is_none()
                })
        {
            return Err(invalid("llm.thinking_mode/thinking_parameter_location"));
        }
        if llm.context_window_tokens.is_some_and(|value| value < 2048)
            || llm.verified_max_output_tokens == Some(0)
            || !["reduce_once", "stop"].contains(&llm.truncation_policy.as_str())
            || llm.max_strategy_changes > 1
            || llm.invalid_response_retries > 2
        {
            return Err(invalid("llm 容量与截断设置"));
        }
        let url = reqwest::Url::parse(&self.endpoint()).map_err(|_| invalid("llm.base_url"))?;
        if !["https", "http"].contains(&url.scheme())
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid("llm.base_url"));
        }
        let report = &self.report;
        if report.scope_id != "personal_work"
            || report.timezone != "Asia/Shanghai"
            || report.default_target != "previous_calendar_day"
            || report.root_alias.is_empty()
            || report.root_alias.len() > 64
            || !report
                .root_alias
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(invalid("report"));
        }
        let recovery = &self.recovery;
        if recovery.missed_days_policy != "latest_due_day_with_gap_notice"
            || recovery.max_calls_per_run == 0
            || recovery.max_calls_per_calendar_day < recovery.max_calls_per_run
            || recovery.run_budget_seconds == 0
            || !recovery.lock_wait_seconds.is_finite()
            || !(0.0..=60.0).contains(&recovery.lock_wait_seconds)
        {
            return Err(invalid("recovery"));
        }
        if recovery.max_tokens_per_run == Some(0) || recovery.max_tokens_per_calendar_day == Some(0)
        {
            return Err(invalid("recovery token 预算"));
        }
        for (run, day) in [
            (
                recovery.max_tokens_per_run,
                recovery.max_tokens_per_calendar_day,
            ),
            (
                recovery.max_cost_per_run_microunits,
                recovery.max_cost_per_calendar_day_microunits,
            ),
        ] {
            if matches!((run, day), (Some(a), Some(b)) if b < a) {
                return Err(invalid("recovery 每日预算不得小于单次预算"));
            }
        }
        if recovery.model_pricing.is_none()
            && (recovery.max_cost_per_run_microunits.is_some()
                || recovery.max_cost_per_calendar_day_microunits.is_some())
        {
            return Err(invalid("recovery.model_pricing"));
        }
        if recovery.model_pricing.as_ref().is_some_and(|pricing| {
            pricing.rate_version.trim().is_empty()
                || pricing.rate_version.len() > 128
                || !["CNY", "USD"].contains(&pricing.currency.as_str())
        }) {
            return Err(invalid("recovery.model_pricing"));
        }
        if self.retention.input_snapshot_days == 0
            || self.retention.terminal_run_days < self.retention.input_snapshot_days
            || self.retention.snapshot_max_mib == 0
        {
            return Err(invalid("retention"));
        }
        self.schedule.due_time()?;
        if self.schedule.task_name.trim().is_empty()
            || self.schedule.task_name.chars().count() > 128
            || self.schedule.restart_count > 10
            || self.schedule.restart_interval_minutes == 0
            || self.schedule.execution_limit_minutes == 0
        {
            return Err(invalid("schedule"));
        }
        ai_daily_scanner_core::config::normalize_scanner_settings(&self.scanner, ReportMode::Daily)
            .map_err(|_| invalid("scanner"))?;
        Ok(())
    }

    pub fn config_check(&self) -> Value {
        json!({"status":"ok", "config_dir":self.config_dir, "timezone":self.report.timezone,
            "local_date":Utc::now().with_timezone(&shanghai()).date_naive(), "enforce_model_budgets":self.recovery.enforce_model_budgets,
            "max_attempts":self.llm.max_retries, "model_limits_verified":self.llm.context_window_tokens.is_some() && self.llm.verified_max_output_tokens.is_some()})
    }
}
