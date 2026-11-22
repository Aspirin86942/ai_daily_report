use crate::contract::{
    AuditTextBuilder, ContentUnitKind, ParsedContentAudit, SourceRange, SourceRangeUnit,
};
use crate::settings::{TextParseProfile, Validate};
use serde_json::{value::RawValue, Value};
use std::borrow::Cow;
use std::fs::File;
use std::path::Path;
use std::time::Instant;
use thiserror::Error;

mod daily_csv;
mod daily_text;
#[cfg(test)]
mod tests;
mod windows;

use daily_csv::audited_csv;
use daily_text::audited_text;
use windows::RawExcerpt;

const TEXT_FILE_TYPES: &[&str] = &[".txt", ".md", ".csv", ".json", ".log"];
const LEGACY_CSV_FIELD_SIZE_LIMIT: usize = 131_072;
pub const MAX_TEXT_WINDOW_BYTES: u64 = 2 * 1024 * 1024;
type TextOutcome = (String, bool, Option<LightTextWarning>, ParsedContentAudit);

// Charge dependent headings and rows together. A unit already kept is free.
struct Selection {
    kept: Vec<bool>,
    remaining: u64,
    slots: usize,
}

impl Selection {
    fn new(count: usize, remaining: u64, slots: usize) -> Self {
        Self {
            kept: vec![false; count],
            remaining,
            slots,
        }
    }

    fn keep(
        &mut self,
        group: impl Iterator<Item = usize> + Clone,
        cost_of: impl Fn(usize) -> u64,
    ) -> bool {
        let (cost, slots) = group
            .clone()
            .filter(|&i| !self.kept[i])
            .fold((0, 0), |(cost, slots), i| (cost + cost_of(i), slots + 1));
        if cost > self.remaining || slots > self.slots {
            return false;
        }
        for i in group {
            self.kept[i] = true;
        }
        self.remaining -= cost;
        self.slots -= slots;
        true
    }
}

fn matches_label(value: &str, labels: &[&str]) -> bool {
    labels.iter().any(|label| value.eq_ignore_ascii_case(label))
}

