//! Bounded personal summaries keep their evidence in the published report.
use super::data::{CoverageGap, ReportSource};
use crate::{
    evidence::EvidenceRef,
    snapshots::{digest, valid_hash},
    types::{parse_date, utc_window, RunContext},
    ReportError,
};
use ai_daily_worker_contract::{SourceRange, SourceRangeUnit};
use chrono::{DateTime, Utc};
use minijinja::{context, Template, Value};
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
pub struct EvidenceAttachment {
    pub markdown: String,
    pub sha256: String,
}

impl EvidenceAttachment {
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
            if !markdown.contains(&format!("]({})", attachment.link(&self.date))) {
                return Err(invalid("正文缺少完整依据附件链接"));
            }
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
        let attachment = match &self.evidence_attachment {
            Some(attachment) => Some(attachment.clone()),
            None => self.render_attachment(template)?,
        };
        let link = attachment.as_ref().map(|a| a.link(&self.date));
        self.render_view(template, false, link.as_deref().unwrap_or(""))
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
        let markdown = self.render_view(template, true, "")?;
        Ok(Some(EvidenceAttachment {
            sha256: digest(markdown.as_bytes()),
            markdown,
        }))
    }

    pub fn output_bytes(&self, markdown: &str) -> usize {
        markdown.len()
            + self
                .evidence_attachment
                .as_ref()
                .map_or(0, |a| a.markdown.len())
    }

    /// Freeze the attachment and size both Markdown files before changing this report.
    pub fn render_detailed_output(&mut self, template: &str) -> Result<String, ReportError> {
        if self.schema_version != 6 {
            return Err(invalid("详细日报需要格式 v6"));
        }
        self.validate_content()?;
        let env = super::environment();
        let template = env
            .template_from_str(template)
            .map_err(|_| invalid("个人日报模板无效"))?;
        let labels = self.detailed_labels();
        let cited: BTreeSet<_> = self
            .items
            .iter()
            .flat_map(|item| &item.source_ids)
            .collect();
        let mut order: Vec<_> = self.sources.iter().enumerate().collect();
        // Keep the first copy of shared text before any link to it, even after dropping a suffix.
        order.sort_by_key(|(index, source)| {
            let start = source.coverage_limits["personal"]["excerpt_start"]
                .as_u64()
                .or_else(|| {
                    self.evidence
                        .get(&source.source_id)?
                        .first()?
                        .ranges
                        .first()
                        .map(|r| r.start)
                });
            (
                !cited.contains(&source.source_id),
                &source.relative_path,
                start,
                *index,
            )
        });
        let mut attachment = self.detailed_frame(&template, &labels, true, "", &self.quality)?;
        let mut seen = HashMap::new();
        let mut headings = HashMap::new();
        for source in &self.sources {
            let path = source.relative_path.as_deref().unwrap_or("");
            for entry in source.coverage_limits["personal"]["context"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(text) = entry["text"].as_str() {
                    headings.insert((path, text.trim()), None);
                }
            }
        }
        let mut supplements = Vec::new();
        let mut section = None;
        let mut supplement_start = attachment.len();
        for (index, source) in order {
            let required = cited.contains(&source.source_id);
            if section != Some(required) {
                if !required {
                    supplement_start = attachment.len();
                }
                attachment.push('\n');
                attachment.push_str(&fragment(
                    &template,
                    context! { section => "heading", required },
                )?);
                section = Some(required);
            }
            if !required {
                supplements.push((attachment.len(), index));
            }
            attachment.push('\n');
            attachment.push_str(&self.detailed_material(
                &template,
                source,
                &labels[source.source_id.as_str()],
                &mut seen,
                &mut headings,
            )?);
        }

        // The digest has fixed width. Measure the linked body before its attachment hash exists.
        let placeholder = format!("attachments/{}.{}.md", self.date, "0".repeat(64));
        let body_len = self
            .detailed_frame(&template, &labels, false, &placeholder, &self.quality)?
            .len();
        let mut reduction = None;
        if body_len + attachment.len() > MAX_REPORT_BYTES {
            let body_len = self
                .detailed_frame(&template, &labels, false, &placeholder, "partial")?
                .len();
            let available = MAX_REPORT_BYTES
                .checked_sub(body_len)
                .ok_or_else(too_large)?;
            let &(_, last) = supplements.last().ok_or_else(too_large)?;
            // Each offset is the start of a whole source. No re-render or re-hash per removal.
            for (kept, &(offset, first)) in supplements.iter().enumerate().rev() {
                let end = if kept == 0 { supplement_start } else { offset };
                if end >= available {
                    continue;
                }
                let count = supplements.len() - kept;
                let span = if first == last {
                    self.block_scope(first)
                } else {
                    format!("{} 至 {}", self.block_scope(first), self.block_scope(last))
                };
                let message =
                    format!("附件因字节上限省略了 {count} 块补充材料，按路径排列的范围为 {span}。");
                let mut note = format!("\n{}\n", super::markdown_inline(&message));
                if end + note.len() > available {
                    note = format!(
                        "\n省略补充材料 {count} 块（S{:03} 至 S{:03}，按路径排列）。\n",
                        first + 1,
                        last + 1
                    );
                }
                if end + note.len() <= available {
                    attachment.truncate(end);
                    attachment.push_str(&note);
                    reduction = Some(CoverageGap::new(
                        ATTACHMENT_REDUCED,
                        "publish",
                        None,
                        &message,
                    ));
                    break;
                }
            }
            if reduction.is_none() {
                return Err(too_large());
            }
        }
        let attachment = EvidenceAttachment {
            sha256: digest(attachment.as_bytes()),
            markdown: attachment,
        };
        let quality = if reduction.is_some() {
            "partial"
        } else {
            &self.quality
        };
        let body = self.detailed_frame(
            &template,
            &labels,
            false,
            &attachment.link(&self.date),
            quality,
        )?;
        if body.len() + attachment.markdown.len() > MAX_REPORT_BYTES {
            return Err(too_large());
        }
        if !body.contains(&format!("]({})", attachment.link(&self.date))) {
            return Err(invalid("正文缺少完整依据附件链接"));
        }
        self.gaps.retain(|gap| gap.code != ATTACHMENT_REDUCED);
        if let Some(gap) = reduction {
            self.gaps.push(gap);
            self.quality = "partial".into();
        }
        self.evidence_attachment = Some(attachment);
        Ok(body)
    }

    fn block_scope(&self, index: usize) -> String {
        let source = &self.sources[index];
        let path = source.relative_path.as_deref().unwrap_or("用户补充");
        let mut ranges = self
            .evidence
            .get(&source.source_id)
            .into_iter()
            .flatten()
            .flat_map(|r| &r.ranges);
        let location = match (ranges.next(), ranges.next_back()) {
            (Some(first), Some(last)) if first != last => {
                format!(" · {} 至 {}", range_label(first), range_label(last))
            }
            (Some(first), _) => format!(" · {}", range_label(first)),
            _ => String::new(),
        };
        format!("S{:03}（{path}{location}）", index + 1)
    }

    fn detailed_labels(&self) -> BTreeMap<&str, String> {
        self.sources
            .iter()
            .enumerate()
            .map(|(i, source)| (source.source_id.as_str(), format!("S{:03}", i + 1)))
            .collect()
    }

    fn detailed_body(&self, template: &str, link: &str) -> Result<String, ReportError> {
        let env = super::environment();
        let template = env
            .template_from_str(template)
            .map_err(|_| invalid("个人日报模板无效"))?;
        self.detailed_frame(
            &template,
            &self.detailed_labels(),
            false,
            link,
            &self.quality,
        )
    }

    fn detailed_frame(
        &self,
        template: &Template<'_, '_>,
        labels: &BTreeMap<&str, String>,
        evidence_only: bool,
        link: &str,
        quality: &str,
    ) -> Result<String, ReportError> {
        let mut overview = Vec::new();
        let mut remaining = MAX_OVERVIEW_CHARS;
        if !evidence_only {
            for (index, item) in self.items.iter().enumerate() {
                let chars = item.title.chars().count();
                if chars <= remaining {
                    overview.push(json!({"number":index + 1, "title":item.title, "statement_kind":item.statement_kind}));
                    remaining -= chars;
                    if remaining == 0 || overview.len() == 3 {
                        break;
                    }
                }
            }
        }
        let mut gaps = BTreeMap::new();
        for gap in &self.gaps {
            if gap.code != ATTACHMENT_REDUCED {
                *gaps.entry(&gap.message).or_insert(0usize) += 1;
            }
        }
        fragment(
            template,
            super::template_value(&json!({
                "section":"report", "evidence_only":evidence_only, "evidence_link":link, "date":self.date,
                "selection":self.selection, "items":self.items, "overview":overview, "labels":labels, "quality":quality, "gaps":gaps,
            })),
        )
    }

    fn detailed_material<'a>(
        &'a self,
        template: &Template<'_, '_>,
        source: &'a ReportSource,
        label: &str,
        seen: &mut HashMap<&'a str, String>,
        headings: &mut HashMap<(&'a str, &'a str), Option<String>>,
    ) -> Result<String, ReportError> {
        let refs = self
            .evidence
            .get(&source.source_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let contexts = block_context(source)?;
        let mut parts = Vec::new();
        let anchor = label.to_ascii_lowercase();
        let path = source.relative_path.as_deref().unwrap_or("");
        let mut add = |kind: &str, text: &'a str, ranges: &[SourceRange], shared: Option<&str>| {
            let part_anchor = format!("{anchor}-p{}", parts.len() + 1);
            let previous = shared.or_else(|| seen.get(text).map(String::as_str));
            let printed_at = previous.unwrap_or(&part_anchor).to_owned();
            let mut range = ranges
                .iter()
                .map(range_label)
                .collect::<Vec<_>>()
                .join("、");
            if range.is_empty() && kind == "本次读取" {
                let metadata = &source.coverage_limits["personal"];
                if let (Some(start), Some(end)) = (
                    metadata["excerpt_start"].as_u64(),
                    metadata["excerpt_end"].as_u64(),
                ) {
                    range = format!("摘录字符 [{start}, {end})");
                }
            }
            parts.push(context! {
                kind, range, anchor => &part_anchor, previous,
                text => if previous.is_none() { text } else { "" },
            });
            if previous.is_none() {
                seen.insert(text, part_anchor);
            }
            printed_at
        };
        if !contexts.is_empty() {
            let lines: BTreeSet<_> = refs
                .iter()
                .flat_map(|r| r.quote.lines().map(str::trim))
                .collect();
            for context in contexts {
                if !context.text.lines().all(|line| lines.contains(line.trim())) {
                    let shared = headings
                        .get(&(path, context.text.trim()))
                        .and_then(Option::as_deref);
                    add("表头、标题或单位", context.text, &context.ranges, shared);
                }
            }
        }
        for reference in refs {
            let kind = match reference.side.as_str() {
                "before" => "变更前",
                "after" => "变更后",
                _ => "本次读取",
            };
            let printed_at = add(kind, &reference.quote, &reference.ranges, None);
            for line in reference.quote.lines() {
                if let Some(location) = headings.get_mut(&(path, line.trim())) {
                    location.get_or_insert_with(|| printed_at.clone());
                }
            }
        }
        if source.source_kind == "user_statement" {
            add("用户陈述", &self.user_notes, &[], None);
        }
        fragment(
            template,
            context! {
                section => "material", label, anchor, path => source.relative_path.as_deref(),
                observed => &source.observed_at_utc, unverified => source.unverified(), parts,
            },
        )
    }

    /// A no-save preview has no files to link to, so carry its evidence inline.
    pub fn render_preview(&self, template: &str) -> Result<String, ReportError> {
        if self.schema_version == 6 {
            if let Some(attachment) = &self.evidence_attachment {
                self.validate()?;
                let body = self.detailed_body(template, "")?;
                if self.output_bytes(&body) + 1 > MAX_REPORT_BYTES {
                    return Err(too_large());
                }
                return Ok(format!("{body}\n{}", attachment.markdown));
            }
            let mut report = self.clone();
            report.render_detailed_output(template)?;
            return report.render_preview(template);
        }
        let body = self.render_view(template, false, "")?;
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

fn too_large() -> ReportError {
    ReportError::new(
        "PERSONAL_REPORT_TOO_LARGE",
        "正文及必要依据超过 1,000,000 字节",
    )
}

fn fragment(template: &Template<'_, '_>, view: Value) -> Result<String, ReportError> {
    let text = template
        .render(view)
        .map_err(|_| invalid("个人日报排版失败"))?;
    Ok(format!(
        "{}\n",
        text.replace("\r\n", "\n").replace('\r', "\n").trim_end()
    ))
}

#[derive(Deserialize)]
struct BlockContext<'a> {
    text: &'a str,
    #[serde(default)]
    ranges: Vec<SourceRange>,
}

