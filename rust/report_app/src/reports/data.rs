use crate::{
    model::{
        contracts::{ComposedReport, SelectedFact},
        slicing::{InputUnit, SourceInput},
    },
    snapshots::digest,
    types::{parse_date, shanghai, utc_window},
    ReportError,
};
use ai_daily_scanner_core::sources::ScanSource;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

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
        let mut value = serde_json::to_value(source)?;
        let map = value.as_object_mut().unwrap();
        map.remove("excerpt");
        map.insert(
            "excerpt_chars".into(),
            json!(source.excerpt.chars().count()),
        );
        map.insert("source_kind".into(), json!("file"));
        let source: Self = serde_json::from_value(value)?;
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
    pub fn input(&self, text: &str) -> Result<Option<SourceInput>, ReportError> {
        if !self.usable() {
            return Ok(None);
        }
        if text.chars().count() != self.excerpt_chars
            || digest(text.as_bytes()) != self.excerpt_sha256
        {
            return Err(ReportError::new(
                "SOURCE_EXCERPT_HASH_MISMATCH",
                "摘录正文与来源哈希不一致",
            ));
        }
        let mut units = Vec::new();
        if let Some(declared) = self.coverage_limits["evidence_audit"]["content_units"].as_array() {
            for unit in declared {
                units.push(InputUnit {
                    start: usize::try_from(unit["text_start"].as_u64().ok_or_else(invalid)?)
                        .map_err(|_| invalid())?,
                    end: usize::try_from(unit["text_end"].as_u64().ok_or_else(invalid)?)
                        .map_err(|_| invalid())?,
                    kind: unit["kind"].as_str().ok_or_else(invalid)?.into(),
                    ranges: unit["ranges"].as_array().ok_or_else(invalid)?.clone(),
                });
            }
        }
        let history = &self.coverage_limits["history"];
        Ok(Some(SourceInput {
            source_id: self.source_id.clone(),
            source_kind: self
                .extension
                .clone()
                .unwrap_or_else(|| self.source_kind.clone()),
            path: self
                .relative_path
                .clone()
                .unwrap_or_else(|| "用户补充".into()),
            text: text.into(),
            units,
            history_priority: history["excerpt_priority"] == true
                && history["prior_content_sha256"].as_str() == self.content_sha256.as_deref(),
            text_unverified: self.unverified(),
        }))
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
fn invalid() -> ReportError {
    ReportError::new("REPORT_DATA_INVALID", "报告结构、日期窗口或来源引用无效")
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DailyReport {
    pub schema_version: u32,
    pub report_kind: String,
    pub date: String,
    pub scope_id: String,
    pub root_alias: String,
    pub timezone: String,
    pub window_start_utc: String,
    pub window_end_utc: String,
    pub quality: String,
    pub sources: Vec<ReportSource>,
    pub gaps: Vec<CoverageGap>,
    pub missed_dates: Vec<String>,
    pub generated_at: String,
    pub user_notes: String,
    pub facts: Vec<SelectedFact>,
    pub content: Option<ComposedReport>,
}

impl DailyReport {
    pub fn validate(&self) -> Result<(), ReportError> {
        let day = parse_date(&self.date)?;
        let (start, end) = utc_window(day)?;
        if !matches!(self.schema_version, 3 | 4)
            || self.report_kind != "file_evidence"
            || self.scope_id != "personal_work"
            || self.timezone != "Asia/Shanghai"
            || self.root_alias.is_empty()
            || self.root_alias.chars().count() > 64
            || DateTime::parse_from_rfc3339(&self.window_start_utc)
                .ok()
                .map(|t| t.with_timezone(&Utc))
                != Some(start)
            || DateTime::parse_from_rfc3339(&self.window_end_utc)
                .ok()
                .map(|t| t.with_timezone(&Utc))
                != Some(end)
            || !utc(&self.generated_at)
            || self.user_notes.chars().count() > 65536
            || !matches!(self.quality.as_str(), "complete" | "partial" | "no_changes")
        {
            return Err(invalid());
        }
        let mut available = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut unverified = BTreeSet::new();
        for source in &self.sources {
            source.validate()?;
            if !ids.insert(source.source_id.clone()) {
                return Err(invalid());
            }
            if source.usable() {
                available.insert(source.source_id.clone());
            }
            if source.unverified() {
                unverified.insert(source.source_id.clone());
            }
        }
        for gap in &self.gaps {
            if gap.source_id.as_ref().is_some_and(|id| !ids.contains(id))
                || gap.code.is_empty()
                || gap.code.len() > 128
                || !gap
                    .code
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || gap.message.is_empty()
                || !matches!(
                    gap.stage.as_str(),
                    "discovery" | "parse" | "excerpt" | "model" | "schedule"
                )
            {
                return Err(invalid());
            }
        }
        for day in &self.missed_dates {
            parse_date(day)?;
        }
        if self.quality == "no_changes"
            && (!self.sources.is_empty()
                || !self.facts.is_empty()
                || !self.gaps.is_empty()
                || self.content.is_some())
        {
            return Err(invalid());
        }
        if self.schema_version == 3 && self.quality != "no_changes" && available.is_empty() {
            return Err(invalid());
        }
        if self.quality == "complete"
            && (!self.gaps.is_empty()
                || self.sources.iter().any(|s| {
                    s.parse_status != "success"
                        || s.parse_truncated
                        || s.excerpt_truncated
                        || s.model_truncated
                        || s.unverified()
                }))
        {
            return Err(invalid());
        }
        let mut fact_ids = BTreeSet::new();
        for selected in &self.facts {
            if !fact_ids.insert(&selected.fact_id)
                || selected.fact.source_ids.is_empty()
                || selected.fact.source_ids.iter().any(|id| {
                    !available.contains(id) || (!selected.pending && unverified.contains(id))
                })
            {
                return Err(invalid());
            }
        }
        match (&self.content, self.facts.is_empty()) {
            (Some(content), false) => content.validate(&self.facts)?,
            (None, true) => {}
            _ => return Err(invalid()),
        }
        Ok(())
    }
    pub fn source_labels(&self) -> BTreeMap<String, String> {
        self.sources
            .iter()
            .enumerate()
            .map(|(i, s)| (s.source_id.clone(), format!("S{:03}", i + 1)))
            .collect()
    }
    pub fn is_historical(&self) -> Result<bool, ReportError> {
        let generated = DateTime::parse_from_rfc3339(&self.generated_at)
            .map_err(|_| invalid())?
            .with_timezone(&shanghai())
            .date_naive();
        Ok(generated > parse_date(&self.date)? + chrono::Duration::days(1))
    }
}
