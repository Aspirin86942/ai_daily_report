use crate::ReportError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputUnit {
    pub start: usize,
    pub end: usize,
    pub kind: String,
    #[serde(default)]
    pub ranges: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    pub source_id: String,
    pub source_kind: String,
    pub path: String,
    pub text: String,
    pub units: Vec<InputUnit>,
    pub history_priority: bool,
    pub text_unverified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub source_id: String,
    pub source_kind: String,
    pub path: String,
    pub part: usize,
    pub text: String,
    pub text_unverified: bool,
    pub text_ranges: Vec<TextRange>,
    pub input_units: Vec<InputUnit>,
}

fn essential(kind: &str) -> bool {
    matches!(kind, "heading" | "table_header" | "unit" | "placeholder")
}
pub fn slice(text: &str, start: usize, end: usize) -> String {
    text.chars().skip(start).take(end - start).collect()
}

/// Keep paragraphs and table rows intact. Offsets are Unicode scalar positions,
/// matching the historical Python excerpt and parser content-unit contracts.
pub(crate) fn sentences(text: &str, offset: usize, kind: &str, ranges: &[Value]) -> Vec<InputUnit> {
    let chars: Vec<_> = text.chars().collect();
    let mut units = Vec::new();
    let mut start = 0;
    for (i, c) in chars.iter().enumerate() {
        if matches!(c, '。' | '！' | '？' | '\n')
            || (*c == '.' && chars.get(i + 1).is_some_and(|c| c.is_whitespace()))
            || i + 1 == chars.len()
        {
            if chars[start..=i].iter().any(|c| !c.is_whitespace()) {
                let kind = if kind == "paragraph"
                    && chars[start..=i]
                        .iter()
                        .collect::<String>()
                        .trim_start()
                        .starts_with('#')
                {
                    "heading"
                } else {
                    kind
                };
                units.push(InputUnit {
                    start: offset + start,
                    end: offset + i + 1,
                    kind: kind.into(),
                    ranges: ranges.to_vec(),
                });
            }
            start = i + 1;
        }
    }
    units
}

impl SourceInput {
    pub fn chunks(&self) -> Result<Vec<Entry>, ReportError> {
        if self.source_id.is_empty()
            || self.source_id.len() > 128
            || self.path.len() > 32768
            || self.source_kind.is_empty()
        {
            return Err(ReportError::new(
                "MODEL_SOURCE_INVALID",
                "来源标识或路径无效",
            ));
        }
        let chars = self.text.chars().count();
        let mut units = if self.units.is_empty() {
            let mut offset = 0;
            self.text
                .split_inclusive('\n')
                .map(|line| {
                    let start = offset;
                    offset += line.chars().count();
                    InputUnit {
                        start,
                        end: offset,
                        kind: if line.trim_start().starts_with('#') {
                            "heading"
                        } else {
                            "paragraph"
                        }
                        .into(),
                        ranges: vec![],
                    }
                })
                .collect()
        } else {
            self.units.clone()
        };
        let mut prior = 0;
        for unit in &units {
            if unit.start < prior || unit.end <= unit.start || unit.end > chars {
                return Err(ReportError::new(
                    "MODEL_SOURCE_RANGE_MISMATCH",
                    "来源内容范围无效",
                ));
            }
            prior = unit.end;
        }
        if chars == 0 {
            return Ok(vec![]);
        }
        let first = units.iter().position(|u| !essential(&u.kind)).unwrap_or(0);
        if units.is_empty() {
            units.push(InputUnit {
                start: 0,
                end: chars,
                kind: "paragraph".into(),
                ranges: vec![],
            });
        }
        let mut pieces = Vec::new();
        let mut cursor = 0;
        let mut index = first;
        while index < units.len() {
            let mut end = units[index].end;
            index += 1;
            while index < units.len() && units[index].end - cursor <= 512 {
                end = units[index].end;
                index += 1;
            }
            if index == units.len() {
                end = chars;
            }
            pieces.push((cursor, end));
            cursor = end;
        }
        let mut result = Vec::new();
        for (part, (start, end)) in pieces.into_iter().enumerate() {
            let heading = units
                .iter()
                .rev()
                .find(|u| u.end <= start && u.kind == "heading")
                .map_or(0, |u| u.start);
            let mut spans = vec![TextRange { start, end }];
            for unit in units.iter().rev().filter(|u| {
                u.end <= start
                    && u.start >= heading
                    && essential(&u.kind)
                    && u.kind != "placeholder"
            }) {
                spans.push(TextRange {
                    start: unit.start,
                    end: unit.end,
                });
            }
            spans.sort_by_key(|s| s.start);
            let mut merged: Vec<TextRange> = Vec::new();
            for span in spans {
                if let Some(last) = merged
                    .last_mut()
                    .filter(|last| span.start <= last.end.saturating_add(2))
                {
                    last.end = last.end.max(span.end);
                } else {
                    merged.push(span);
                }
            }
            let mut text = String::new();
            let mut mapped = Vec::new();
            let mut local = 0;
            for span in &merged {
                if !text.is_empty() {
                    text.push('\n');
                    local += 1;
                }
                text.push_str(&slice(&self.text, span.start, span.end));
                for unit in units
                    .iter()
                    .filter(|u| u.start < span.end && u.end > span.start)
                {
                    mapped.push(InputUnit {
                        start: local + unit.start.max(span.start) - span.start,
                        end: local + unit.end.min(span.end) - span.start,
                        kind: unit.kind.clone(),
                        ranges: unit.ranges.clone(),
                    });
                }
                local += span.end - span.start;
            }
            result.push(Entry {
                source_id: self.source_id.clone(),
                source_kind: self.source_kind.clone(),
                path: self.path.clone(),
                part,
                text,
                text_unverified: self.text_unverified,
                text_ranges: merged,
                input_units: mapped,
            });
        }
        Ok(result)
    }
}

