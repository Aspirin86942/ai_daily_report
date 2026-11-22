use super::*;

/// Validate the calendar range and the optional, narrower observation window.
pub fn validate_scan_request(
    request_id: &str,
    start_date: &str,
    end_date: &str,
    window_start: Option<&str>,
    window_end: Option<&str>,
    mode: ReportMode,
    compression: Option<CompressionProfile>,
) -> Result<(), String> {
    require_request_id(request_id)?;
    require_date(start_date, "start_date")?;
    require_date(end_date, "end_date")?;
    if start_date > end_date {
        return Err("start_date must not be after end_date".into());
    }
    match (window_start, window_end) {
        (None, None) => {}
        (Some(start), Some(end)) => {
            if utc_instant(start)? >= utc_instant(end)? {
                return Err("scan window must have positive duration".into());
            }
        }
        _ => return Err("UTC window boundaries must be supplied together".into()),
    }
    if compression.is_some_and(|policy| policy != mode.compression_profile()) {
        return Err("compression profile does not match report mode".into());
    }
    Ok(())
}

fn utc_instant(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
    // Saved requests use seconds, up to six fractional digits and an explicit UTC offset.
    let body = value
        .strip_suffix('Z')
        .or_else(|| value.strip_suffix("+00:00"));
    let valid_shape = body.is_some_and(|body| {
        let (whole, fraction) = body
            .split_once('.')
            .map_or((body, None), |(w, f)| (w, Some(f)));
        whole.len() == 19
            && whole.as_bytes()[10] == b'T'
            && !whole.starts_with("0000")
            && whole.get(17..19).is_some_and(|s| s < "60")
            && fraction
                .is_none_or(|f| (1..=6).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit()))
    });
    if !valid_shape {
        return Err("scan window must use RFC 3339 UTC timestamps".into());
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|_| "scan window timestamp is invalid".into())
}

fn validate_diagnostics(warnings: &[Diagnostic], error: Option<&Diagnostic>) -> Result<(), String> {
    warnings
        .iter()
        .chain(error)
        .try_for_each(Validate::validate)
}

fn status_matches_diagnostics(
    status: EngineStatus,
    warnings: &[Diagnostic],
    error: &Nullable<Diagnostic>,
) -> bool {
    error.0.is_some() == (status == EngineStatus::Error)
        && (status != EngineStatus::Partial || !warnings.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEnvelope {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub engine_version: String,
    pub engine_build: String,
    pub status: EngineStatus,
    pub file_context: String,
    pub summary: ContextSummary,
    pub scan_run_id: Nullable<u64>,
    pub context_run_id: Nullable<u64>,
    pub warnings: Vec<Diagnostic>,
    pub error: Nullable<Diagnostic>,
}

impl Validate for ContextEnvelope {
    fn validate(&self) -> Result<(), String> {
        validate_context_header(&self.contract, self.protocol_version, &self.request_id)?;
        require_non_empty(&self.engine_version, 4096, "engine_version")?;
        require_non_empty(&self.engine_build, 4096, "engine_build")?;
        self.summary.validate()?;
        if self.warnings.len() > 100_000 {
            return Err("too many context warnings".to_string());
        }
        validate_diagnostics(&self.warnings, self.error.0.as_ref())?;
        for (field, value) in [
            ("scan_run_id", self.scan_run_id.0),
            ("context_run_id", self.context_run_id.0),
        ] {
            if let Some(value) = value {
                require_range(value, 1, u64::MAX, field)?;
            }
        }
        let valid_context = if self.status == EngineStatus::Error {
            self.file_context.is_empty()
        } else {
            !self.file_context.is_empty()
                && self.scan_run_id.0.is_some()
                && self.context_run_id.0.is_some()
        };
        if valid_context && status_matches_diagnostics(self.status, &self.warnings, &self.error) {
            Ok(())
        } else {
            Err("context violates status invariants".into())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorCheckStatus,
    pub message: String,
}

impl Validate for DoctorCheck {
    fn validate(&self) -> Result<(), String> {
        require_non_empty(&self.name, 4096, "doctor check name")?;
        require_non_empty(&self.message, 4096, "doctor check message")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorResponse {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub status: EngineStatus,
    pub engine_version: String,
    pub engine_build: String,
    pub checks: Vec<DoctorCheck>,
    pub warnings: Vec<Diagnostic>,
    pub error: Nullable<Diagnostic>,
}

impl Validate for DoctorResponse {
    fn validate(&self) -> Result<(), String> {
        validate_context_header(&self.contract, self.protocol_version, &self.request_id)?;
        require_non_empty(&self.engine_version, 4096, "engine_version")?;
        require_non_empty(&self.engine_build, 4096, "engine_build")?;
        if self.checks.len() > 256 || self.warnings.len() > 256 {
            return Err("doctor response contains too many items".to_string());
        }
        for check in &self.checks {
            check.validate()?;
        }
        validate_diagnostics(&self.warnings, self.error.0.as_ref())?;
        if status_matches_diagnostics(self.status, &self.warnings, &self.error) {
            Ok(())
        } else {
            Err("doctor response violates status invariants".into())
        }
    }
}
