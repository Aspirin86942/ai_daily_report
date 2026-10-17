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

const TEXT_FILE_TYPES: &[&str] = &[".txt", ".md", ".csv", ".json", ".log"];
const LEGACY_CSV_FIELD_SIZE_LIMIT: usize = 131_072;

pub fn parse_archive_text(
    request: &ai_daily_worker_contract::archive::ArchiveTextRequest,
) -> Result<ai_daily_worker_contract::archive::ArchiveArtifact, &'static str> {
    use ai_daily_worker_contract::archive::ArchiveWriter;
    use std::time::{Duration, Instant};
    request.validate().map_err(|_| "PARSER_INVALID_PAYLOAD")?;
    if std::fs::metadata(&request.file_path)
        .map_err(|_| "SOURCE_UNAVAILABLE")?
        .len()
        > request.max_input_bytes
    {
        return Err("FILE_TOO_LARGE");
    }
    let started = Instant::now();
    let deadline = started + Duration::from_millis(request.timeout_ms);
    let csv = request.file_type == ".csv";
    let mut encodings = if csv {
        Vec::new()
    } else {
        vec![encoding_rs::UTF_8]
    };
    if csv {
        for name in &request.csv_encodings {
            let encoding = if name == "gb18030" {
                encoding_rs::GB18030
            } else {
                encoding_rs::UTF_8
            };
            if !encodings.contains(&encoding) {
                encodings.push(encoding);
            }
        }
    }
    for (attempt, encoding) in encodings.into_iter().enumerate() {
        if attempt > 0 {
            // Only files created in this request's private staging directory.
            for name in ["body.md", "units.jsonl", "units.idx"] {
                std::fs::remove_file(request.output_dir.join(name))
                    .map_err(|_| "ARCHIVE_OUTPUT_FAILED")?;
            }
        }
        let mut writer = ArchiveWriter::new(&request.output_dir, request.max_output_bytes)
            .map_err(|_| "ARCHIVE_OUTPUT_FAILED")?;
        let mut decoder = encoding.new_decoder_with_bom_removal();
        let mut file = File::open(&request.file_path).map_err(|_| "SOURCE_UNAVAILABLE")?;
        let mut bytes = [0; 65536];
        let mut record = String::new();
        let mut quoted = false;
        let mut row = 1;
        let mut offset = 0;
        let mut failed = false;
        let mut stopped = false;
        loop {
            if Instant::now() >= deadline {
                return Err("PARSE_DEADLINE");
            }
            let size = file.read(&mut bytes).map_err(|_| "SOURCE_UNAVAILABLE")?;
            let mut decoded = String::with_capacity(512 * 1024);
            let (result, consumed) = decoder.decode_to_string_without_replacement(
                &bytes[..size],
                &mut decoded,
                size == 0,
            );
            if result != encoding_rs::DecoderResult::InputEmpty || consumed != size {
                failed = true;
                break;
            }
            for character in decoded.chars() {
                record.push(character);
                if csv && character == '"' {
                    quoted = !quoted;
                }
                if (character == '\n' && (!csv || !quoted)) || (!csv && record.len() >= 256 * 1024)
                {
                    if !archive_text_record(&mut writer, &record, csv, row, offset) {
                        stopped = true;
                        break;
                    }
                    offset += record.chars().count() as u64;
                    row += 1;
                    record.clear();
                }
                if csv && record.len() >= 8 * 1024 * 1024 {
                    writer.gap("CSV_RECORD_COARSE");
                    let range = SourceRange::new(
                        SourceRangeUnit::CsvRecord,
                        row,
                        row + 1,
                        Some("raw_csv_fragment".into()),
                    );
                    if !writer.push(&record, ContentUnitKind::Text, vec![range]) {
                        stopped = true;
                        break;
                    }
                    record.clear();
                }
            }
            if stopped || size == 0 {
                break;
            }
        }
        if failed {
            drop(writer);
            continue;
        }
        if csv && quoted {
            return Err("CSV_INVALID");
        }
        if !record.is_empty() && !stopped {
            archive_text_record(&mut writer, &record, csv, row, offset);
        }
        let mut artifact = writer.finish();
        if artifact.error.is_some() {
            return Err("ARCHIVE_OUTPUT_FAILED");
        }
        artifact.raw_sha256 = request.raw_sha256.clone();
        artifact.parser_backend = "light_text_v2".into();
        artifact.worker_lane = "rust_text_archive_process_v4".into();
        artifact.duration_ms = started.elapsed().as_millis() as u64;
        if artifact.usable_chars == 0 {
            artifact.gaps.push("NO_USABLE_TEXT".into());
        }
        return Ok(artifact);
    }
    Err("TEXT_DECODE_FAILED")
}