fn column_label(cell: &str) -> &str {
    cell.split(['(', '（']).next().unwrap_or("").trim()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLightText {
    pub content: String,
    pub evidence_audit: ParsedContentAudit,
    pub parser_backend: String,
    pub truncated: bool,
    pub warnings: Vec<LightTextWarning>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightTextWarning {
    JsonPreviewFallback,
    CsvPreviewFallback,
}

impl LightTextWarning {
    pub const fn code(self) -> &'static str {
        match self {
            Self::JsonPreviewFallback => "JSON_PREVIEW_FALLBACK",
            Self::CsvPreviewFallback => "CSV_PREVIEW_FALLBACK",
        }
    }
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum LightTextError {
    #[error("light-text parser profile is invalid")]
    InvalidProfile,
    #[error("file extension is not supported by light-text parser")]
    UnsupportedExtension,
    #[error("file exceeds the configured size limit")]
    FileTooLarge,
    #[error("file metadata could not be read")]
    MetadataFailed,
    #[error("file content could not be read")]
    ReadFailed,
    #[error("file content is not valid UTF-8")]
    DecodeFailed,
    #[error("light-text parser reached its deadline")]
    TimedOut,
}

pub fn parse_light_text(
    file_path: &Path,
    file_type: &str,
    profile: &TextParseProfile,
    max_file_size_bytes: u64,
) -> Result<ParsedLightText, LightTextError> {
    parse(file_path, file_type, profile, max_file_size_bytes, None)
}

pub fn parse_light_text_until(
    file_path: &Path,
    file_type: &str,
    profile: &TextParseProfile,
    max_file_size_bytes: u64,
    deadline: Instant,
) -> Result<ParsedLightText, LightTextError> {
    parse(
        file_path,
        file_type,
        profile,
        max_file_size_bytes,
        Some(deadline),
    )
}

fn parse(
    file_path: &Path,
    file_type: &str,
    profile: &TextParseProfile,
    max_file_size_bytes: u64,
    deadline: Option<Instant>,
) -> Result<ParsedLightText, LightTextError> {
    profile
        .validate()
        .map_err(|_| LightTextError::InvalidProfile)?;
    let normalized_type = file_type.to_lowercase();
    if !TEXT_FILE_TYPES.contains(&normalized_type.as_str()) {
        return Err(LightTextError::UnsupportedExtension);
    }
    check_time(deadline)?;
    let mut file = File::open(file_path).map_err(|_| LightTextError::ReadFailed)?;
    let metadata = file
        .metadata()
        .map_err(|_| LightTextError::MetadataFailed)?;
    if !metadata.is_file() {
        return Err(LightTextError::UnsupportedExtension);
    }
    if metadata.len() > max_file_size_bytes {
        return Err(LightTextError::FileTooLarge);
    }

    let windows = windows::read_excerpts(
        &mut file,
        metadata.len(),
        &normalized_type,
        profile,
        deadline,
    )?;
    let raw = &windows[0];
    let max_output_chars = profile.max_chars.min(profile.excerpt_max_chars);
    let (content, truncated, warning, evidence_audit) = match normalized_type.as_str() {
        ".json" => {
            let (content, truncated, warning) =
                build_json_content(raw, max_output_chars, deadline)?;
            let audit = ParsedContentAudit {
                read_ranges: Some(raw.byte_range().into_iter().collect()),
                usable_chars: Some(if raw.text.trim().is_empty() {
                    0
                } else {
                    content.chars().count() as u64
                }),
                ..ParsedContentAudit::default()
            };
            (content, truncated, warning, audit)
        }
        ".csv" => audited_csv(raw, max_output_chars, deadline)?,
        _ => audited_text(
            &windows,
            max_output_chars,
            normalized_type == ".log",
            None,
            deadline,
        )?,
    };
    check_time(deadline)?;
    Ok(ParsedLightText {
        content,
        evidence_audit,
        parser_backend: profile.backend.clone(),
        truncated,
        warnings: warning.into_iter().collect(),
    })
}

fn check_time(deadline: Option<Instant>) -> Result<(), LightTextError> {
    if deadline.is_some_and(|end| Instant::now() >= end) {
        return Err(LightTextError::TimedOut);
    }
    Ok(())
}

/// The same complete lines and sentences are used by both daily parsing and excerpt allocation.
/// A table line and a decimal value stay intact. Returned slices retain their original offsets.
pub fn text_segments(text: &str, include_final_fragment: bool) -> impl Iterator<Item = &str> {
    let mut lines = text.split_inclusive('\n').peekable();
    let mut start = 0;
    std::iter::from_fn(move || {
        let first = lines.next()?;
        let trimmed = first.trim_start();
        let atomic = trimmed.starts_with('#')
            || first.contains(['|', '\t'])
            || daily_text::label_value(trimmed, &["页眉", "页脚", "header", "footer"]).is_some();
        let mut end = start + first.len();
        if !atomic && !trimmed.is_empty() {
            while let Some(next) = lines.next_if(|next| {
                !next.contains(['|', '\t']) && qualifying_continuation(next.trim_start())
            }) {
                end += next.len();
            }
        }
        let line = &text[start..end];
        start = end;
        Some((line, atomic))
    })
    .flat_map(move |(line, atomic_line)| {
        let mut rest = line;
        std::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            if !atomic_line {
                for (byte, ch) in rest.char_indices() {
                    let after = &rest[byte + ch.len_utf8()..];
                    let end = match ch {
                        '。' | '！' | '？' => true,
                        '!' | '?' => after.starts_with(char::is_whitespace),
                        '.' if after.starts_with(char::is_whitespace) => {
                            // Keep initials, abbreviated names and numbered items with the
                            // words they qualify. Decimal points never precede whitespace.
                            rest[..byte]
                                .split_whitespace()
                                .next_back()
                                .is_some_and(|word| {
                                    let abbreviation = word.contains('.')
                                        || (word.len() <= 3
                                            && word.bytes().all(|b| b.is_ascii_alphabetic()));
                                    !(abbreviation || word.ends_with(|c: char| c.is_ascii_digit()))
                                })
                        }
                        _ => false,
                    };
                    if end {
                        let next =
                            after.trim_start_matches(['。', '！', '？', '”', '’', '」', '』']);
                        if qualifying_continuation(next.trim_start()) {
                            continue;
                        }
                        let (part, tail) = rest.split_at(rest.len() - next.len());
                        rest = tail;
                        return Some(part);
                    }
                }
            }
            let part = std::mem::take(&mut rest);
            (include_final_fragment || line.ends_with('\n')).then_some(part)
        })
    })
}

fn qualifying_continuation(text: &str) -> bool {
    ["但", "否则", "除非", "仅当", "例外", "除外", "前提是"]
        .iter()
        .any(|s| text.starts_with(s))
        || ["unless ", "except ", "but ", "only if "].iter().any(|s| {
            text.get(..s.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(s))
        })
}

