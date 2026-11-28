use super::invalid;
use crate::{
    infra::json::fingerprint,
    types::{shanghai, DEFAULT_SCOPE_ID, DEFAULT_TIMEZONE},
    ReportError,
};
use chrono::Utc;
use daily_parsers::settings::ReportMode;
use serde_json::{json, Value};

impl super::AppConfig {
    pub fn fingerprint(&self, recovery: bool) -> Result<String, ReportError> {
        fingerprint(&self.fingerprint_fields(recovery)?)
    }

    fn fingerprint_fields(&self, recovery: bool) -> Result<Value, ReportError> {
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
        Ok(json!({
            "version": 3,
            "report": self.report,
            "llm": llm,
            "scanner": self.scanner,
            "work_dir": self.paths.work_dir,
        }))
    }

    pub fn matches_personal_fingerprint(
        &self,
        expected: &str,
        recovery: bool,
    ) -> Result<bool, ReportError> {
        let mut fields = self.fingerprint_fields(recovery)?;
        if fingerprint(&fields)? == expected {
            return Ok(true);
        }
        if !self.personal_scan() {
            return Ok(false);
        }
        // Only the older character and worker ceilings can vary during replay.
        let old_chars = self.scanner.total_max_chars.filter(|chars| *chars > 18_000);
        let worker_caps = [2, 1].into_iter().filter(|cap| {
            self.scanner
                .max_workers
                .is_some_and(|workers| workers > *cap)
        });
        for worker_cap in std::iter::once(None).chain(worker_caps.map(Some)) {
            if let Some(cap) = worker_cap {
                fields["scanner"]["max_workers"] = json!(cap);
                if let Some(chars) = old_chars {
                    fields["scanner"]["total_max_chars"] = json!(chars);
                }
                if fingerprint(&fields)? == expected {
                    return Ok(true);
                }
            }
            if old_chars.is_some() {
                fields["scanner"]["total_max_chars"] = json!(18_000);
                if fingerprint(&fields)? == expected {
                    return Ok(true);
                }
            }
        }
        Ok(false)
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
        {
            return Err(invalid("llm.max_retries/temperature/max_tokens"));
        }
        if !llm.request_timeout_seconds.is_finite()
            || llm.request_timeout_seconds <= 0.0
            || llm.request_timeout_seconds > 900.0
        {
            return Err(invalid("llm.request_timeout_seconds"));
        }
        if llm.max_request_utf8_bytes < 2048 || llm.response_format != "json_object" {
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
        if let Some(effort) = llm.reasoning_effort.as_deref() {
            if llm.provider != "openai"
                || !["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort)
                || llm.thinking_mode.is_some()
                || llm.thinking_parameter_location.is_some()
            {
                return Err(invalid("llm.reasoning_effort"));
            }
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
        if report.scope_id != DEFAULT_SCOPE_ID
            || report.timezone != DEFAULT_TIMEZONE
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
            || recovery.run_budget_seconds == 0
            || !recovery.lock_wait_seconds.is_finite()
            || !(0.0..=60.0).contains(&recovery.lock_wait_seconds)
        {
            return Err(invalid("recovery"));
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
        crate::scan::config::normalize_scanner_settings(&self.scanner, ReportMode::Daily)
            .map_err(|_| invalid("scanner"))?;
        Ok(())
    }

    pub fn config_check(&self) -> Value {
        json!({"status":"ok", "config_dir":self.config_dir, "timezone":self.report.timezone,
            "local_date":Utc::now().with_timezone(&shanghai()).date_naive(),
            "max_attempts":self.llm.max_retries, "model_limits_verified":self.llm.context_window_tokens.is_some() && self.llm.verified_max_output_tokens.is_some()})
    }
}
