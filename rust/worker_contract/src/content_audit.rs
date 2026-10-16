//! Bounded original positions and logical text units. Ranges are half-open.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRangeUnit {
    PdfPage,
    SheetRow,
    Paragraph,
    TableRow,
    Slide,
    TextChar,
    CsvRecord,
    Byte,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRange {
    pub unit: SourceRangeUnit,
    pub start: u64,
    pub end: u64,
    pub label: Option<String>,
    pub column_start: Option<u64>,
    pub column_end: Option<u64>,
}

impl SourceRange {
    pub fn new(unit: SourceRangeUnit, start: u64, end: u64, label: Option<String>) -> Self {
        Self {
            unit,
            start,
            end,
            label,
            column_start: None,
            column_end: None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.start >= self.end
            || (!matches!(self.unit, SourceRangeUnit::TextChar | SourceRangeUnit::Byte)
                && self.start == 0)
            || self
                .label
                .as_ref()
                .is_some_and(|label| label.chars().count() > 512)
        {
            return Err("source range is invalid".to_string());
        }
        match (self.column_start, self.column_end) {
            (None, None) => Ok(()),
            (Some(start), Some(end))
                if start > 0
                    && start < end
                    && matches!(
                        self.unit,
                        SourceRangeUnit::SheetRow | SourceRangeUnit::TableRow
                    ) =>
            {
                Ok(())
            }
            _ => Err("source column range is invalid".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentUnitKind {
    Heading,
    Paragraph,
    TableHeader,
    TableRow,
    Unit,
    Total,
    Slide,
    Text,
    Placeholder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentUnit {
    pub unit_id: String,
    pub kind: ContentUnitKind,
    pub text_start: u64,
    pub text_end: u64,
    pub ranges: Vec<SourceRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParsedContentAudit {
    pub version: u64,
    pub read_ranges: Option<Vec<SourceRange>>,
    pub content_units: Option<Vec<ContentUnit>>,
    pub usable_chars: Option<u64>,
    pub encoding: Option<String>,
    pub detected_format: Option<String>,
}

impl Default for ParsedContentAudit {
    fn default() -> Self {
        Self {
            version: 1,
            read_ranges: None,
            content_units: None,
            usable_chars: None,
            encoding: None,
            detected_format: None,
        }
    }
}

impl ParsedContentAudit {
    pub fn validate(&self, content: &str) -> Result<(), String> {
        let text_chars = content.chars().count() as u64;
        if self.version != 1
            || self.usable_chars.is_some_and(|chars| chars > text_chars)
            || self
                .read_ranges
                .as_ref()
                .is_some_and(|ranges| ranges.len() > 10_000)
            || self
                .content_units
                .as_ref()
                .is_some_and(|units| units.len() > 10_000)
            || self.encoding.as_ref().is_some_and(|encoding| {
                !matches!(encoding.as_str(), "utf-8" | "utf-8-sig" | "gb18030")
            })
            || self.detected_format.as_ref().is_some_and(|format| {
                !matches!(
                    format.as_str(),
                    "xls" | "xlsx" | "doc" | "docx" | "ppt" | "pptx" | "pdf" | "text" | "csv"
                )
            })
        {
            return Err("parser audit fields are invalid".to_string());
        }
        for range in self.read_ranges.iter().flatten() {
            range.validate()?;
        }
        let mut identities = HashSet::new();
        let mut previous_end = 0;
        for unit in self.content_units.iter().flatten() {
            if unit.unit_id.is_empty()
                || unit.unit_id.chars().count() > 128
                || !identities.insert(&unit.unit_id)
                || unit.text_start >= unit.text_end
                || unit.text_end > text_chars
                || unit.ranges.len() > 128
                || unit.text_start < previous_end
            {
                return Err("parser content unit is invalid".to_string());
            }
            previous_end = unit.text_end;
            for range in &unit.ranges {
                range.validate()?;
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| "parser audit cannot be encoded")?
            .len()
            > 256 * 1024
        {
            return Err("parser audit exceeds metadata byte budget".to_string());
        }
        Ok(())
    }
}

/// Builds complete semantic units and their Unicode offsets in the same operation.
pub struct AuditTextBuilder {
    content: String,
    chars: u64,
    max_chars: u64,
    audit: ParsedContentAudit,
    truncated: bool,
}

impl AuditTextBuilder {
    pub fn new(max_chars: u64, format: &str) -> Self {
        Self {
            content: String::new(),
            chars: 0,
            max_chars,
            audit: ParsedContentAudit {
                read_ranges: Some(Vec::new()),
                content_units: Some(Vec::new()),
                usable_chars: Some(0),
                detected_format: Some(format.into()),
                ..Default::default()
            },
            truncated: false,
        }
    }

    pub fn read(&mut self, range: SourceRange) {
        let Some(ranges) = self.audit.read_ranges.as_mut() else {
            return;
        };
        if ranges.len() < 512 && !ranges.contains(&range) {
            ranges.push(range);
        } else if !ranges.contains(&range) {
            self.audit.read_ranges = None;
        }
        if serde_json::to_vec(&self.audit).unwrap().len() > 256 * 1024 {
            self.audit.read_ranges = None;
        }
    }

    pub fn push(&mut self, text: &str, kind: ContentUnitKind, ranges: Vec<SourceRange>) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return true;
        }
        let sep = if self.content.is_empty() { "" } else { "\n" };
        let size = text.chars().count() as u64;
        let units = self.audit.content_units.as_mut().unwrap();
        if self.chars + sep.len() as u64 + size > self.max_chars || units.len() >= 512 {
            self.truncated = true;
            return false;
        }
        let start = self.chars + sep.len() as u64;
        units.push(ContentUnit {
            unit_id: format!("u{}", units.len() + 1),
            kind,
            text_start: start,
            text_end: start + size,
            ranges,
        });
        if serde_json::to_vec(&self.audit).unwrap().len() > 256 * 1024 {
            self.audit.content_units.as_mut().unwrap().pop();
            self.truncated = true;
            return false;
        }
        self.content.push_str(sep);
        self.content.push_str(text);
        self.chars = start + size;
        if !matches!(
            kind,
            ContentUnitKind::Heading
                | ContentUnitKind::TableHeader
                | ContentUnitKind::Unit
                | ContentUnitKind::Placeholder
        ) {
            *self.audit.usable_chars.as_mut().unwrap() += size;
        }
        true
    }

    pub fn finish(mut self) -> (String, ParsedContentAudit, bool) {
        if self.audit.usable_chars == Some(0) {
            self.content.clear();
            self.audit.content_units = Some(Vec::new());
        }
        (self.content, self.audit, self.truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_rows_and_unicode_units_are_bounded() {
        let text = "金额1234.56元𠀀。";
        let range = SourceRange::new(SourceRangeUnit::SheetRow, 2, 3, Some("采购".into()));
        let audit = ParsedContentAudit {
            read_ranges: Some(vec![range.clone()]),
            content_units: Some(vec![ContentUnit {
                unit_id: "row-2".into(),
                kind: ContentUnitKind::TableRow,
                text_start: 0,
                text_end: text.chars().count() as u64,
                ranges: vec![range],
            }]),
            usable_chars: Some(text.chars().count() as u64),
            ..Default::default()
        };
        assert!(audit.validate(text).is_ok());
        assert!(audit.validate("金额1234.56元𠀀").is_err());
        assert!(SourceRange::new(SourceRangeUnit::PdfPage, 0, 1, None)
            .validate()
            .is_err());
    }

    #[test]
    fn unknown_and_verified_empty_ranges_have_different_wire_values() {
        let unknown = ParsedContentAudit::default();
        let empty = ParsedContentAudit {
            read_ranges: Some(vec![]),
            content_units: Some(vec![]),
            usable_chars: Some(0),
            ..Default::default()
        };
        assert!(empty.validate("").is_ok());
        assert_ne!(
            serde_json::to_value(unknown).unwrap(),
            serde_json::to_value(empty).unwrap()
        );
    }
}