/// Signals on the actual fact, independent of row numbers, dates or column headings.
pub fn is_attention_fact(text: &str) -> bool {
    let lower = if text.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(text.to_ascii_lowercase())
    } else {
        Cow::Borrowed(text)
    };
    [
        "异常",
        "失败",
        "错误",
        "风险",
        "告警",
        "下一步",
        "待办",
        "待处理",
        "阻塞",
        "合计",
        "小计",
        "总计",
        "exception",
        "error",
        "failed",
        "next step",
        "todo",
        "#ref!",
        "#value!",
        "#div/0!",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || text.match_indices(['-', '−', '－']).any(|(at, sign)| {
            !text[..at].ends_with(|c: char| c.is_ascii_alphanumeric())
                && text[at + sign.len()..].starts_with(|c: char| c.is_ascii_digit())
        })
}

fn markdown_separator(text: &str) -> bool {
    text.contains('|')
        && text.trim_matches('|').split('|').all(|cell| {
            let value = cell.trim();
            value.chars().filter(|ch| *ch == '-').count() >= 3
                && value.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
        })
}

fn build_json_content(
    raw: &RawExcerpt,
    max_chars: u64,
    deadline: Option<Instant>,
) -> Result<(String, bool, Option<LightTextWarning>), LightTextError> {
    let payload = if raw.truncated {
        None
    } else {
        let normalized = normalize_json_for_preview(&raw.text, false);
        match serde_json::from_str::<Value>(&normalized) {
            Ok(value) => Some(value),
            Err(_) => {
                check_time(deadline)?;
                // Check the original number grammar with RawValue. The
                // placeholders only let Value validate the surrounding JSON.
                let bounded_numbers = normalize_json_for_preview(&normalized, true);
                serde_json::from_str::<Value>(&bounded_numbers).ok()
            }
        }
    };
    let warning = payload
        .is_none()
        .then_some(LightTextWarning::JsonPreviewFallback);
    let title = if matches!(payload, Some(Value::Object(_))) {
        "JSON object preview"
    } else {
        "JSON preview"
    };
    let detail = match payload {
        Some(Value::Object(values)) => {
            let mut keys: Vec<_> = values.keys().map(String::as_str).collect();
            keys.sort_unstable();
            format!("top_level_keys: {}", keys.join(", "))
        }
        Some(Value::Array(values)) => {
            format!("top_level_type: list\ntop_level_items: {}", values.len())
        }
        Some(Value::String(_)) => "top_level_type: str".into(),
        Some(Value::Number(_)) => format!("top_level_type: {}", python_json_number_type(&raw.text)),
        Some(Value::Bool(_)) => "top_level_type: bool".into(),
        Some(Value::Null) => "top_level_type: NoneType".into(),
        None => format!("warning: {}", LightTextWarning::JsonPreviewFallback.code()),
    };
    let render = |truncated| {
        let meta = preview_metadata(title, raw.source, truncated);
        limit_chars(format!("{meta}\n{detail}\n\n{}", raw.text), max_chars)
    };
    let (mut content, mut clipped) = render(raw.truncated);
    if clipped && !raw.truncated {
        (content, clipped) = render(true);
    }
    Ok((content, raw.truncated || clipped, warning))
}

fn normalize_json_for_preview(input: &str, replace_numbers: bool) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    let mut output = String::new();
    let mut copied_until = 0;
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let start = index;
        let byte = bytes[start];
        index += 1;
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        if byte == b'"' {
            in_string = true;
            continue;
        }

        let constant = ["-Infinity", "Infinity", "NaN"]
            .into_iter()
            .find(|token| python_json_constant_at(input, start, token));
        let replacement = if let Some(token) = constant {
            index = start + token.len();
            "0.0"
        } else if replace_numbers && matches!(byte, b'-' | b'0'..=b'9') {
            index += bytes[index..]
                .iter()
                .take_while(|&&b| matches!(b, b'0'..=b'9' | b'e' | b'E' | b'.' | b'-' | b'+'))
                .count();
            if serde_json::from_str::<&RawValue>(&input[start..index]).is_err() {
                continue;
            }
            "0"
        } else {
            continue;
        };
        output.push_str(&input[copied_until..start]);
        output.push_str(replacement);
        copied_until = index;
    }
    if copied_until == 0 {
        Cow::Borrowed(input)
    } else {
        output.push_str(&input[copied_until..]);
        Cow::Owned(output)
    }
}

fn python_json_constant_at(input: &str, index: usize, token: &str) -> bool {
    let bytes = input.as_bytes();
    let token = token.as_bytes();
    if !bytes[index..].starts_with(token) {
        return false;
    }
    let before = bytes[..index]
        .iter()
        .rev()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace());
    let after = bytes[index + token.len()..]
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace());
    matches!(before, None | Some(b'[' | b',' | b':'))
        && matches!(after, None | Some(b',' | b']' | b'}'))
}