fn archive_text_record(
    writer: &mut ai_daily_worker_contract::archive::ArchiveWriter,
    text: &str,
    csv: bool,
    row: u64,
    offset: u64,
) -> bool {
    if csv {
        let range = SourceRange::new(SourceRangeUnit::CsvRecord, row, row + 1, None);
        match parse_csv_with_limit(text, 8 * 1024 * 1024) {
            Ok(rows) => {
                for cells in rows {
                    let escaped = cells
                        .iter()
                        .map(|v| {
                            v.replace('\\', "\\\\")
                                .replace('|', "\\|")
                                .replace('\r', "")
                                .replace('\n', "<br>")
                        })
                        .collect::<Vec<_>>();
                    if !writer.push(
                        &format!("| {} |", escaped.join(" | ")),
                        if row == 1 {
                            ContentUnitKind::TableHeader
                        } else {
                            ContentUnitKind::TableRow
                        },
                        vec![range.clone()],
                    ) {
                        return false;
                    }
                }
            }
            Err(_) => {
                writer.gap("CSV_RECORD_INVALID");
                return writer.push(text, ContentUnitKind::Text, vec![range]);
            }
        }
        true
    } else {
        let end = offset + text.chars().count() as u64;
        if end == offset {
            return true;
        }
        writer.push(
            text,
            if text.trim_start().starts_with('#') {
                ContentUnitKind::Heading
            } else {
                ContentUnitKind::Text
            },
            vec![SourceRange::new(
                SourceRangeUnit::TextChar,
                offset,
                end,
                None,
            )],
        )
    }
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
}

