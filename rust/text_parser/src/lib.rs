use ai_daily_scanner_contract::{TextParseProfile, Validate};
use ai_daily_worker_contract::{
    AuditTextBuilder, ContentUnitKind, ParsedContentAudit, SourceRange, SourceRangeUnit,
};
use serde_json::Value;
use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use thiserror::Error;

mod archive;
mod daily_csv;
mod daily_text;
#[cfg(test)]
mod tests;

pub use archive::parse_archive_text;
use daily_csv::audited_csv;
use daily_text::audited_text;

const TEXT_FILE_TYPES: &[&str] = &[".txt", ".md", ".csv", ".json", ".log"];
const LEGACY_CSV_FIELD_SIZE_LIMIT: usize = 131_072;

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
}

#[derive(Debug)]
struct RawExcerpt {
    text: String,
    encoding: &'static str,
    source: &'static str,
    truncated: bool,
    byte_start: u64,
    byte_end: u64,
    /// Absolute byte offset of the first decoded character (after boundary alignment).
    text_byte_start: u64,
}

pub fn parse_light_text(
    file_path: &Path,
    file_type: &str,
    profile: &TextParseProfile,
    max_file_size_bytes: u64,
) -> Result<ParsedLightText, LightTextError> {
    profile
        .validate()
        .map_err(|_| LightTextError::InvalidProfile)?;
    let normalized_type = file_type.to_lowercase();
    if !TEXT_FILE_TYPES.contains(&normalized_type.as_str()) {
        return Err(LightTextError::UnsupportedExtension);
    }
    let mut file = File::open(file_path).map_err(|_| LightTextError::ReadFailed)?;
    let metadata = file
        .metadata()
        .map_err(|_| LightTextError::MetadataFailed)?;
    if metadata.len() > max_file_size_bytes {
        return Err(LightTextError::FileTooLarge);
    }

    let windows = match normalized_type.as_str() {
        ".txt" | ".md" => {
            daily_text::read_windows(&mut file, metadata.len(), profile.read_head_bytes)?
        }
        ".log" => vec![read_tail(
            &mut file,
            metadata.len(),
            profile.read_tail_bytes,
        )?],
        _ => vec![read_head(
            &mut file,
            metadata.len(),
            profile.read_head_bytes,
            (normalized_type == ".csv").then_some(profile.csv_encodings.as_slice()),
        )?],
    };
    let raw = &windows[0];
    let max_output_chars = profile.max_chars.min(profile.excerpt_max_chars);
    let (content, truncated, warning, evidence_audit) = match normalized_type.as_str() {
        ".json" => {
            let (content, truncated, warning) = build_json_content(raw, max_output_chars);
            let audit = ParsedContentAudit {
                read_ranges: Some(byte_ranges(raw)),
                usable_chars: Some(if raw.text.trim().is_empty() {
                    0
                } else {
                    content.chars().count() as u64
                }),
                ..ParsedContentAudit::default()
            };
            (content, truncated, warning, audit)
        }
        ".csv" => audited_csv(raw, max_output_chars),
        _ => audited_text(&windows, max_output_chars, normalized_type == ".log", None),
    };
    Ok(ParsedLightText {
        content,
        evidence_audit,
        parser_backend: profile.backend.clone(),
        truncated,
        warnings: warning.into_iter().collect(),
    })
}

fn read_head(
    file: &mut File,
    file_size: u64,
    read_bytes: u64,
    csv_encodings: Option<&[String]>,
) -> Result<RawExcerpt, LightTextError> {
    let mut raw = Vec::new();
    file.by_ref()
        .take(read_bytes)
        .read_to_end(&mut raw)
        .map_err(|_| LightTextError::ReadFailed)?;
    let truncated = file_size > raw.len() as u64;
    let (text, encoding) = match csv_encodings {
        Some(encodings) => decode_csv(&raw, truncated, encodings)?,
        None => (decode_head(&raw, truncated)?, "utf-8"),
    };
    Ok(RawExcerpt {
        text,
        encoding,
        source: "head",
        truncated,
        byte_start: 0,
        byte_end: raw.len() as u64,
        text_byte_start: 0,
    })
}

