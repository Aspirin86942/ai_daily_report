use ai_daily_scanner_contract::{TextParseProfile, Validate};
use serde_json::Value;
use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use thiserror::Error;

const TEXT_FILE_TYPES: &[&str] = &[".txt", ".md", ".csv", ".json", ".log"];
const LEGACY_CSV_FIELD_SIZE_LIMIT: usize = 131_072;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLightText {
    pub content: String,
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
    source: &'static str,
    truncated: bool,
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

    let raw = if normalized_type == ".log" {
        read_tail(&mut file, metadata.len(), profile.read_tail_bytes)?
    } else {
        read_head(&mut file, metadata.len(), profile.read_head_bytes)?
    };
    let max_output_chars = profile.max_chars.min(profile.excerpt_max_chars);
    let (content, truncated, warning) = match normalized_type.as_str() {
        ".json" => build_json_content(&raw, max_output_chars),
        ".csv" => build_csv_content(&raw, max_output_chars),
        // 日志是时间序，新信息在尾部——摘录取读窗的最后 max_chars 字符。
        ".log" => build_log_content(&raw, max_output_chars),
        _ => build_text_content(&raw, "Text preview", max_output_chars, None),
    };
    Ok(ParsedLightText {
        content,
        parser_backend: profile.backend.clone(),
        truncated,
        warnings: warning.into_iter().collect(),
    })
}

