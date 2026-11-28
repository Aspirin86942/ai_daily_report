use crate::{
    config::AppConfig,
    infra::{fs::reject_redirects, sqlite::open_read_only},
    store::queries::run_phase as query_run_phase,
    types, ReportError,
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{fs, io::Write, time::Duration};

fn run_phase(config: &AppConfig, run_id: &str) -> String {
    let path = config.reports_db_path();
    let Ok(connection) = open_read_only(
        &path,
        Duration::from_secs(1),
        "DATABASE_PATH_REDIRECTED",
        "DATABASE_MISSING",
    ) else {
        return "unknown".into();
    };
    query_run_phase(&connection, run_id)
}

pub fn append_scheduled_log(
    config: &AppConfig,
    kind: &str,
    duration_ms: u64,
    run_id: Option<&str>,
    target_date: Option<&str>,
    result: &Result<Value, ReportError>,
) -> Result<(), ReportError> {
    fs::create_dir_all(&config.paths.log_dir)?;
    let now = Utc::now();
    let local_day = now.with_timezone(&types::shanghai()).date_naive();
    let path = config
        .paths
        .log_dir
        .join(format!("scheduled-{local_day}.jsonl"));
    reject_redirects(&path, "LOG_PATH_REDIRECTED")?;
    let mut event = match result {
        Ok(value) => {
            let action = value["action"].as_str().unwrap_or("");
            let skipped = action == "skipped";
            let status = |key| {
                if skipped {
                    "skipped"
                } else {
                    value[key].as_str().unwrap_or("succeeded")
                }
            };
            let material = value["material_status"]
                .as_str()
                .or(value["quality"].as_str())
                .unwrap_or("unknown");
            let publication = if kind == "daily" {
                if matches!(
                    action,
                    "published" | "existing" | "reused" | "generated" | "repaired"
                ) {
                    "published"
                } else {
                    "not_published"
                }
            } else {
                "not_applicable"
            };
            let reasons = value.get("reason_codes").cloned().unwrap_or_else(|| {
                json!(value["checks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|check| check["level"] != "ok")
                    .filter_map(|check| check["code"].as_str())
                    .collect::<Vec<_>>())
            });
            let field_or = |key, fallback| {
                value
                    .get(key)
                    .filter(|value| !value.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!(fallback))
            };
            let date = field_or("target_date", target_date);
            let mut event = json!({
                "status": status("status"),
                "execution_status": status("execution_status"),
                "material_status": material,
                "publication_status": publication,
                "run_id": field_or("run_id", run_id),
                "date": date,
                "target_date": date,
                "action": value["action"],
                "quality": value["quality"],
                "errors": value["errors"],
                "reason_codes": reasons,
                "phase": "completed",
            });
            if skipped {
                event["reason"] = value["reason"].clone();
                event["model_calls"] = json!(0);
            }
            if let Some(calls) = value.get("model_calls") {
                event["model_calls"] = calls.clone();
            }
            event
        }
        Err(e) => json!({
            "status": "failed",
            "execution_status":"failed", "material_status":"unknown",
            "publication_status":if kind == "daily" { "unknown" } else { "not_applicable" },
            "errors":1, "reason_codes":[e.code],
            "run_id": run_id,
            "target_date": target_date,
            "phase": run_id.map(|id| run_phase(config, id)).unwrap_or_else(|| "unknown".into()),
            "error_code": e.code,
            "message": e.message,
            "retryable": e.retryable(),
        }),
    };
    event["at"] = json!(now);
    event["kind"] = json!(kind);
    event["duration_ms"] = json!(duration_ms);
    event["termination"] = json!("application_exit");
    if config.personal_scan() {
        return super::personal_log::append(&config.paths.log_dir, event, now);
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{event}")?;
    file.sync_all()?;
    Ok(())
}