fn block_context(source: &ReportSource) -> Result<Vec<BlockContext<'_>>, ReportError> {
    let value = &source.coverage_limits["personal"]["context"];
    if value.is_null() {
        return Ok(Vec::new());
    }
    let contexts =
        Vec::<BlockContext<'_>>::deserialize(value).map_err(|_| invalid("材料上下文格式无效"))?;
    if contexts
        .iter()
        .any(|c| c.text.trim().is_empty() || c.ranges.iter().any(|r| r.validate().is_err()))
    {
        return Err(invalid("材料上下文的原文或位置无效"));
    }
    Ok(contexts)
}

fn range_label(range: &SourceRange) -> String {
    // Stored ranges are half-open. Display page, row and column numbers inclusively.
    let positions = |start: u64, end: u64| {
        if end - start == 1 {
            start.to_string()
        } else {
            format!("{start}–{}", end - 1)
        }
    };
    let unit = match range.unit {
        SourceRangeUnit::PdfPage => "页",
        SourceRangeUnit::SheetRow | SourceRangeUnit::TableRow => "行",
        SourceRangeUnit::Paragraph => "段",
        SourceRangeUnit::Slide => "张幻灯片",
        SourceRangeUnit::CsvRecord => "条记录",
        SourceRangeUnit::TextChar => "字符",
        SourceRangeUnit::Byte => "字节",
    };
    let mut location = match range.unit {
        SourceRangeUnit::TextChar | SourceRangeUnit::Byte => {
            format!("{unit} [{}, {})", range.start, range.end)
        }
        _ => format!("第 {} {unit}", positions(range.start, range.end)),
    };
    if let (Some(start), Some(end)) = (range.column_start, range.column_end) {
        location.push_str(&format!("，第 {} 列", positions(start, end)));
    }
    if let Some(label) = &range.label {
        location = format!("{label} · {location}");
    }
    location
}
