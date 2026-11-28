use super::{invalid, Paths, ReportProfile, ReportSettings};
use crate::{
    types::{MAX_FILE_CHARS, SCAN_CACHE_FILENAME},
    ReportError,
};
use daily_parsers::settings::ScannerSettings;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

fn section<T: DeserializeOwned>(settings: &mut Value, name: &str) -> Result<T, ReportError> {
    serde_json::from_value(settings[name].take()).map_err(|_| invalid(name))
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

const SETTINGS_SECTIONS: &[&str] = &[
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
];

fn merged_settings(
    config_dir: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Value, ReportError> {
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
        if !SETTINGS_SECTIONS.contains(&path[0]) {
            continue;
        }
        let value =
            serde_saphyr::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.clone()));
        set_leaf(&mut raw, &path, value)?;
    }
    for key in raw.as_object().unwrap().keys() {
        if key != "app" && !SETTINGS_SECTIONS.contains(&key.as_str()) {
            return Err(invalid(key));
        }
    }
    let mut settings: Value = serde_json::from_str(include_str!("../config.defaults.json"))?;
    merge(&mut settings, raw);
    Ok(settings)
}

fn take_scanner_path(
    settings: &mut Map<String, Value>,
    key: &str,
    filenames: &[&str],
) -> Result<Option<PathBuf>, ReportError> {
    let Some(value) = settings.remove(key) else {
        return Ok(None);
    };
    let path = value.as_str().map(Path::new).filter(|path| {
        path.file_name().is_some_and(|name| {
            filenames
                .iter()
                .any(|filename| name.eq_ignore_ascii_case(filename))
        })
    });
    path.map(Path::to_owned)
        .map(Some)
        .ok_or_else(|| invalid(&format!("scanner.{key}")))
}

impl super::AppConfig {
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
        let mut settings = merged_settings(&config_dir, env)?;
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
        let legacy_key = if provider == "deepseek" {
            legacy_key.as_ref().and_then(Value::as_str)
        } else {
            None
        };
        let api_key = env
            .get(credential_name)
            .map(String::as_str)
            .filter(|key| !key.trim().is_empty())
            .or_else(|| settings["api"][credential_name.to_lowercase()].as_str())
            .or(legacy_key)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut paths: Paths = section(&mut settings, "paths")?;
        let mut report: ReportSettings = section(&mut settings, "report")?;
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
            *path = program_dir.join(&*path);
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
        let mut scanner = settings["scanner"].take();
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
        let cache_filename = SCAN_CACHE_FILENAME;
        let index = take_scanner_path(
            scanner_map,
            "index_db_path",
            &["scan_index_rust_v3.sqlite3", cache_filename],
        )?
        .map_or_else(
            || paths.db_dir.join(cache_filename),
            |path| path.with_file_name(cache_filename),
        );
        take_scanner_path(
            scanner_map,
            "office_worker_path",
            &["ai-daily-office-parser.exe"],
        )?;
        take_scanner_path(
            scanner_map,
            "pdf_worker_path",
            &["ai-daily-pdf-parser.exe", "ai-daily-pdf-worker.exe"],
        )?;
        let scanner: ScannerSettings =
            serde_json::from_value(scanner).map_err(|_| invalid("scanner"))?;
        // Old saved settings may contain quota options. They no longer affect execution.
        if let Some(recovery) = settings["recovery"].as_object_mut() {
            for key in [
                "enforce_model_budgets",
                "max_calls_per_run",
                "max_calls_per_calendar_day",
                "max_tokens_per_run",
                "max_tokens_per_calendar_day",
                "max_cost_per_run_microunits",
                "max_cost_per_calendar_day_microunits",
                "model_pricing",
            ] {
                recovery.remove(key);
            }
        }
        let mut config = Self {
            program_dir: program_dir.clone(),
            config_dir,
            paths,
            report,
            scanner,
            archive: settings
                .as_object_mut()
                .unwrap()
                .remove("archive")
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            llm: section(&mut settings, "llm")?,
            recovery: section(&mut settings, "recovery")?,
            retention: section(&mut settings, "retention")?,
            schedule: section(&mut settings, "schedule")?,
            index_db_path: program_dir.join(index),
            pdf_worker_path: program_dir.join("ai-daily-pdf-worker.exe"),
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
            *path = program_dir.join(&*path);
        }
        if config.report.profile == ReportProfile::Personal {
            config.archive.max_input_mib = config.archive.max_input_mib.min(50);
            config.archive.max_output_mib = config.archive.max_output_mib.min(16);
            config.recovery.run_budget_seconds = 300;
            config.recovery.lock_wait_seconds = config.recovery.lock_wait_seconds.min(5.0);
            config.recovery.model_workers = 1;
            // Leave room for user notes and model execution within the daily budget.
            config.scanner.max_workers.get_or_insert_with(|| {
                std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u64
            });
            for (setting, limit) in [
                (&mut config.scanner.total_max_chars, MAX_FILE_CHARS as u64),
                (&mut config.scanner.total_deadline_ms, 120_000),
            ] {
                let value = setting.get_or_insert(limit);
                *value = (*value).min(limit);
            }
        }
        config.validate()?;
        // Explicitly disabling the optional parser preserves frozen configuration identities.
        config.scanner.pdf_image_ocr = config.scanner.pdf_image_ocr.filter(|ocr| ocr.enabled);
        config.archive.validate()?;
        Ok(config)
    }
}