fn python_json_number_type(input: &str) -> &'static str {
    let value = input.trim();
    if matches!(value, "NaN" | "Infinity" | "-Infinity")
        || value.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
    {
        "float"
    } else {
        "int"
    }
}

fn preview_metadata(title: &str, excerpt_source: &str, truncated: bool) -> String {
    format!("{title}\nexcerpt_source: {excerpt_source}\ntruncated: {truncated}")
}

fn limit_chars(mut content: String, max_chars: u64) -> (String, bool) {
    let max_chars = usize::try_from(max_chars).unwrap_or(usize::MAX);
    let Some((cutoff, _)) = content.char_indices().nth(max_chars) else {
        return (content, false);
    };
    content.truncate(cutoff);
    (content, true)
}

fn format_csv_row(row: &[String]) -> String {
    row.iter()
        .map(|cell| cell.trim())
        .collect::<Vec<_>>()
        .join(" | ")
}

enum CsvToken {
    Value(char),
    FieldEnd,
    RowEnd,
    Invalid,
}

// Report each token's byte end. Both window selection and field decoding must agree on
// literal quotes, escaped quotes, quoted newlines and CRLF.
fn csv_tokens(input: &str) -> impl Iterator<Item = (usize, CsvToken)> + '_ {
    #[derive(PartialEq)]
    enum Field {
        Start,
        Unquoted,
        Quoted,
        Closed,
    }
    let mut state = Field::Start;
    let mut chars = input.chars();
    std::iter::from_fn(move || loop {
        let Some(ch) = chars.next() else {
            return (std::mem::replace(&mut state, Field::Start) == Field::Quoted)
                .then_some((input.len(), CsvToken::Invalid));
        };
        let token = match ch {
            '"' if state == Field::Start => {
                state = Field::Quoted;
                continue;
            }
            '"' if state == Field::Quoted => {
                if !chars.as_str().starts_with('"') {
                    state = Field::Closed;
                    continue;
                }
                chars.next();
                CsvToken::Value('"')
            }
            _ if state == Field::Quoted => CsvToken::Value(ch),
            ',' => {
                state = Field::Start;
                CsvToken::FieldEnd
            }
            '\r' if !chars.as_str().starts_with('\n') => {
                state = Field::Unquoted;
                CsvToken::Invalid
            }
            '\r' | '\n' => {
                if ch == '\r' {
                    chars.next();
                }
                state = Field::Start;
                CsvToken::RowEnd
            }
            _ if state == Field::Closed => {
                state = Field::Unquoted;
                CsvToken::Invalid
            }
            _ => {
                state = Field::Unquoted;
                CsvToken::Value(ch)
            }
        };
        return Some((input.len() - chars.as_str().len(), token));
    })
}

fn parse_csv(
    input: &str,
    limit: usize,
    partial: bool,
    deadline: Option<Instant>,
) -> Result<Vec<Vec<String>>, LightTextError> {
    // A discarded tail can contain millions of separators. Find its boundary without
    // allocating fields or rejecting errors in that unfinished logical record.
    let input = if partial {
        let mut end = 0;
        for (index, (position, token)) in csv_tokens(input).enumerate() {
            if index % 8192 == 0 {
                check_time(deadline)?;
            }
            if matches!(token, CsvToken::RowEnd) {
                end = position;
            }
        }
        &input[..end]
    } else {
        input
    };
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut field_chars = 0;
    let mut row_start = 0;
    let mut fields = 0;
    for (index, (end, token)) in csv_tokens(input).enumerate() {
        if index % 8192 == 0 {
            check_time(deadline)?;
        }
        match token {
            CsvToken::Value(ch) if field_chars < limit => {
                field.push(ch);
                field_chars += 1;
            }
            CsvToken::Invalid | CsvToken::Value(_) => return Err(LightTextError::DecodeFailed),
            CsvToken::FieldEnd | CsvToken::RowEnd => {
                fields += 1;
                if fields > 65_536 || rows.len() >= 16_384 || row.len() >= 1024 {
                    return Err(LightTextError::DecodeFailed);
                }
                row.push(std::mem::take(&mut field));
                field_chars = 0;
                if matches!(token, CsvToken::RowEnd) {
                    rows.push(std::mem::take(&mut row));
                    row_start = end;
                }
            }
        }
    }
    if row_start < input.len() {
        if row.len() >= 1024 || rows.len() >= 16_384 || fields >= 65_536 {
            return Err(LightTextError::DecodeFailed);
        }
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}
