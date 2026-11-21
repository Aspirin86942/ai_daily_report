use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildContextRequest {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub work_dir: String,
    pub start_date: String,
    pub end_date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_end_utc: Option<String>,
    pub report_mode: ReportMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub compression_profile: Nullable<CompressionProfile>,
    pub scan_db_path: String,
    pub scanner_settings: ScannerSettings,
    pub adapters: AdapterPaths,
}

impl Validate for BuildContextRequest {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_absolute_path(&self.work_dir, "work_dir")?;
        require_date(&self.start_date, "start_date")?;
        require_date(&self.end_date, "end_date")?;
        match (&self.window_start_utc, &self.window_end_utc) {
            (None, None) => {}
            (Some(start), Some(end)) => {
                let parse = |value: &str| -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
                    // Match the strict Python contract (seconds and explicit UTC).
                    let body = value
                        .strip_suffix('Z')
                        .or_else(|| value.strip_suffix("+00:00"));
                    let valid_shape =
                        body.is_some_and(|body| {
                            let bytes = body.as_bytes();
                            bytes.len() >= 19
                                && bytes.iter().take(19).enumerate().all(
                                    |(index, byte)| match index {
                                        4 | 7 => *byte == b'-',
                                        10 => *byte == b'T',
                                        13 | 16 => *byte == b':',
                                        _ => byte.is_ascii_digit(),
                                    },
                                )
                                && &body[..4] != "0000"
                                && &body[17..19] < "60"
                                && (bytes.len() == 19
                                    || ((21..=26).contains(&bytes.len())
                                        && bytes[19] == b'.'
                                        && bytes[20..].iter().all(u8::is_ascii_digit)))
                        });
                    if !valid_shape {
                        return Err("scan window must use RFC 3339 UTC timestamps".to_string());
                    }
                    chrono::DateTime::parse_from_rfc3339(value)
                        .map_err(|_| "scan window timestamp is invalid".to_string())
                };
                if parse(start)? >= parse(end)? {
                    return Err("scan window must have positive duration".to_string());
                }
            }
            _ => return Err("UTC window boundaries must be supplied together".to_string()),
        }
        if self.start_date > self.end_date {
            return Err("start_date must not be after end_date".to_string());
        }
        if self
            .compression_profile
            .0
            .is_some_and(|p| p != self.report_mode.compression_profile())
        {
            return Err("compression profile does not match report mode".to_string());
        }
        require_absolute_path(&self.scan_db_path, "scan_db_path")?;
        self.scanner_settings.validate()?;
        self.adapters.validate()
    }
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub scan_run_id: Nullable<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub context_run_id: Nullable<u64>,
    pub warnings: Vec<Diagnostic>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub error: Nullable<Diagnostic>,
}

impl Validate for ContextEnvelope {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_non_empty(&self.engine_version, 4096, "engine_version")?;
        require_non_empty(&self.engine_build, 4096, "engine_build")?;
        self.summary.validate()?;
        if self.warnings.len() > 100_000 {
            return Err("too many context warnings".to_string());
        }
        for warning in &self.warnings {
            warning.validate()?;
        }
        if let Some(error) = &self.error.0 {
            error.validate()?;
        }
        for (field, value) in [
            ("scan_run_id", self.scan_run_id.0),
            ("context_run_id", self.context_run_id.0),
        ] {
            if let Some(value) = value {
                require_range(value, 1, u64::MAX, field)?;
            }
        }
        match self.status {
            EngineStatus::Ok => {
                if self.file_context.is_empty()
                    || self.scan_run_id.0.is_none()
                    || self.context_run_id.0.is_none()
                    || self.error.0.is_some()
                {
                    Err("ok context violates status invariants".to_string())
                } else {
                    Ok(())
                }
            }
            EngineStatus::Partial => {
                if self.file_context.is_empty()
                    || self.scan_run_id.0.is_none()
                    || self.context_run_id.0.is_none()
                    || self.warnings.is_empty()
                    || self.error.0.is_some()
                {
                    Err("partial context violates status invariants".to_string())
                } else {
                    Ok(())
                }
            }
            EngineStatus::Error => {
                if !self.file_context.is_empty() || self.error.0.is_none() {
                    Err("error context violates status invariants".to_string())
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorRequest {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub scan_db_path: String,
    pub adapters: AdapterPaths,
}

impl Validate for DoctorRequest {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_absolute_path(&self.scan_db_path, "scan_db_path")?;
        self.adapters.validate()
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub error: Nullable<Diagnostic>,
}

impl Validate for DoctorResponse {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_non_empty(&self.engine_version, 4096, "engine_version")?;
        require_non_empty(&self.engine_build, 4096, "engine_build")?;
        if self.checks.len() > 256 || self.warnings.len() > 256 {
            return Err("doctor response contains too many items".to_string());
        }
        for check in &self.checks {
            check.validate()?;
        }
        for warning in &self.warnings {
            warning.validate()?;
        }
        if let Some(error) = &self.error.0 {
            error.validate()?;
        }
        match self.status {
            EngineStatus::Ok if self.error.0.is_none() => Ok(()),
            EngineStatus::Partial if self.error.0.is_none() && !self.warnings.is_empty() => Ok(()),
            EngineStatus::Error if self.error.0.is_some() => Ok(()),
            _ => Err("doctor response violates status invariants".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelatedPayload {
    pub role: String,
    pub schema: String,
    pub payload: Value,
}

const BUILD_REQUEST_SCHEMA: &str = "build-context-request-v1.schema.json";
const ENVELOPE_SCHEMA: &str = "context-envelope-v1.schema.json";

fn round_trip<T: DeserializeOwned + Validate + Serialize>(value: &Value) -> Result<Value, String> {
    let typed = T::deserialize(value).map_err(|error| error.to_string())?;
    typed.validate()?;
    serde_json::to_value(typed).map_err(|error| error.to_string())
}

fn parse_contract_payload(schema: &str, value: &Value) -> Result<Value, String> {
    match schema {
        BUILD_REQUEST_SCHEMA => round_trip::<BuildContextRequest>(value),
        ENVELOPE_SCHEMA => round_trip::<ContextEnvelope>(value),
        "diagnostic-v1.schema.json" => round_trip::<Diagnostic>(value),
        "doctor-request-v1.schema.json" => round_trip::<DoctorRequest>(value),
        "doctor-response-v1.schema.json" => round_trip::<DoctorResponse>(value),
        "normalized-scanner-settings.schema.json" => round_trip::<NormalizedScannerSettings>(value),
        "scanner-settings.schema.json" => round_trip::<ScannerSettings>(value),
        _ => Err(format!("unknown scanner contract schema: {schema}")),
    }
}

/// Validate each typed payload and the last request paired with this envelope.
pub fn validate_contract_payload(
    schema: &str,
    value: &Value,
    related_payloads: &[RelatedPayload],
) -> Result<Value, String> {
    let parsed = parse_contract_payload(schema, value)?;
    let mut request_id = None;
    for related in related_payloads {
        parse_contract_payload(&related.schema, &related.payload)?;
        if related.role != "request" {
            return Err(format!("unknown related payload role: {}", related.role));
        }
        request_id =
            (related.schema == BUILD_REQUEST_SCHEMA).then(|| &related.payload["request_id"]);
    }
    if schema == ENVELOPE_SCHEMA && request_id.is_some_and(|id| id != &parsed["request_id"]) {
        return Err("context response request_id mismatch".into());
    }
    Ok(parsed)
}