fn read_head(
    file: &mut File,
    file_size: u64,
    read_bytes: u64,
) -> Result<RawExcerpt, LightTextError> {
    let mut raw = Vec::new();
    file.by_ref()
        .take(read_bytes)
        .read_to_end(&mut raw)
        .map_err(|_| LightTextError::ReadFailed)?;
    let truncated = file_size > raw.len() as u64;
    let text = decode_head(&raw, truncated)?;
    Ok(RawExcerpt {
        text,
        source: "head",
        truncated,
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
    let text = decode_tail(raw, leading_context, truncated)?;
    Ok(RawExcerpt {
        text,
        source: "tail",
        truncated,
    })
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

fn decode_tail(
    raw: &[u8],
    leading_context: &[u8],
    trim_leading_fragment: bool,
) -> Result<String, LightTextError> {
    let trimmed = if trim_leading_fragment {
        trim_leading_utf8_fragment(raw, leading_context)
    } else {
        raw
    };
    std::str::from_utf8(trimmed)
        .map(str::to_string)
        .map_err(|_| LightTextError::DecodeFailed)
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
    if raw.truncated {
        return build_text_content(
            raw,
            "JSON preview",
            max_chars,
            Some(LightTextWarning::JsonPreviewFallback),
        );
    }
    let normalized_json = normalize_python_json_constants(&raw.text);
    let payload: Value = match serde_json::from_str(normalized_json.as_ref()) {
        Ok(payload) => payload,
        Err(_) => {
            return build_text_content(
                raw,
                "JSON preview",
                max_chars,
                Some(LightTextWarning::JsonPreviewFallback),
            );
        }
    };
    let (content, truncated) = finalize_content(
        |truncated| {
            let title = if payload.is_object() {
                "JSON object preview"
            } else {
                "JSON preview"
            };
            let mut lines = metadata_lines(title, raw.source, truncated);
            match &payload {
                Value::Object(values) => {
                    let mut keys: Vec<&str> = values.keys().map(String::as_str).collect();
                    keys.sort_unstable();
                    lines.push(format!("top_level_keys: {}", keys.join(", ")));
                }
                Value::Array(values) => {
                    lines.push("top_level_type: list".to_string());
                    lines.push(format!("top_level_items: {}", values.len()));
                }
                Value::String(_) => lines.push("top_level_type: str".to_string()),
                Value::Number(_) => lines.push(format!(
                    "top_level_type: {}",
                    python_json_number_type(&raw.text)
                )),
                Value::Bool(_) => lines.push("top_level_type: bool".to_string()),
                Value::Null => lines.push("top_level_type: NoneType".to_string()),
            }
            lines.push(String::new());
            lines.push(raw.text.clone());
            lines.join("\n")
        },
        raw.truncated,
        max_chars,
    );
    (content, truncated, None)
}

fn build_csv_content(raw: &RawExcerpt, max_chars: u64) -> (String, bool, Option<LightTextWarning>) {
    if raw.truncated {
        return build_text_content(
            raw,
            "Text preview",
            max_chars,
            Some(LightTextWarning::CsvPreviewFallback),
        );
    }
    let rows = match parse_csv(&raw.text) {
        Ok(rows) => rows,
        Err(()) => {
            return build_text_content(
                raw,
                "Text preview",
                max_chars,
                Some(LightTextWarning::CsvPreviewFallback),
            );
        }
    };
    let (content, truncated) = finalize_content(
        |truncated| {
            let mut lines = metadata_lines("CSV preview", raw.source, truncated);
            if let Some(header) = rows.first() {
                lines.push(format!("header: {}", format_csv_row(header)));
                for (index, row) in rows.iter().skip(1).take(3).enumerate() {
                    lines.push(format!("row {}: {}", index + 1, format_csv_row(row)));
                }
            } else {
                lines.push("header: ".to_string());
            }
            lines.join("\n")
        },
        raw.truncated,
        max_chars,
    );
    (content, truncated, None)
}

fn build_text_content(
    raw: &RawExcerpt,
    title: &str,
    max_chars: u64,
    warning: Option<LightTextWarning>,
) -> (String, bool, Option<LightTextWarning>) {
    let (content, truncated) = finalize_content(
        |truncated| {
            let mut lines = metadata_lines(title, raw.source, truncated);
            if let Some(warning) = warning {
                lines.push(format!("warning: {}", warning.code()));
            }
            lines.push(String::new());
            lines.push(raw.text.clone());
            lines.join("\n")
        },
        raw.truncated,
        max_chars,
    );
    (content, truncated, warning)
}

/// `.log` 摘录：metadata 行保持在开头，正文取读窗的最后 `max_chars` 字符
/// （日志为时间序，新信息在尾部）。
fn build_log_content(raw: &RawExcerpt, max_chars: u64) -> (String, bool, Option<LightTextWarning>) {
    let (content, truncated) = finalize_tail_content(
        |truncated| {
            let mut lines = metadata_lines("Log tail preview", raw.source, truncated);
            lines.push(String::new());
            lines.push(raw.text.clone());
            lines.join("\n")
        },
        raw.truncated,
        max_chars,
    );
    (content, truncated, None)
}

/// `finalize_content` 的尾部变体：正文取最后 `max_chars` 字符，metadata 行
/// （正文前的第一个 `\n\n` 之前）始终保留。
fn finalize_tail_content<F>(builder: F, read_truncated: bool, max_chars: u64) -> (String, bool)
where
    F: Fn(bool) -> String,
{
    let content = builder(read_truncated);
    let (limited, output_truncated) = limit_tail_chars(&content, max_chars);
    let final_truncated = read_truncated || output_truncated;
    if final_truncated != read_truncated {
        let rebuilt = builder(final_truncated);
        let (limited, rebuilt_truncated) = limit_tail_chars(&rebuilt, max_chars);
        return (limited, read_truncated || rebuilt_truncated);
    }
    (limited, final_truncated)
}

/// 保留 metadata 头，只对正文做后缀截断（Unicode scalar 安全）。
fn limit_tail_chars(content: &str, max_chars: u64) -> (String, bool) {
    let max_chars = usize::try_from(max_chars).unwrap_or(usize::MAX);
    let total = content.chars().count();
    if total <= max_chars {
        return (content.to_string(), false);
    }
    let body_start = content
        .match_indices("\n\n")
        .next()
        .map(|(index, _)| index + 2)
        .unwrap_or(0);
    let metadata = &content[..body_start.min(content.len())];
    let body = &content[body_start.min(content.len())..];
    let metadata_chars = metadata.chars().count();
    let body_budget = max_chars.saturating_sub(metadata_chars);
    let body_chars = body.chars().count();
    let skip = body_chars.saturating_sub(body_budget);
    let keep_from = body.char_indices().nth(skip).map(|(index, _)| index).unwrap_or(0);
    (format!("{metadata}{}", &body[keep_from..]), true)
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

fn metadata_lines(title: &str, excerpt_source: &str, truncated: bool) -> Vec<String> {
    vec![
        title.to_string(),
        format!("excerpt_source: {excerpt_source}"),
        format!("truncated: {truncated}"),
    ]
}

fn finalize_content<F>(builder: F, read_truncated: bool, max_chars: u64) -> (String, bool)
where
    F: Fn(bool) -> String,
{
    let content = builder(read_truncated);
    let (limited, output_truncated) = limit_chars(&content, max_chars);
    let final_truncated = read_truncated || output_truncated;
    if final_truncated != read_truncated {
        let rebuilt = builder(final_truncated);
        let (limited, rebuilt_truncated) = limit_chars(&rebuilt, max_chars);
        return (limited, read_truncated || rebuilt_truncated);
    }
    (limited, final_truncated)
}

fn limit_chars(content: &str, max_chars: u64) -> (String, bool) {
    let max_chars = usize::try_from(max_chars).unwrap_or(usize::MAX);
    let mut boundaries = content.char_indices();
    let Some((cutoff, _)) = boundaries.nth(max_chars) else {
        return (content.to_string(), false);
    };
    (content[..cutoff].to_string(), true)
}

fn format_csv_row(row: &[String]) -> String {
    row.iter()
        .map(|cell| cell.trim())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn parse_csv(input: &str) -> Result<Vec<Vec<String>>, ()> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = input.chars().peekable();
    let mut in_quotes = false;
    let mut after_quote = false;
    let mut field_started = false;
    let mut field_chars = 0_usize;
    while let Some(character) = chars.next() {
        if in_quotes {
            if character == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    push_csv_field_char(&mut field, &mut field_chars, '"')?;
                } else {
                    in_quotes = false;
                    after_quote = true;
                }
            } else {
                push_csv_field_char(&mut field, &mut field_chars, character)?;
            }
            continue;
        }
        match character {
            '"' if !field_started => {
                in_quotes = true;
                field_started = true;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                after_quote = false;
                field_started = false;
                field_chars = 0;
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                after_quote = false;
                field_started = false;
                field_chars = 0;
            }
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    return Err(());
                }
                chars.next();
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                after_quote = false;
                field_started = false;
                field_chars = 0;
            }
            _ if after_quote => return Err(()),
            '"' => {
                push_csv_field_char(&mut field, &mut field_chars, character)?;
                field_started = true;
            }
            _ => {
                push_csv_field_char(&mut field, &mut field_chars, character)?;
                field_started = true;
            }
        }
    }
    if in_quotes {
        return Err(());
    }
    if field_started || after_quote || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

fn push_csv_field_char(field: &mut String, field_chars: &mut usize, value: char) -> Result<(), ()> {
    *field_chars = field_chars.checked_add(1).ok_or(())?;
    if *field_chars > LEGACY_CSV_FIELD_SIZE_LIMIT {
        return Err(());
    }
    field.push(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn log_excerpt_keeps_the_recent_tail_lines() {
        let directory = tempdir().expect("temporary root should exist");
        let path = directory.path().join("app.log");
        let lines: Vec<String> = (0..500).map(|i| format!("2026-08-11 {i:04} 日志行")).collect();
        fs::write(&path, lines.join("\n")).expect("log fixture should be written");
        let profile = TextParseProfile {
            backend: "light_text_v1".to_string(),
            read_head_bytes: 1_048_576,
            read_tail_bytes: 1_048_576,
            max_chars: 1_000,
            excerpt_max_chars: 1_000,
        };
        let parsed = parse_light_text(&path, ".log", &profile, 16 * 1024 * 1024)
            .expect("log should parse");

        assert!(parsed.truncated);
        assert!(parsed.content.contains("Log tail preview"));
        assert!(parsed.content.contains("excerpt_source: tail"));
        assert!(parsed.content.ends_with("2026-08-11 0499 日志行"));
        assert!(parsed.content.chars().count() <= 1_000);
    }

    #[test]
    fn text_excerpt_keeps_the_head_for_regular_files() {
        let directory = tempdir().expect("temporary root should exist");
        let path = directory.path().join("notes.md");
        let lines: Vec<String> = (0..500).map(|i| format!("2026-08-11 {i:04} 日志行")).collect();
        fs::write(&path, lines.join("\n")).expect("md fixture should be written");
        let profile = TextParseProfile {
            backend: "light_text_v1".to_string(),
            read_head_bytes: 1_048_576,
            read_tail_bytes: 1_048_576,
            max_chars: 1_000,
            excerpt_max_chars: 1_000,
        };
        let parsed = parse_light_text(&path, ".md", &profile, 16 * 1024 * 1024)
            .expect("md should parse");

        assert!(parsed.truncated);
        assert!(parsed.content.contains("2026-08-11 0000 日志行"));
        assert!(!parsed.content.contains("0499"));
        assert!(parsed.content.chars().count() <= 1_000);
    }

    #[test]
    fn bounded_read_uses_the_same_open_handle_as_its_metadata() {
        let directory = tempdir().expect("temporary root should exist");
        let path = directory.path().join("candidate.txt");
        let archived = directory.path().join("opened.txt");
        fs::write(&path, "opened content").expect("original fixture should be written");
        let mut file = File::open(&path).expect("original fixture should open");
        let metadata = file.metadata().expect("opened metadata should be readable");
        fs::rename(&path, &archived).expect("opened fixture should be renamed");
        fs::write(&path, "replacement content that is much larger")
            .expect("replacement fixture should be written");

        let excerpt = read_head(&mut file, metadata.len(), 1024)
            .expect("the original opened handle should remain readable");

        assert_eq!(excerpt.text, "opened content");
        assert!(!excerpt.truncated);
    }
}
