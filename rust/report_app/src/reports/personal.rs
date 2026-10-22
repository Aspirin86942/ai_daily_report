//! Bounded personal summaries keep their evidence in the published report.
use super::data::{CoverageGap, ReportSource};
use crate::{
    archive::model_input::EvidenceRef,
    snapshots::{digest, valid_hash},
    types::{parse_date, utc_window, RunContext},
    ReportError,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_INPUT_CHARS: usize = 20_000;
pub const MAX_USER_CHARS: usize = 2_000;
pub const MAX_BODY_CHARS: usize = 1_500;
pub const MAX_ITEMS: usize = 10;
pub const MAX_REPORT_BYTES: usize = 100_000;
pub const TEMPLATE: &str = include_str!("../../../../templates/personal_daily_template_v5.md");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Material {
    pub source: ReportSource,
    pub text: String,
    pub evidence: Vec<EvidenceRef>,
    pub priority: u8,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionSummary {
    pub observed_files: u64,
    pub inspected_files: u64,
    pub selected_files: u64,
    pub selected_chars: usize,
    pub incomplete_inventory: bool,
    pub omissions: BTreeMap<String, u64>,
    pub omitted_paths: Vec<String>,
}

impl SelectionSummary {
    pub fn omit(&mut self, reason: &str, path: &str) {
        *self.omissions.entry(reason.into()).or_default() += 1;
        if self.omitted_paths.len() < 10 && !self.omitted_paths.iter().any(|p| p == path) {
            self.omitted_paths.push(path.into());
        }
    }
    pub fn partial(&self) -> bool {
        self.incomplete_inventory
            || ["budget", "read_failed", "comparison_unavailable"]
                .iter()
                .any(|key| self.omissions.get(*key).is_some_and(|n| *n > 0))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub materials: Vec<Material>,
    pub summary: SelectionSummary,
    pub gaps: Vec<CoverageGap>,
}

impl Selection {
    pub fn validate(&self) -> Result<(), ReportError> {
        let mut ids = BTreeSet::new();
        let mut chars = 0;
        for material in &self.materials {
            material.source.validate()?;
            if !ids.insert(&material.source.source_id)
                || material.text.chars().count() != material.source.excerpt_chars
                || digest(material.text.as_bytes()) != material.source.excerpt_sha256
            {
                return Err(invalid("选入材料身份或正文不一致"));
            }
            validate_evidence(&material.source, &material.evidence, &material.text)?;
            chars += material.text.chars().count();
            if material.source.source_kind == "user_statement"
                && material.text.chars().count() > MAX_USER_CHARS
            {
                return Err(ReportError::new(
                    "USER_INPUT_TOO_LARGE",
                    "单日补充最多 2,000 字",
                ));
            }
        }
        if chars > MAX_INPUT_CHARS
            || chars != self.summary.selected_chars
            || self.summary.omitted_paths.len() > 10
        {
            return Err(invalid("选入材料超过范围或字符统计不一致"));
        }
        Ok(())
    }
    pub fn user_notes(&self) -> String {
        self.materials
            .iter()
            .find(|m| m.source.source_id == "user_input")
            .map(|m| m.text.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Progress,
    Attention,
    NextStep,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    Observed,
    UserStatement,
    Uncertain,
    Suggestion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalItem {
    pub title: String,
    pub text: String,
    pub kind: ItemKind,
    pub statement_kind: StatementKind,
    pub source_ids: Vec<String>,
}

pub fn validate_items(items: &[PersonalItem], sources: &[ReportSource]) -> Result<(), ReportError> {
    let by_id: BTreeMap<_, _> = sources.iter().map(|s| (s.source_id.as_str(), s)).collect();
    for item in items {
        if item.title.trim().is_empty() || item.text.trim().is_empty() || item.source_ids.is_empty()
        {
            return Err(invalid("事项缺少文字或来源"));
        }
        let mut seen = BTreeSet::new();
        for id in &item.source_ids {
            let source = by_id
                .get(id.as_str())
                .ok_or_else(|| invalid("事项引用未知来源"))?;
            if !seen.insert(id) || !source.usable() {
                return Err(invalid("事项引用重复或不可用来源"));
            }
            if item.statement_kind == StatementKind::Observed
                && (source.unverified() || source.source_kind == "user_statement")
            {
                return Err(invalid("未确认材料或用户陈述不能标为确定观察"));
            }
        }
        if item.statement_kind == StatementKind::UserStatement
            && !item.source_ids.iter().any(|id| id == "user_input")
        {
            return Err(invalid("用户陈述缺少用户来源"));
        }
    }
    Ok(())
}

pub fn body_chars(items: &[PersonalItem]) -> usize {
    items
        .iter()
        .map(|i| i.title.chars().count() + i.text.chars().count())
        .sum()
}

pub fn evidence_text(refs: &[EvidenceRef]) -> String {
    refs.iter()
        .map(|r| r.quote.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn validate_evidence(
    source: &ReportSource,
    refs: &[EvidenceRef],
    text: &str,
) -> Result<(), ReportError> {
    if source.source_kind == "user_statement" {
        if !refs.is_empty()
            || source.content_sha256.as_deref() != Some(digest(text.as_bytes()).as_str())
        {
            return Err(invalid("用户原文或来源不一致"));
        }
        return Ok(());
    }
    if refs.is_empty() || evidence_text(refs) != text {
        return Err(invalid("来源缺少独立依据"));
    }
    let mut offset = 0;
    for r in refs {
        if r.source_id != source.source_id
            || !matches!(r.side.as_str(), "before" | "after" | "current")
            || !valid_hash(&r.raw_sha256)
            || !valid_hash(&r.unit_sha256)
            || r.quote.is_empty()
            || digest(r.quote.as_bytes()) != r.quote_sha256
            || r.unit_sha256 != r.quote_sha256
            || r.unit_char_start != 0
            || matches!(r.side.as_str(), "after" | "current")
                && source.content_sha256.as_deref() != Some(&r.raw_sha256)
            || r.input_start != offset
            || r.input_end != offset + r.quote.chars().count()
            // Parser-generated headings have no original row/page range. Their
            // exact text remains bound to the archived unit and its fingerprints.
            || r.ranges.iter().any(|range| range.validate().is_err())
            || r.version_id.is_empty()
            || r.extraction_id.is_empty()
            || r.unit_id.is_empty()
        {
            return Err(invalid("原文引用的位置或指纹不一致"));
        }
        offset = r.input_end + 2;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalReport {
    pub schema_version: u32,
    pub report_kind: String,
    pub date: String,
    pub scope_id: String,
    pub root_alias: String,
    pub timezone: String,
    pub window_start_utc: String,
    pub window_end_utc: String,
    pub generated_at: String,
    pub quality: String,
    pub sources: Vec<ReportSource>,
    pub items: Vec<PersonalItem>,
    pub evidence: BTreeMap<String, Vec<EvidenceRef>>,
    pub selection: SelectionSummary,
    pub gaps: Vec<CoverageGap>,
    pub user_notes: String,
}

impl PersonalReport {
    pub fn from_selection(context: &RunContext, root_alias: &str, selection: &Selection) -> Self {
        Self {
            schema_version: 5,
            report_kind: "personal_daily".into(),
            date: context.target_date.to_string(),
            scope_id: context.scope_id.clone(),
            root_alias: root_alias.into(),
            timezone: context.timezone.clone(),
            window_start_utc: context.window_start_utc.to_rfc3339(),
            window_end_utc: context.window_end_utc.to_rfc3339(),
            generated_at: context.started_at.to_rfc3339(),
            quality: if selection.materials.is_empty()
                && selection.gaps.is_empty()
                && !selection.summary.partial()
            {
                "no_changes"
            } else {
                "partial"
            }
            .into(),
            sources: selection
                .materials
                .iter()
                .map(|m| m.source.clone())
                .collect(),
            items: vec![],
            evidence: selection
                .materials
                .iter()
                .filter(|m| m.source.source_kind == "file")
                .map(|m| (m.source.source_id.clone(), m.evidence.clone()))
                .collect(),
            selection: selection.summary.clone(),
            gaps: selection.gaps.clone(),
            user_notes: selection.user_notes(),
        }
    }

    pub fn validate(&self) -> Result<(), ReportError> {
        let window = utc_window(parse_date(&self.date)?)?;
        let utc = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        };
        if self.schema_version != 5
            || self.report_kind != "personal_daily"
            || self.scope_id != "personal_work"
            || self.timezone != "Asia/Shanghai"
            || self.root_alias.is_empty()
            || self.root_alias.chars().count() > 64
            || utc(&self.window_start_utc) != Some(window.0)
            || utc(&self.window_end_utc) != Some(window.1)
            || utc(&self.generated_at).is_none()
            || !matches!(self.quality.as_str(), "complete" | "partial" | "no_changes")
            || self.items.len() > MAX_ITEMS
            || body_chars(&self.items) > MAX_BODY_CHARS
            || self.user_notes.chars().count() > MAX_USER_CHARS
        {
            return Err(invalid("日报日期、格式或篇幅无效"));
        }
        let mut ids = BTreeSet::new();
        for source in &self.sources {
            source.validate()?;
            if !ids.insert(source.source_id.clone()) {
                return Err(invalid("来源编号重复"));
            }
            let refs = self
                .evidence
                .get(&source.source_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let text = if source.source_kind == "user_statement" {
                self.user_notes.clone()
            } else {
                evidence_text(refs)
            };
            if digest(text.as_bytes()) != source.excerpt_sha256
                || text.chars().count() != source.excerpt_chars
            {
                return Err(invalid("长期依据与来源指纹不一致"));
            }
            validate_evidence(source, refs, &text)?;
        }
        if self.evidence.keys().any(|id| !ids.contains(id))
            || !self.user_notes.is_empty() && !ids.contains("user_input")
            || self.quality == "no_changes"
                && (!self.items.is_empty()
                    || !self.sources.is_empty()
                    || !self.gaps.is_empty()
                    || self.selection.partial())
            || self.quality == "complete" && (!self.gaps.is_empty() || self.selection.partial())
        {
            return Err(invalid("日报来源或质量状态不一致"));
        }
        validate_items(&self.items, &self.sources)
    }

    /// Keep whole items and all their evidence together when reducing the output.
    pub fn retain_cited(&mut self) {
        let ids: BTreeSet<_> = self
            .items
            .iter()
            .flat_map(|i| i.source_ids.iter())
            .cloned()
            .collect();
        self.sources
            .retain(|s| ids.contains(&s.source_id) || s.source_id == "user_input");
        self.evidence.retain(|id, _| ids.contains(id));
    }

    pub fn render(&self, template: &str) -> Result<String, ReportError> {
        self.validate()?;
        let mut value = serde_json::to_value(self)?;
        value["sources_view"] = json!(self
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                json!({"id":source.source_id,"label":format!("S{:03}",index+1),
                "path":source.relative_path.as_deref().unwrap_or("用户补充"),
                "observed":source.observed_at_utc,
                "refs":self.evidence.get(&source.source_id)})
            })
            .collect::<Vec<_>>());
        let mut env = super::environment();
        env.add_template("personal", template)
            .map_err(|_| invalid("个人日报模板无效"))?;
        let body = env
            .get_template("personal")
            .and_then(|t| t.render(super::template_value(&value)))
            .map_err(|_| invalid("个人日报排版失败"))?;
        Ok(format!("{}\n", body.replace("\r\n", "\n").trim_end()))
    }
}

fn invalid(message: &str) -> ReportError {
    ReportError::new("PERSONAL_REPORT_INVALID", message)
}