impl Entry {
    pub fn split(&self) -> Result<[Self; 2], ReportError> {
        let content: Vec<_> = self
            .input_units
            .iter()
            .filter(|u| !essential(&u.kind))
            .collect();
        if content.len() < 2 {
            return Err(ReportError::new(
                "MODEL_CONTENT_UNIT_TOO_LARGE",
                "单个完整段落或表格行仍超出模型容量",
            ));
        }
        let cut = content[content.len() / 2 - 1].end;
        let heading = self
            .input_units
            .iter()
            .rev()
            .find(|u| u.end <= cut && u.kind == "heading")
            .map_or(0, |u| u.start);
        let mut right = vec![TextRange {
            start: cut,
            end: self.text.chars().count(),
        }];
        right.extend(
            self.input_units
                .iter()
                .filter(|u| {
                    u.end <= cut
                        && u.start >= heading
                        && essential(&u.kind)
                        && u.kind != "placeholder"
                })
                .map(|u| TextRange {
                    start: u.start,
                    end: u.end,
                }),
        );
        Ok([
            self.select_spans(vec![TextRange { start: 0, end: cut }]),
            self.select_spans(right),
        ])
    }

    fn select_spans(&self, mut spans: Vec<TextRange>) -> Self {
        spans.sort_by_key(|span| span.start);
        let mut merged: Vec<TextRange> = Vec::new();
        for span in spans {
            if let Some(last) = merged.last_mut().filter(|last| span.start <= last.end) {
                last.end = last.end.max(span.end);
            } else {
                merged.push(span);
            }
        }
        let mut result = self.clone();
        result.text.clear();
        result.text_ranges.clear();
        result.input_units.clear();
        let mut output_offset = 0;
        for span in merged {
            let mut original_offset = 0;
            for original in &self.text_ranges {
                let end = original_offset + original.end - original.start;
                let start = span.start.max(original_offset);
                let stop = span.end.min(end);
                if start < stop {
                    if !result.text.is_empty() {
                        result.text.push('\n');
                        output_offset += 1;
                    }
                    result.text.push_str(&slice(&self.text, start, stop));
                    result.text_ranges.push(TextRange {
                        start: original.start + start - original_offset,
                        end: original.start + stop - original_offset,
                    });
                    result.input_units.extend(
                        self.input_units
                            .iter()
                            .filter(|u| u.start < stop && u.end > start)
                            .map(|u| InputUnit {
                                start: output_offset + u.start.max(start) - start,
                                end: output_offset + u.end.min(stop) - start,
                                kind: u.kind.clone(),
                                ranges: u.ranges.clone(),
                            }),
                    );
                    output_offset += stop - start;
                }
                original_offset = end + 1;
            }
        }
        result
    }

    pub fn wire(&self) -> Value {
        json!({"source_id":self.source_id,"source_kind":self.source_kind,"path":self.path,"part":self.part,"text":self.text,"text_unverified":self.text_unverified})
    }
    pub fn prefix_ends(&self) -> Vec<usize> {
        self.input_units
            .iter()
            .filter(|u| !essential(&u.kind))
            .map(|u| u.end)
            .collect()
    }
    pub fn clip(&self, end: usize) -> Result<Self, ReportError> {
        if !self.prefix_ends().contains(&end) {
            return Err(ReportError::new(
                "MODEL_SOURCE_RANGE_MISMATCH",
                "缩批必须保留完整内容单元",
            ));
        }
        let mut result = self.clone();
        result.text = slice(&self.text, 0, end);
        result.input_units.retain(|u| u.end <= end);
        result.text_ranges.clear();
        let mut left = end;
        for span in &self.text_ranges {
            if !result.text_ranges.is_empty() {
                left = left.saturating_sub(1);
            }
            if left == 0 {
                break;
            }
            let amount = left.min(span.end - span.start);
            result.text_ranges.push(TextRange {
                start: span.start,
                end: span.start + amount,
            });
            left -= amount;
        }
        Ok(result)
    }
}

pub fn union_chars(spans: &[TextRange]) -> usize {
    let mut sorted = spans.to_vec();
    sorted.sort_by_key(|s| s.start);
    let mut end = 0;
    let mut total = 0;
    for span in sorted {
        total += span.end.saturating_sub(end.max(span.start));
        end = end.max(span.end);
    }
    total
}