fn byte_ranges(raw: &RawExcerpt) -> Vec<SourceRange> {
    if raw.byte_end == raw.byte_start {
        return Vec::new();
    }
    vec![SourceRange::new(
        SourceRangeUnit::Byte,
        raw.byte_start,
        raw.byte_end,
        Some(raw.source.into()),
    )]
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

fn read_tail(
    file: &mut File,
    file_size: u64,
    read_bytes: u64,
) -> Result<RawExcerpt, LightTextError> {
    let start_offset = file_size.saturating_sub(read_bytes);
    let context_offset = start_offset.saturating_sub(3);
    let leading_length =
        usize::try_from(start_offset - context_offset).map_err(|_| LightTextError::ReadFailed)?;
    let bytes_to_read = file_size.saturating_sub(context_offset);
    file.seek(SeekFrom::Start(context_offset))
        .map_err(|_| LightTextError::ReadFailed)?;
    let mut context_and_raw = Vec::new();
    file.take(bytes_to_read)
        .read_to_end(&mut context_and_raw)
        .map_err(|_| LightTextError::ReadFailed)?;
    if context_and_raw.len() < leading_length {
        return Err(LightTextError::ReadFailed);
    }
    let (leading_context, raw) = context_and_raw.split_at(leading_length);
    let truncated = start_offset > 0;
    let text = decode_head(trim_leading_utf8_fragment(raw, leading_context), false)?;
    let byte_end = context_offset + context_and_raw.len() as u64;
    Ok(RawExcerpt {
        text_byte_start: byte_end - text.len() as u64,
        text,
        encoding: "utf-8",
        source: "tail",
        truncated,
        byte_start: context_offset,
        byte_end,
    })
}

fn decode_csv(
    raw: &[u8],
    truncated: bool,
    allowed: &[String],
) -> Result<(String, &'static str), LightTextError> {
    let has_bom = raw.starts_with(&[0xef, 0xbb, 0xbf]);
    let utf_encoding = if has_bom { "utf-8-sig" } else { "utf-8" };
    if allowed.iter().any(|encoding| encoding == utf_encoding) {
        if let Ok(text) = decode_head(raw, truncated) {
            return Ok((text, utf_encoding));
        }
    }
    // A UTF-8 BOM fixes the encoding; malformed BOM-marked input cannot be
    // silently reinterpreted through the alternate encoding.
    if has_bom || !allowed.iter().any(|encoding| encoding == "gb18030") {
        return Err(LightTextError::DecodeFailed);
    }
    let mut decoder = encoding_rs::GB18030.new_decoder_without_bom_handling();
    let capacity = decoder
        .max_utf8_buffer_length_without_replacement(raw.len())
        .ok_or(LightTextError::DecodeFailed)?;
    let mut text = String::with_capacity(capacity);
    let (result, consumed) =
        decoder.decode_to_string_without_replacement(raw, &mut text, !truncated);
    if result != encoding_rs::DecoderResult::InputEmpty || consumed != raw.len() {
        return Err(LightTextError::DecodeFailed);
    }
    // last=false deliberately keeps a split two/four-byte character pending.
    // The CSV reader then emits only complete logical records from this window.
    Ok((text, "gb18030"))
}

fn decode_head(raw: &[u8], trim_trailing_fragment: bool) -> Result<String, LightTextError> {
    match std::str::from_utf8(raw) {
        Ok(text) => Ok(text.to_string()),
        Err(error) if trim_trailing_fragment && error.error_len().is_none() => {
            std::str::from_utf8(&raw[..error.valid_up_to()])
                .map(str::to_string)
                .map_err(|_| LightTextError::DecodeFailed)
        }
        Err(_) => Err(LightTextError::DecodeFailed),
    }
}

fn trim_leading_utf8_fragment<'a>(raw: &'a [u8], leading_context: &[u8]) -> &'a [u8] {
    if raw.first().is_none_or(|byte| !is_utf8_continuation(*byte)) {
        return raw;
    }
    let boundary = leading_context.len();
    let mut combined = Vec::with_capacity(boundary + raw.len().min(3));
    combined.extend_from_slice(leading_context);
    combined.extend_from_slice(&raw[..raw.len().min(3)]);
    for lead_index in boundary.saturating_sub(3)..boundary {
        let sequence_length = utf8_sequence_length(combined[lead_index]);
        let sequence_end = lead_index + sequence_length;
        if sequence_length == 0
            || !(lead_index < boundary && boundary < sequence_end)
            || sequence_end > combined.len()
            || std::str::from_utf8(&combined[lead_index..sequence_end]).is_err()
        {
            continue;
        }
        return &raw[sequence_end - boundary..];
    }
    raw
}

