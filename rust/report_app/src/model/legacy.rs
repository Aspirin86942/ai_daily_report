//! Read the frozen v1/v2 model contract without rewriting its requests or ledger.
use super::{contracts::invalid, recovery::FrozenCall};
use crate::{
    snapshots::{canonical_json, digest, SnapshotRef, SnapshotStore},
    store::{RunRecord, Store},
    ReportError,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const FULL_SCHEMA: &str = "24e7387961e3dcc8a2bb76f252a8f00424b90bbafa9808abff9f43529c1399a9";
const REDUCED_SCHEMA: &str = "e7a81b0d48f2df22b214de8251a17d6705651f58e7651babf20028d8f4507c95";
fn one() -> u32 {
    1
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyBatch {
    pub logical_step: String,
    pub model: String,
    pub messages: Vec<Value>,
    pub temperature: f64,
    pub max_tokens: u64,
    pub source_ids: Vec<String>,
    pub source_chars: BTreeMap<String, usize>,
    #[serde(default = "one")]
    pub response_schema_version: u32,
    #[serde(default)]
    pub revision: u32,
    pub thinking_mode: Option<String>,
    pub thinking_parameter_location: Option<String>,
    pub response_schema_sha256: Option<String>,
    pub parent_request_fingerprint: Option<String>,
    pub revision_reason: Option<String>,
    pub input_range_version: Option<u32>,
    pub input_audit: Option<Vec<Value>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyItem {
    pub text: String,
    pub source_ids: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacySummary {
    pub summary_items: Vec<LegacyItem>,
    pub verification_items: Vec<LegacyItem>,
}

fn mismatch() -> ReportError {
    ReportError::new(
        "MODEL_PLAN_IDENTITY_MISMATCH",
        "旧版冻结请求或修订身份不一致",
    )
}
impl LegacyBatch {
    pub fn audit_entries(&self) -> Result<Vec<super::slicing::Entry>, ReportError> {
        self.entries()?
            .into_iter()
            .map(|value| {
                let text = value["text"].as_str().ok_or_else(mismatch)?.to_owned();
                let ranges = if value["text_ranges"].is_null() {
                    vec![super::slicing::TextRange {
                        start: 0,
                        end: text.chars().count(),
                    }]
                } else {
                    serde_json::from_value(value["text_ranges"].clone())?
                };
                let mut units = Vec::new();
                if let Some(declared) = value["input_units"].as_array() {
                    for unit in declared {
                        let mut start = unit["start"].as_u64().ok_or_else(mismatch)? as usize;
                        let ends = unit["boundaries"]
                            .as_array()
                            .cloned()
                            .unwrap_or_else(|| vec![unit["end"].clone()]);
                        for end in ends {
                            let end = end.as_u64().ok_or_else(mismatch)? as usize;
                            if end <= start || end > text.chars().count() {
                                return Err(mismatch());
                            }
                            units.push(super::slicing::InputUnit {
                                start,
                                end,
                                kind: unit["kind"].as_str().ok_or_else(mismatch)?.into(),
                                ranges: unit["ranges"].as_array().cloned().unwrap_or_default(),
                            });
                            start = end;
                        }
                    }
                } else {
                    units = super::slicing::sentences(&text, 0, "paragraph", &[]);
                }
                Ok(super::slicing::Entry {
                    source_id: value["source_id"].as_str().ok_or_else(mismatch)?.into(),
                    source_kind: value["source_kind"].as_str().unwrap_or("file").into(),
                    path: value["relative_path"]
                        .as_str()
                        .or(value["path"].as_str())
                        .unwrap_or("")
                        .into(),
                    part: value["part"].as_u64().unwrap_or(0) as usize,
                    text,
                    text_unverified: value["coverage_limits"]["text_unverified"] == true,
                    text_ranges: ranges,
                    input_units: units,
                })
            })
            .collect()
    }
    pub fn reduce(&self, parent: &str) -> Result<Self, ReportError> {
        self.validate()?;
        if self.response_schema_version != 2 || self.revision != 0 {
            return Err(ReportError::new(
                "MODEL_STRATEGY_BUDGET_EXHAUSTED",
                "旧请求已达到缩批上限",
            ));
        }
        let original = self.entries()?;
        let audit = self.audit_entries()?;
        let limit = canonical_json(&self.request())?.len() / 2;
        let mut selected = Vec::new();
        let mut seen = BTreeSet::new();
        let build = |selected: &Vec<Value>| -> Result<Self, ReportError> {
            let mut revised = self.clone();
            revised.revision = 1;
            revised.parent_request_fingerprint = Some(parent.into());
            revised.revision_reason = Some("MODEL_OUTPUT_TRUNCATED".into());
            revised.response_schema_sha256 = Some(REDUCED_SCHEMA.into());
            revised.messages[0]["content"] = json!(format!(
                "{}\n本次为有界缩批：最多3条摘要、1条待核实；仅引用本次提供的来源。",
                self.messages[0]["content"].as_str().unwrap()
            ));
            let mut wire: Value =
                serde_json::from_str(self.messages[1]["content"].as_str().unwrap())?;
            let entries: Vec<_> = selected
                .iter()
                .cloned()
                .map(|mut e| {
                    if matches!(self.input_range_version, Some(2 | 3)) {
                        let m = e.as_object_mut().unwrap();
                        m.remove("text_ranges");
                        m.remove("input_units");
                        if self.input_range_version == Some(3) {
                            m.remove("modified_at_utc");
                            let coverage = m.remove("coverage_limits").unwrap_or(Value::Null);
                            if coverage["text_unverified"] == true {
                                m.insert("coverage_limits".into(), json!({"text_unverified":true}));
                            }
                        }
                    }
                    e
                })
                .collect();
            wire["sources"] = json!(entries);
            revised.messages[1]["content"] =
                json!(String::from_utf8(canonical_json(&wire)?).unwrap());
            revised.input_audit = if matches!(self.input_range_version, Some(2 | 3)) {
                Some(selected.clone())
            } else {
                None
            };
            revised.source_chars = selected
                .iter()
                .map(|e| {
                    (
                        e["source_id"].as_str().unwrap().to_owned(),
                        e["text"].as_str().unwrap().chars().count(),
                    )
                })
                .collect();
            revised.source_ids = revised.source_chars.keys().cloned().collect();
            Ok(revised)
        };
        let clip = |original: &Value,
                    entry: &super::slicing::Entry,
                    end: usize|
         -> Result<Value, ReportError> {
            let prefix = entry.clip(end)?;
            let mut value = original.clone();
            value["text"] = json!(prefix.text);
            if self.input_range_version.is_some() {
                value["text_ranges"] = json!(prefix.text_ranges);
            }
            if let Some(declared) = original["input_units"].as_array() {
                let mut units = Vec::new();
                for old in declared {
                    if old["start"].as_u64().ok_or_else(mismatch)? as usize >= end {
                        break;
                    }
                    let mut unit = old.clone();
                    unit["end"] =
                        json!(end.min(old["end"].as_u64().ok_or_else(mismatch)? as usize));
                    if let Some(boundaries) = old["boundaries"].as_array() {
                        let kept: Vec<_> = boundaries
                            .iter()
                            .filter(|n| n.as_u64().is_some_and(|n| n <= end as u64))
                            .cloned()
                            .collect();
                        unit["boundaries"] = json!(kept);
                        if let Some(range_ends) = old["range_ends"].as_array() {
                            let kept: Vec<_> =
                                range_ends.iter().take(kept.len()).cloned().collect();
                            let last = kept.last().ok_or_else(mismatch)?.clone();
                            unit["range_ends"] = json!(kept);
                            unit["ranges"][0]["end"] = last;
                        }
                    }
                    units.push(unit);
                }
                value["input_units"] = json!(units);
            }
            Ok(value)
        };
        let mut choices = Vec::new();
        for (index, entry) in audit.iter().enumerate() {
            if !seen.insert(&entry.source_id) {
                continue;
            }
            let ends = entry.prefix_ends();
            let Some(first) = ends.first() else { continue };
            let value = clip(&original[index], entry, *first)?;
            let mut next = selected.clone();
            next.push(value);
            if canonical_json(&build(&next)?.request())?.len() <= limit {
                choices.push((selected.len(), index, ends));
                selected = next;
            }
        }
        if selected.is_empty() {
            return Err(ReportError::new(
                "MODEL_ADAPTIVE_PLAN_UNAVAILABLE",
                "半容量内无法容纳完整内容单元",
            ));
        }
        for (position, index, ends) in choices {
            for end in ends.into_iter().skip(1).take_while(|n| *n <= 512) {
                let mut next = selected.clone();
                next[position] = clip(&original[index], &audit[index], end)?;
                if canonical_json(&build(&next)?.request())?.len() > limit {
                    break;
                }
                selected = next;
            }
        }
        let reduced = build(&selected)?;
        reduced.validate()?;
        Ok(reduced)
    }
    pub fn request(&self) -> Value {
        let mut value = json!({"model":self.model,"messages":self.messages,"temperature":self.temperature,"max_tokens":self.max_tokens,"response_format":{"type":"json_object"}});
        if let Some(mode) = &self.thinking_mode {
            let thinking = json!({"type":mode});
            if self.thinking_parameter_location.as_deref() == Some("extra_body") {
                value["extra_body"] = json!({"thinking":thinking});
            } else {
                value["thinking"] = thinking;
            }
        }
        value
    }
    pub fn identity(&self, run: &RunRecord) -> Result<String, ReportError> {
        let mut value = json!({"generation_id":run.generation_id,"request":self.request(),"config_fingerprint":run.config_fingerprint});
        if self.response_schema_version == 2 {
            value["response_schema_sha256"] = json!(self.response_schema_sha256);
        }
        Ok(digest(&canonical_json(&value)?))
    }
    pub fn entries(&self) -> Result<Vec<Value>, ReportError> {
        if matches!(self.input_range_version, Some(2 | 3)) {
            return self.input_audit.clone().ok_or_else(mismatch);
        }
        let wire: Value =
            serde_json::from_str(self.messages[1]["content"].as_str().ok_or_else(mismatch)?)?;
        wire["sources"].as_array().cloned().ok_or_else(mismatch)
    }
    pub fn validate(&self) -> Result<(), ReportError> {
        let ids: BTreeSet<_> = self.source_ids.iter().collect();
        if self.logical_step.len() != 9
            || !self.logical_step.starts_with("batch_")
            || !self.logical_step[6..].bytes().all(|c| c.is_ascii_digit())
            || self.model.is_empty()
            || !(0.0..=2.0).contains(&self.temperature)
            || self.max_tokens == 0
            || self.messages.len() != 2
            || ids.is_empty()
            || ids.len() != self.source_ids.len()
            || ids != self.source_chars.keys().collect()
            || self.source_chars.values().any(|v| *v == 0)
            || !matches!(self.response_schema_version, 1 | 2)
            || self.revision > 1
        {
            return Err(mismatch());
        }
        for (index, message) in self.messages.iter().enumerate() {
            if message.as_object().is_none_or(|m| m.len() != 2)
                || message["role"] != if index == 0 { "system" } else { "user" }
                || !message["content"].is_string()
            {
                return Err(mismatch());
            }
        }
        if self
            .thinking_mode
            .as_deref()
            .is_some_and(|s| !matches!(s, "enabled" | "disabled"))
            || self
                .thinking_parameter_location
                .as_deref()
                .is_some_and(|s| !matches!(s, "top_level" | "extra_body"))
            || self.thinking_mode.is_none() && self.thinking_parameter_location.is_some()
        {
            return Err(mismatch());
        }
        if self.response_schema_version == 1 {
            if self.revision != 0
                || self.response_schema_sha256.is_some()
                || self.parent_request_fingerprint.is_some()
                || self.revision_reason.is_some()
                || self.input_audit.is_some()
                || self.thinking_mode.is_some()
                || self.thinking_parameter_location.is_some()
            {
                return Err(mismatch());
            }
            return Ok(());
        }
        if self.response_schema_sha256.as_deref()
            != Some(if self.revision == 0 {
                FULL_SCHEMA
            } else {
                REDUCED_SCHEMA
            })
            || (self.revision == 1)
                != (self.parent_request_fingerprint.is_some()
                    && self.revision_reason.as_deref() == Some("MODEL_OUTPUT_TRUNCATED"))
            || self
                .input_range_version
                .is_some_and(|v| !(1..=3).contains(&v))
        {
            return Err(mismatch());
        }
        let wire: Value = serde_json::from_str(self.messages[1]["content"].as_str().unwrap())?;
        let entries = self.entries()?;
        let mut counts = BTreeMap::new();
        let mut expected = Vec::new();
        for entry in entries {
            let id = entry["source_id"].as_str().ok_or_else(mismatch)?;
            let text = entry["text"].as_str().ok_or_else(mismatch)?;
            *counts.entry(id.to_owned()).or_insert(0usize) += text.chars().count();
            if self.input_range_version.is_some() {
                let ranges: Vec<super::slicing::TextRange> =
                    serde_json::from_value(entry["text_ranges"].clone())?;
                if ranges.is_empty()
                    || ranges.iter().any(|r| r.end <= r.start)
                    || ranges.iter().map(|r| r.end - r.start).sum::<usize>() != text.chars().count()
                {
                    return Err(mismatch());
                }
            }
            let mut row = entry.clone();
            if matches!(self.input_range_version, Some(2 | 3)) {
                let object = row.as_object_mut().ok_or_else(mismatch)?;
                object.remove("text_ranges");
                object.remove("input_units");
                if self.input_range_version == Some(3) {
                    object.remove("modified_at_utc");
                    let limits = object.remove("coverage_limits").unwrap_or(Value::Null);
                    if limits["text_unverified"] == true {
                        object.insert("coverage_limits".into(), json!({"text_unverified":true}));
                    }
                }
            }
            expected.push(row);
        }
        if counts != self.source_chars || json!(expected) != wire["sources"] {
            return Err(mismatch());
        }
        Ok(())
    }
}

pub fn validate_summary(value: Value, batch: &LegacyBatch) -> Result<LegacySummary, ReportError> {
    let summary: LegacySummary = serde_json::from_value(value).map_err(|_| invalid())?;
    for (items, maximum, length) in [
        (
            &summary.summary_items,
            if batch.response_schema_version == 1 {
                256
            } else if batch.revision == 0 {
                6
            } else {
                3
            },
            if batch.response_schema_version == 1 {
                4000
            } else {
                180
            },
        ),
        (
            &summary.verification_items,
            if batch.response_schema_version == 1 {
                256
            } else if batch.revision == 0 {
                3
            } else {
                1
            },
            if batch.response_schema_version == 1 {
                4000
            } else {
                120
            },
        ),
    ] {
        if items.len() > maximum {
            return Err(invalid());
        }
        for item in items {
            let ids: BTreeSet<_> = item.source_ids.iter().collect();
            if item.text.trim().is_empty()
                || item.text.chars().count() > length
                || ids.is_empty()
                || ids.len() > 64
                || ids.len() != item.source_ids.len()
                || ids.iter().any(|id| !batch.source_ids.contains(id))
            {
                return Err(invalid());
            }
        }
    }
    Ok(summary)
}

pub fn cached(
    store: &Store,
    snapshots: &SnapshotStore,
    run: &RunRecord,
    root: &LegacyBatch,
    pin: Option<&FrozenCall>,
    job_id: Option<&str>,
) -> Result<(LegacySummary, LegacyBatch, Value), ReportError> {
    root.validate()?;
    let mut batch = root.clone();
    let pinned = pin.map(|pin| pin.read(store, run, job_id)).transpose()?;
    let owner = pinned.as_ref().map(|(parent, _)| parent).unwrap_or(run);
    let rows = store.generation_calls(&owner.generation_id, &root.logical_step)?;
    for row in rows.iter().rev() {
        let usage: Value = serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
        let audit = &usage["reserved_budget"]["request_audit"];
        if audit["revision"] != 1 {
            continue;
        }
        let reference: SnapshotRef = serde_json::from_value(audit["plan_revision"].clone())?;
        let value = snapshots.read(&reference)?;
        let revised: LegacyBatch = serde_json::from_value(value["batch"].clone())?;
        revised.validate()?;
        if value["version"] != 1
            || revised.revision != 1
            || revised.parent_request_fingerprint.as_deref() != Some(&root.identity(owner)?)
            || revised.logical_step != root.logical_step
            || revised.model != root.model
            || revised.temperature != root.temperature
            || revised.max_tokens != root.max_tokens
            || revised.thinking_mode != root.thinking_mode
            || revised.thinking_parameter_location != root.thinking_parameter_location
            || revised.input_range_version != root.input_range_version
            || canonical_json(&revised.request())?.len()
                > canonical_json(&root.request())?.len() / 2
        {
            return Err(mismatch());
        }
        let original = root.entries()?;
        for entry in revised.entries()? {
            if !original.iter().any(|prior| {
                prior["source_id"] == entry["source_id"]
                    && prior["part"] == entry["part"]
                    && prior["text"].as_str().is_some_and(|text| {
                        entry["text"]
                            .as_str()
                            .is_some_and(|prefix| text.starts_with(prefix))
                    })
            }) {
                return Err(mismatch());
            }
        }
        if row["request_fingerprint"] != revised.identity(owner)? {
            return Err(mismatch());
        }
        batch = revised;
        break;
    }
    let (parent, row) = if let Some(pinned) = pinned {
        pinned
    } else {
        let row = store
            .successful_call(&run.generation_id, &batch.identity(run)?)?
            .ok_or_else(|| {
                ReportError::new(
                    "RECOVERY_CONFIG_CHANGED",
                    "旧版冻结输入仍有未完成调用，须通过显式修订继续；已发生用量保留",
                )
            })?;
        (
            store.get(row["run_id"].as_str().ok_or_else(mismatch)?)?,
            row,
        )
    };
    if row["status"] != "succeeded"
        || row["logical_step"] != batch.logical_step
        || parent.scope_id != run.scope_id
        || parent.target_date != run.target_date
        || row["request_fingerprint"] != batch.identity(&parent)?
    {
        return Err(mismatch());
    }
    let request = snapshots.read(&super::reference(&row, "request")?)?;
    if request != batch.request() {
        return Err(mismatch());
    }
    let response = snapshots.read(&super::reference(&row, "response")?)?;
    let raw = response.get("response").unwrap_or(&response);
    let (content, _) = super::response_content(&canonical_json(raw)?)?;
    Ok((validate_summary(content, &batch)?, batch, row))
}
