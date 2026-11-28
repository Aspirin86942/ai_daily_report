//! Bounded personal summaries keep their evidence in the published report.
use super::data::{CoverageGap, ReportSource};
use crate::{
    evidence::EvidenceRef,
    snapshots::{digest, valid_hash},
    types::{parse_date, utc_window, RunContext},
    ReportError,
};
use chrono::{DateTime, Utc};
use daily_parsers::contract::{SourceRange, SourceRangeUnit};
use minijinja::{context, Environment, Template, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const MAX_FILE_CHARS: usize = 180_000;
pub const MAX_USER_CHARS: usize = 2_000;
pub const MAX_INPUT_CHARS: usize = MAX_FILE_CHARS + MAX_USER_CHARS;
pub const MAX_BODY_CHARS: usize = 1_500;
pub const MAX_ITEMS: usize = 10;
// Historical v5 reports retain their original validation limits.
pub const MAX_SUMMARY_CHARS: usize = 600;
pub const MAX_SUMMARY_ITEMS: usize = 5;
/// New personal plans keep at most five items while allowing the documented body bound.
pub const MAX_SUMMARY_CHARS_EXTENDED: usize = MAX_BODY_CHARS;
pub const MAX_REPORT_BYTES: usize = 1_000_000;
/// One detailed title must fit the overview even when it stands alone.
pub const MAX_OVERVIEW_CHARS: usize = 160;
pub const TEMPLATE: &str = include_str!("../../../../templates/personal_daily_template_v5.md");
pub const SCAN_TEMPLATE: &str =
    include_str!("../../../../templates/personal_daily_scan_template_v5.md");
pub const DETAILED_SCAN_TEMPLATE: &str =
    include_str!("../../../../templates/personal_daily_scan_template_v6.md");
pub const SCAN_TEMPLATE_V7: &str = include_str!("../../../../templates/daily_render_template.md");
const ATTACHMENT_REDUCED: &str = "PERSONAL_EVIDENCE_REDUCED";

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
            || [
                "budget",
                "read_failed",
                "office_content_missing",
                "ocr_budget",
                "file_size_policy",
                "comparison_unavailable",
                "no_usable_text",
            ]
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
    pub(crate) fn sources(&self) -> impl Iterator<Item = &ReportSource> {
        self.materials.iter().map(|material| &material.source)
    }

    pub fn validate(&self) -> Result<(), ReportError> {
        let mut ids = BTreeSet::new();
        let mut chars = 0;
        for material in &self.materials {
            material.source.validate()?;
            if !ids.insert(&material.source.source_id)
                || !matches_excerpt(&material.source, &material.text)
            {
                return Err(invalid("选入材料身份或正文不一致"));
            }
            validate_evidence(&material.source, &material.evidence, &material.text)?;
            chars += material.source.excerpt_chars;
            if material.source.source_kind == "user_statement"
                && material.source.excerpt_chars > MAX_USER_CHARS
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
    validate_item_sources(items, &source_map(sources.iter()))
}

pub(crate) fn source_map<'a>(
    sources: impl Iterator<Item = &'a ReportSource>,
) -> BTreeMap<&'a str, &'a ReportSource> {
    sources
        .map(|source| (source.source_id.as_str(), source))
        .collect()
}

pub(crate) fn validate_item_sources(
    items: &[PersonalItem],
    by_id: &BTreeMap<&str, &ReportSource>,
) -> Result<(), ReportError> {
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

fn matches_excerpt(source: &ReportSource, text: &str) -> bool {
    text.chars().count() == source.excerpt_chars && digest(text.as_bytes()) == source.excerpt_sha256
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
pub struct EvidenceAttachment {
    pub markdown: String,
    pub sha256: String,
}

impl EvidenceAttachment {
    fn new(markdown: String) -> Self {
        Self {
            sha256: digest(markdown.as_bytes()),
            markdown,
        }
    }

    pub fn link(&self, date: &str) -> String {
        format!("attachments/{date}.{}.md", self.sha256)
    }

    pub fn validate(&self) -> Result<(), ReportError> {
        if self.markdown.is_empty()
            || self.markdown.len() > MAX_REPORT_BYTES
            || self.markdown.contains('\r')
            || self.markdown.starts_with('\u{feff}')
            || digest(self.markdown.as_bytes()) != self.sha256
        {
            return Err(invalid("依据附件内容或指纹无效"));
        }
        Ok(())
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_attachment: Option<EvidenceAttachment>,
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
            sources: selection.sources().cloned().collect(),
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
            evidence_attachment: None,
        }
    }

    pub fn validate(&self) -> Result<(), ReportError> {
        if let Some(attachment) = &self.evidence_attachment {
            if self.schema_version == 6 && attachment.markdown.len() > MAX_REPORT_BYTES {
                return Err(too_large());
            }
            attachment.validate()?;
            if self.schema_version == 5
                && (self.items.len() > MAX_SUMMARY_ITEMS
                    || body_chars(&self.items) > MAX_SUMMARY_CHARS_EXTENDED)
            {
                return Err(invalid("个人日报正文最多 1,500 字、5 个重点"));
            }
        }
        self.validate_content()?;
        if self.schema_version == 6 {
            let minimum = self
                .items
                .iter()
                .fold(self.output_bytes(""), |bytes, item| {
                    bytes
                        .saturating_add(item.title.len())
                        .saturating_add(item.text.len())
                });
            if minimum > MAX_REPORT_BYTES {
                return Err(too_large());
            }
        }
        Ok(())
    }

    fn validate_content(&self) -> Result<(), ReportError> {
        if self.schema_version == 6
            && self
                .items
                .iter()
                .any(|item| item.title.chars().count() > MAX_OVERVIEW_CHARS)
        {
            return Err(invalid("详细日报事项标题最多 160 个字符"));
        }
        let window = utc_window(parse_date(&self.date)?)?;
        let utc = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        };
        if !matches!(self.schema_version, 5 | 6)
            || self.report_kind != "personal_daily"
            || self.scope_id != "personal_work"
            || self.timezone != "Asia/Shanghai"
            || self.root_alias.is_empty()
            || self.root_alias.chars().count() > 64
            || utc(&self.window_start_utc) != Some(window.0)
            || utc(&self.window_end_utc) != Some(window.1)
            || utc(&self.generated_at).is_none()
            || !matches!(self.quality.as_str(), "complete" | "partial" | "no_changes")
            || self.schema_version == 5
                && (self.items.len() > MAX_ITEMS || body_chars(&self.items) > MAX_BODY_CHARS)
            || self.user_notes.chars().count() > MAX_USER_CHARS
        {
            return Err(invalid("日报日期、格式或篇幅无效"));
        }
        let mut ids = BTreeSet::new();
        for source in &self.sources {
            source.validate()?;
            if self.schema_version == 6 {
                block_context(source)?;
            }
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
            if !matches_excerpt(source, &text) {
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

    pub(crate) fn validate_saved(&self) -> Result<(), ReportError> {
        self.validate()?;
        if self.schema_version == 6 && self.evidence_attachment.is_none() {
            return Err(invalid("已保存的详细日报缺少依据附件"));
        }
        Ok(())
    }

    pub(crate) fn validate_output(&self, markdown: &str) -> Result<(), ReportError> {
        self.validate_saved()?;
        if self.schema_version == 6
            && (markdown.is_empty() || markdown.contains('\r') || markdown.starts_with('\u{feff}'))
        {
            return Err(invalid("已保存的详细日报正文格式无效"));
        }
        self.validate_output_size(markdown.len() as u64)?;
        if let Some(attachment) = &self.evidence_attachment {
            validate_attachment_link(markdown, &attachment.link(&self.date))?;
        }
        Ok(())
    }

    pub(crate) fn validate_output_size(&self, markdown_bytes: u64) -> Result<(), ReportError> {
        if markdown_bytes.saturating_add(self.output_bytes("") as u64) > MAX_REPORT_BYTES as u64 {
            return Err(too_large());
        }
        Ok(())
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
        if self.schema_version == 6 {
            if let Some(attachment) = &self.evidence_attachment {
                let body = self.detailed_body(template, &attachment.link(&self.date))?;
                self.validate_output(&body)?;
                return Ok(body);
            }
            return self.clone().render_detailed_output(template);
        }
        let link = match &self.evidence_attachment {
            Some(attachment) => attachment.link(&self.date),
            None => self
                .render_attachment(template)?
                .map_or_else(String::new, |attachment| attachment.link(&self.date)),
        };
        self.render_view(template, false, &link)
    }

    /// Both views use the same frozen template. Old inline templates stay inline.
    pub fn render_attachment(
        &self,
        template: &str,
    ) -> Result<Option<EvidenceAttachment>, ReportError> {
        if self.schema_version == 6 {
            if let Some(attachment) = &self.evidence_attachment {
                self.validate()?;
                return Ok(Some(attachment.clone()));
            }
            let mut report = self.clone();
            report.render_detailed_output(template)?;
            return Ok(report.evidence_attachment);
        }
        if !template.starts_with("{% if evidence_only %}") {
            return Ok(None);
        }
        Ok(Some(EvidenceAttachment::new(
            self.render_view(template, true, "")?,
        )))
    }

    pub fn output_bytes(&self, markdown: &str) -> usize {
        markdown.len()
            + self
                .evidence_attachment
                .as_ref()
                .map_or(0, |a| a.markdown.len())
    }

    /// A no-save preview has no files to link to, so carry its evidence inline.
    pub fn render_preview(&self, template: &str) -> Result<String, ReportError> {
        let body = if self.schema_version == 6 {
            if self.evidence_attachment.is_none() {
                let mut report = self.clone();
                report.render_detailed_output(template)?;
                return report.render_preview(template);
            }
            self.validate()?;
            let body = self.detailed_body(template, "")?;
            self.validate_output_size((body.len() + 1) as u64)?;
            body
        } else {
            self.render_view(template, false, "")?
        };
        Ok(match &self.evidence_attachment {
            Some(attachment) => format!("{body}\n{}", attachment.markdown),
            None => body,
        })
    }

    fn render_view(
        &self,
        template: &str,
        evidence_only: bool,
        evidence_link: &str,
    ) -> Result<String, ReportError> {
        self.validate()?;
        let mut value = serde_json::to_value(self)?;
        value["evidence_only"] = json!(evidence_only);
        value["evidence_link"] = json!(evidence_link);
        let mut gaps = Vec::<serde_json::Value>::new();
        for gap in &self.gaps {
            if let Some(group) = gaps.iter_mut().find(|g| g["message"] == gap.message) {
                group["count"] = json!(group["count"].as_u64().unwrap_or(0) + 1);
            } else {
                gaps.push(json!({"message":gap.message,"count":1}));
            }
        }
        value["gaps_grouped"] = json!(gaps);
        value["sources_view"] = json!(self
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                json!({"id":source.source_id,"label":source_label(index),
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

fn source_label(index: usize) -> String {
    format!("S{:03}", index + 1)
}

pub(crate) fn compile_template<'env, 'source>(
    env: &'env Environment<'source>,
    template: &'source str,
) -> Result<Template<'env, 'source>, ReportError> {
    env.template_from_str(template)
        .map_err(|_| invalid("个人日报模板无效"))
}

fn validate_attachment_link(markdown: &str, link: &str) -> Result<(), ReportError> {
    if !markdown.contains(&format!("]({link})")) {
        return Err(invalid("正文缺少完整依据附件链接"));
    }
    Ok(())
}

pub(crate) fn material_labels<'a>(
    sources: impl IntoIterator<Item = &'a ReportSource>,
) -> BTreeMap<String, String> {
    sources
        .into_iter()
        .enumerate()
        .map(|(index, source)| (source.source_id.clone(), source_label(index)))
        .collect()
}

fn invalid(message: &str) -> ReportError {
    ReportError::new("PERSONAL_REPORT_INVALID", message)
}

fn too_large() -> ReportError {
    ReportError::new(
        "PERSONAL_REPORT_TOO_LARGE",
        "正文及必要依据超过 1,000,000 字节",
    )
}

mod bounded;
pub(crate) mod detailed;
pub(crate) use bounded::bounded_body;
use detailed::block_context;