#[derive(Debug)]
struct RawExcerpt {
    text: String,
    encoding: &'static str,
    source: &'static str,
    truncated: bool,
    byte_start: u64,
    byte_end: u64,
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
        read_head(
            &mut file,
            metadata.len(),
            profile.read_head_bytes,
            (normalized_type == ".csv").then_some(profile.csv_encodings.as_slice()),
        )?
    };
    let max_output_chars = profile.max_chars.min(profile.excerpt_max_chars);
    let (content, truncated, warning, evidence_audit) = match normalized_type.as_str() {
        ".json" => {
            let (content, truncated, warning) = build_json_content(&raw, max_output_chars);
            let audit = ParsedContentAudit {
                read_ranges: Some(byte_ranges(&raw)),
                usable_chars: Some(if raw.text.trim().is_empty() {
                    0
                } else {
                    content.chars().count() as u64
                }),
                ..ParsedContentAudit::default()
            };
            (content, truncated, warning, audit)
        }
        ".csv" => audited_csv(&raw, max_output_chars),
        _ => audited_text(&raw, max_output_chars, normalized_type == ".log", None),
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

fn audited_text(
    raw: &RawExcerpt,
    max_chars: u64,
    tail: bool,
    warning: Option<LightTextWarning>,
) -> (String, bool, Option<LightTextWarning>, ParsedContentAudit) {
    let title = if tail {
        "Log tail preview"
    } else {
        "Text preview"
    };
    let mut builder = AuditTextBuilder::new(max_chars, "text");
    for range in byte_ranges(raw) {
        builder.read(range);
    }
    let mut metadata = vec![
        title.to_string(),
        format!("excerpt_source: {}", raw.source),
        format!("read_window_truncated: {}", raw.truncated),
    ];
    if let Some(warning) = warning {
        metadata.push(format!("warning: {}", warning.code()));
    }
    builder.push(&metadata.join("\n"), ContentUnitKind::Heading, vec![]);
    let mut segments = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (byte, ch) in raw.text.char_indices() {
        chars += 1;
        let table_line = raw.text[start..].trim_start().starts_with('|');
        if ch == '\n'
            || (!table_line
                && (matches!(ch, '。' | '！' | '？')
                    || (ch == '.'
                        && raw.text[byte + 1..]
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_whitespace()))))
        {
            segments.push((start, byte + ch.len_utf8(), chars));
            start = byte + ch.len_utf8();
        }
    }
    // A cut read window cannot claim the final partial sentence or line.
    if start < raw.text.len() && (!raw.truncated || tail) {
        segments.push((start, raw.text.len(), chars));
    }
    let mut pieces = Vec::new();
    let mut previous_chars = 0;
    for (from, to, end_chars) in segments {
        let text = &raw.text[from..to];
        let trimmed = text.trim();
        let leading = text.chars().take_while(|c| c.is_whitespace()).count() as u64;
        let ranges = if !tail && !trimmed.is_empty() {
            vec![SourceRange::new(
                SourceRangeUnit::TextChar,
                previous_chars + leading,
                previous_chars + leading + trimmed.chars().count() as u64,
                None,
            )]
        } else {
            vec![]
        };
        previous_chars = end_chars;
        pieces.push((trimmed, ranges));
    }
    let mut selection_bounded = false;
    if tail {
        // Select complete latest lines, then restore their original order.
        let mut remaining = max_chars.saturating_sub(64);
        let mut selected = Vec::new();
        for (text, ranges) in pieces.into_iter().rev() {
            let size = text.chars().count() as u64 + 1;
            if size <= remaining {
                remaining -= size;
                selected.push((text, ranges));
            } else {
                selection_bounded = true;
            }
        }
        selected.reverse();
        for (text, ranges) in selected {
            builder.push(text, ContentUnitKind::Text, ranges);
        }
    } else {
        let mut index = 0;
        while index < pieces.len() {
            let (text, ranges) = &pieces[index];
            if text.starts_with('|')
                && text.ends_with('|')
                && pieces
                    .get(index + 1)
                    .is_some_and(|(separator, _)| markdown_separator(separator))
            {
                let end = (index + 2..pieces.len())
                    .find(|next| {
                        !pieces[*next].0.starts_with('|') || !pieces[*next].0.ends_with('|')
                    })
                    .unwrap_or(pieces.len());
                if end > index + 2 {
                    let headers = [&pieces[index], &pieces[index + 1]];
                    let header_chars = headers
                        .iter()
                        .map(|(line, _)| line.chars().count() + 1)
                        .sum::<usize>();
                    let mut cursor = index + 2;
                    while cursor < end {
                        let mut lines = headers.iter().map(|(line, _)| *line).collect::<Vec<_>>();
                        let mut physical = headers
                            .iter()
                            .flat_map(|(_, ranges)| ranges.iter().cloned())
                            .collect::<Vec<_>>();
                        let mut size = header_chars;
                        let first = cursor;
                        while cursor < end {
                            let cost = pieces[cursor].0.chars().count() + 1;
                            if cursor > first && size + cost > 512 {
                                break;
                            }
                            size += cost;
                            lines.push(pieces[cursor].0);
                            physical.extend(pieces[cursor].1.iter().cloned());
                            cursor += 1;
                        }
                        builder.push(&lines.join("\n"), ContentUnitKind::TableRow, physical);
                    }
                    index = end;
                    continue;
                }
            }
            let kind = if text.starts_with('#') {
                ContentUnitKind::Heading
            } else {
                ContentUnitKind::Text
            };
            builder.push(text, kind, ranges.clone());
            index += 1;
        }
    }
    let (content, mut audit, bounded) = builder.finish();
    audit.encoding = Some(raw.encoding.into());
    (
        content,
        raw.truncated || bounded || selection_bounded,
        warning,
        audit,
    )
}

fn markdown_separator(text: &str) -> bool {
    text.starts_with('|')
        && text.ends_with('|')
        && text.trim_matches('|').split('|').all(|cell| {
            let value = cell.trim();
            value.chars().filter(|ch| *ch == '-').count() >= 3
                && value.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
        })
}