fn is_utf8_continuation(value: u8) -> bool {
    (0x80..=0xbf).contains(&value)
}

fn utf8_sequence_length(first_byte: u8) -> usize {
    match first_byte {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

fn build_json_content(
    raw: &RawExcerpt,
    max_chars: u64,
) -> (String, bool, Option<LightTextWarning>) {
    let payload = if raw.truncated {
        None
    } else {
        serde_json::from_str::<Value>(&normalize_python_json_constants(&raw.text)).ok()
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
    let (content, truncated) = finalize_content(
        |truncated| {
            let meta = preview_metadata(title, raw.source, truncated);
            format!("{meta}\n{detail}\n\n{}", raw.text)
        },
        raw.truncated,
        max_chars,
    );
    (content, truncated, warning)
}

fn normalize_python_json_constants(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    let mut output = String::new();
    let mut copied_until = 0;
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }

        let matched = ["-Infinity", "Infinity", "NaN"]
            .into_iter()
            .find(|token| python_json_constant_at(input, index, token));
        if let Some(token) = matched {
            output.push_str(&input[copied_until..index]);
            output.push_str("0.0");
            index += token.len();
            copied_until = index;
        } else {
            index += 1;
        }
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

fn finalize_content<F>(builder: F, read_truncated: bool, max_chars: u64) -> (String, bool)
where
    F: Fn(bool) -> String,
{
    let content = builder(read_truncated);
    let (limited, output_truncated) = limit_chars(content, max_chars);
    let final_truncated = read_truncated || output_truncated;
    if final_truncated != read_truncated {
        let rebuilt = builder(final_truncated);
        let (limited, rebuilt_truncated) = limit_chars(rebuilt, max_chars);
        return (limited, read_truncated || rebuilt_truncated);
    }
    (limited, final_truncated)
}

fn limit_chars(mut content: String, max_chars: u64) -> (String, bool) {
    let max_chars = usize::try_from(max_chars).unwrap_or(usize::MAX);
    let mut boundaries = content.char_indices();
    let Some((cutoff, _)) = boundaries.nth(max_chars) else {
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

fn parse_csv(input: &str, limit: usize, partial: bool) -> Result<Vec<Vec<String>>, ()> {
    // A discarded tail can contain millions of separators. Find its boundary without
    // allocating fields or rejecting errors in that unfinished logical record.
    let input = if partial {
        let end = csv_tokens(input)
            .filter_map(|(end, token)| matches!(token, CsvToken::RowEnd).then_some(end))
            .last()
            .unwrap_or(0);
        &input[..end]
    } else {
        input
    };
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut field_chars = 0;
    let mut row_start = 0;
    for (end, token) in csv_tokens(input) {
        match token {
            CsvToken::Value(ch) if field_chars < limit => {
                field.push(ch);
                field_chars += 1;
            }
            CsvToken::Invalid | CsvToken::Value(_) => return Err(()),
            CsvToken::FieldEnd | CsvToken::RowEnd => {
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
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}
