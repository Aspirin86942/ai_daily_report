use crate::{scan::sources::ScanSource, snapshots::digest, ReportError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportSource {
    pub source_id: String,
    pub source_kind: String,
    pub relative_path: Option<String>,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_at_utc: Option<String>,
    pub observed_at_utc: String,
    pub content_sha256: Option<String>,
    pub excerpt_sha256: String,
    pub excerpt_chars: usize,
    #[serde(default)]
    pub model_chars: usize,
    pub parser_backend: Option<String>,
    pub worker_lane: Option<String>,
    pub parse_status: String,
    #[serde(default)]
    pub parse_truncated: bool,
    #[serde(default)]
    pub excerpt_truncated: bool,
    #[serde(default)]
    pub model_truncated: bool,
    #[serde(default)]
    pub coverage_limits: Value,
    pub reason: Option<String>,
    pub snapshot_ref: Option<String>,
}

impl ReportSource {
    pub fn from_scan(source: &ScanSource) -> Result<Self, ReportError> {
        let source = Self {
            source_id: source.source_id.clone(),
            source_kind: "file".into(),
            relative_path: Some(source.relative_path.clone()),
            extension: Some(source.extension.clone()),
            size_bytes: source.size_bytes,
            modified_at_utc: Some(source.modified_at_utc.clone()),
            observed_at_utc: source.observed_at_utc.clone(),
            content_sha256: source.content_sha256.clone(),
            excerpt_sha256: source.excerpt_sha256.clone(),
            excerpt_chars: source.excerpt.chars().count(),
            model_chars: 0,
            parser_backend: Some(source.parser_backend.clone()),
            worker_lane: Some(source.worker_lane.as_str().into()),
            parse_status: source.parse_status.as_str().into(),
            parse_truncated: source.parse_truncated,
            excerpt_truncated: source.excerpt_truncated,
            model_truncated: false,
            coverage_limits: source.coverage_limits.clone(),
            reason: source.reason.clone(),
            snapshot_ref: None,
        };
        source.validate()?;
        Ok(source)
    }
    pub fn user(text: &str, observed: DateTime<Utc>) -> Self {
        let hash = digest(text.as_bytes());
        Self {
            source_id: "user_input".into(),
            source_kind: "user_statement".into(),
            relative_path: None,
            extension: None,
            size_bytes: text.len() as u64,
            modified_at_utc: None,
            observed_at_utc: observed.to_rfc3339(),
            content_sha256: Some(hash.clone()),
            excerpt_sha256: hash,
            excerpt_chars: text.chars().count(),
            model_chars: 0,
            parser_backend: None,
            worker_lane: None,
            parse_status: "success".into(),
            parse_truncated: false,
            excerpt_truncated: false,
            model_truncated: false,
            coverage_limits: json!({"evidence_audit":{"version":1,"primary_status":"usable_excerpt","secondary_tags":[]}}),
            reason: None,
            snapshot_ref: None,
        }
    }
    pub fn usable(&self) -> bool {
        self.parse_status == "success"
            && self.excerpt_chars > 0
            && self.content_sha256.is_some()
            && self.coverage_limits["evidence_audit"]["primary_status"]
                .as_str()
                .is_none_or(|s| s == "usable_excerpt")
    }
    pub fn unverified(&self) -> bool {
        self.coverage_limits["evidence_audit"]["text_unverified"] == true
            || self.coverage_limits["ocr"]["text_unverified"] == true
    }
    pub fn validate(&self) -> Result<(), ReportError> {
        let valid_id = self.source_id == "user_input"
            || self.source_id.strip_prefix("src_").is_some_and(sha256);
        if !valid_id
            || !matches!(
                self.parse_status.as_str(),
                "success" | "error" | "timeout" | "not_parsed"
            )
            || !sha256(&self.excerpt_sha256)
            || self.content_sha256.as_ref().is_some_and(|s| !sha256(s))
            || self.model_chars > self.excerpt_chars
            || !utc(&self.observed_at_utc)
            || self.modified_at_utc.as_ref().is_some_and(|s| !utc(s))
            || match self.source_kind.as_str() {
                "file" => {
                    self.source_id == "user_input"
                        || self.relative_path.as_ref().is_none_or(|p| !relative(p))
                }
                "user_statement" => self.source_id != "user_input" || self.relative_path.is_some(),
                _ => true,
            }
        {
            return Err(ReportError::new(
                "REPORT_SOURCE_INVALID",
                "报告来源身份、范围或时间无效",
            ));
        }
        Ok(())
    }
}

fn sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn utc(text: &str) -> bool {
    DateTime::parse_from_rfc3339(text).is_ok_and(|t| t.offset().local_minus_utc() == 0)
}
fn relative(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(['/', '\\'])
        && !text.contains(':')
        && text.split(['/', '\\']).all(|p| !matches!(p, ".." | "."))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageGap {
    pub code: String,
    pub message: String,
    pub source_id: Option<String>,
    pub relative_path: Option<String>,
    pub stage: String,
}
impl CoverageGap {
    pub fn new(code: &str, stage: &str, source: Option<&str>, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            source_id: source.map(str::to_owned),
            relative_path: None,
            stage: stage.into(),
        }
    }
}