fn audited_csv(
    raw: &RawExcerpt,
    max_chars: u64,
) -> (String, bool, Option<LightTextWarning>, ParsedContentAudit) {
    let input = if raw.truncated {
        let mut in_quotes = false;
        let mut last = 0;
        let mut chars = raw.text.char_indices().peekable();
        while let Some((byte, ch)) = chars.next() {
            if ch == '"' {
                if in_quotes && chars.peek().is_some_and(|(_, next)| *next == '"') {
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            } else if ch == '\n' && !in_quotes {
                last = byte + 1;
            }
        }
        &raw.text[..last]
    } else {
        &raw.text
    };
    let Ok(rows) = parse_csv(input.trim_start_matches('\u{feff}')) else {
        let (content, truncated, _, audit) = audited_text(
            raw,
            max_chars,
            false,
            Some(LightTextWarning::CsvPreviewFallback),
        );
        return (
            content,
            truncated,
            Some(LightTextWarning::CsvPreviewFallback),
            audit,
        );
    };
    let mut builder = AuditTextBuilder::new(max_chars, "csv");
    for range in byte_ranges(raw) {
        builder.read(range);
    }
    builder.push(
        &metadata_lines("CSV preview", raw.source, raw.truncated).join("\n"),
        ContentUnitKind::Heading,
        vec![],
    );
    for (index, row) in rows.iter().enumerate() {
        let range = SourceRange::new(
            SourceRangeUnit::CsvRecord,
            index as u64 + 1,
            index as u64 + 2,
            None,
        );
        builder.read(range.clone());
        if index > 3 {
            continue;
        }
        let (prefix, kind) = if index == 0 {
            ("header: ".into(), ContentUnitKind::TableHeader)
        } else {
            (format!("row {index}: "), ContentUnitKind::TableRow)
        };
        builder.push(
            &format!("{prefix}{}", format_csv_row(row)),
            kind,
            vec![range],
        );
    }
    let (content, mut audit, bounded) = builder.finish();
    audit.encoding = Some(raw.encoding.into());
    (
        content,
        raw.truncated || bounded || rows.len() > 4,
        None,
        audit,
    )
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
        encoding: "utf-8",
        source: "tail",
        truncated,
        byte_start: context_offset,
        byte_end: context_offset + context_and_raw.len() as u64,
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
    parse_csv_with_limit(input, LEGACY_CSV_FIELD_SIZE_LIMIT)
}
fn parse_csv_with_limit(input: &str, limit: usize) -> Result<Vec<Vec<String>>, ()> {
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
                    push_csv_field_char(&mut field, &mut field_chars, '"', limit)?;
                } else {
                    in_quotes = false;
                    after_quote = true;
                }
            } else {
                push_csv_field_char(&mut field, &mut field_chars, character, limit)?;
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
                push_csv_field_char(&mut field, &mut field_chars, character, limit)?;
                field_started = true;
            }
            _ => {
                push_csv_field_char(&mut field, &mut field_chars, character, limit)?;
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

fn push_csv_field_char(
    field: &mut String,
    field_chars: &mut usize,
    value: char,
    limit: usize,
) -> Result<(), ()> {
    *field_chars = field_chars.checked_add(1).ok_or(())?;
    if *field_chars > limit {
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
        let lines: Vec<String> = (0..500)
            .map(|i| format!("2026-08-11 {i:04} 日志行"))
            .collect();
        fs::write(&path, lines.join("\n")).expect("log fixture should be written");
        let profile = TextParseProfile {
            csv_encodings: ai_daily_scanner_contract::default_csv_encodings(),
            backend: "light_text_v2".to_string(),
            read_head_bytes: 1_048_576,
            read_tail_bytes: 1_048_576,
            max_chars: 1_000,
            excerpt_max_chars: 1_000,
        };
        let parsed =
            parse_light_text(&path, ".log", &profile, 16 * 1024 * 1024).expect("log should parse");

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
        let lines: Vec<String> = (0..500)
            .map(|i| format!("2026-08-11 {i:04} 日志行"))
            .collect();
        fs::write(&path, lines.join("\n")).expect("md fixture should be written");
        let profile = TextParseProfile {
            csv_encodings: ai_daily_scanner_contract::default_csv_encodings(),
            backend: "light_text_v2".to_string(),
            read_head_bytes: 1_048_576,
            read_tail_bytes: 1_048_576,
            max_chars: 1_000,
            excerpt_max_chars: 1_000,
        };
        let parsed =
            parse_light_text(&path, ".md", &profile, 16 * 1024 * 1024).expect("md should parse");

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

        let excerpt = read_head(&mut file, metadata.len(), 1024, None)
            .expect("the original opened handle should remain readable");

        assert_eq!(excerpt.text, "opened content");
        assert!(!excerpt.truncated);
    }
}
