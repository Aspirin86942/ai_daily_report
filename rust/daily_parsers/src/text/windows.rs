use super::{check_time, text_segments, LightTextError, MAX_TEXT_WINDOW_BYTES};
use crate::contract::{SourceRange, SourceRangeUnit};
use crate::settings::TextParseProfile;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::time::Instant;

pub(super) struct RawExcerpt {
    pub(super) text: String,
    pub(super) encoding: &'static str,
    pub(super) source: &'static str,
    pub(super) truncated: bool,
    pub(super) text_byte_start: u64,
    read_bytes: Range<u64>,
}

impl RawExcerpt {
    pub(super) fn byte_range(&self) -> Option<SourceRange> {
        (!self.read_bytes.is_empty()).then(|| {
            SourceRange::new(
                SourceRangeUnit::Byte,
                self.read_bytes.start,
                self.read_bytes.end,
                Some(self.source.into()),
            )
        })
    }
}

enum Window {
    Head,
    Sample(&'static str),
    LogTail(usize),
}

pub(super) fn read_excerpts(
    file: &mut File,
    size: u64,
    extension: &str,
    profile: &TextParseProfile,
    deadline: Option<Instant>,
) -> Result<Vec<RawExcerpt>, LightTextError> {
    let head = profile.read_head_bytes.min(MAX_TEXT_WINDOW_BYTES);
    let third = head / 3;
    let plan = match extension {
        ".log" => {
            let tail = profile.read_tail_bytes.min(MAX_TEXT_WINDOW_BYTES);
            let offset = size.saturating_sub(tail);
            let start = offset.saturating_sub(3);
            vec![(Window::LogTail((offset - start) as usize), start..size)]
        }
        ".txt" | ".md" if size > head && third >= 8 => {
            let gap = size - head;
            let middle = third + gap / 2;
            let tail = size - third;
            // A one-byte gap leaves adjacent windows. Read their prefix together.
            let mut plan = vec![(Window::Head, 0..if gap == 1 { tail - gap } else { third })];
            if gap != 1 {
                plan.push((Window::Sample("middle"), middle..middle + head - 2 * third));
            }
            plan.push((Window::Sample("tail"), tail..size));
            plan
        }
        _ => vec![(Window::Head, 0..size.min(head))],
    };
    let csv = (extension == ".csv").then_some(profile.csv_encodings.as_slice());
    plan.into_iter()
        .map(|(window, range)| read_excerpt(file, window, range, size, csv, deadline))
        .collect()
}

fn read_excerpt(
    file: &mut File,
    window: Window,
    range: Range<u64>,
    size: u64,
    csv: Option<&[String]>,
    deadline: Option<Instant>,
) -> Result<RawExcerpt, LightTextError> {
    let bytes = read_window(file, range.clone(), deadline)?;
    let end = range.start + bytes.len() as u64;
    let (text, encoding, source, offset, truncated) = match window {
        Window::Head => {
            let partial = end < size;
            let (text, encoding) = match csv {
                Some(allowed) => decode_csv(&bytes, partial, allowed)?,
                None => (decode_utf8(&bytes, partial)?.to_owned(), "utf-8"),
            };
            (text, encoding, "head", 0, partial)
        }
        Window::LogTail(leading) => {
            let (context, body) = bytes
                .split_at_checked(leading)
                .ok_or(LightTextError::ReadFailed)?;
            let aligned = trim_leading_utf8_fragment(body, context);
            (
                decode_utf8(aligned, false)?.to_owned(),
                "utf-8",
                "tail",
                bytes.len() - aligned.len(),
                leading != 0,
            )
        }
        Window::Sample(source) => {
            if end != range.end {
                return Err(LightTextError::ReadFailed);
            }
            let (text, offset) = align_sample(&bytes, end < size)?;
            (text.to_owned(), "utf-8", source, offset, true)
        }
    };
    Ok(RawExcerpt {
        text,
        encoding,
        source,
        truncated,
        text_byte_start: range.start + offset as u64,
        read_bytes: range.start..end,
    })
}

fn read_window(
    file: &mut File,
    range: Range<u64>,
    deadline: Option<Instant>,
) -> Result<Vec<u8>, LightTextError> {
    if range.start != 0 {
        check_time(deadline)?;
        file.seek(SeekFrom::Start(range.start))
            .map_err(|_| LightTextError::ReadFailed)?;
    }
    let limit = (range.end - range.start) as usize;
    let mut output = Vec::new();
    let mut bytes = [0; 16 * 1024];
    while output.len() < limit {
        check_time(deadline)?;
        let chunk = bytes.len().min(limit - output.len());
        match file.read(&mut bytes[..chunk]) {
            Ok(0) => break,
            Ok(size) => output.extend_from_slice(&bytes[..size]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(LightTextError::ReadFailed),
        }
    }
    check_time(deadline)?;
    Ok(output)
}

fn align_sample(bytes: &[u8], partial_end: bool) -> Result<(&str, usize), LightTextError> {
    // The lookbehind is inside the budget and proves a split UTF-8 character.
    let guard = bytes[..4]
        .iter()
        .rposition(|&b| matches!(b, b'\n' | 0x0c))
        .map_or(4, |i| i + 1);
    let body = trim_leading_utf8_fragment(&bytes[guard..], &bytes[..guard]);
    let offset = bytes.len() - body.len();
    let partial = bytes[..guard]
        .iter()
        .take(3)
        .take_while(|&&b| is_utf8_continuation(b))
        .count();
    std::str::from_utf8(&bytes[partial..offset]).map_err(|_| LightTextError::DecodeFailed)?;
    let text = decode_utf8(body, partial_end)?;
    if matches!(bytes[offset - 1], b'\n' | 0x0c) {
        return Ok((text, offset));
    }
    // A partial table row cannot establish columns. Prose can resume at a sentence boundary.
    let line = text.split_inclusive(['\n', '\x0c']).next().unwrap_or("");
    let skip = if line.contains(['|', '\t']) {
        line.len()
    } else {
        text_segments(line, true).next().map_or(0, str::len)
    };
    Ok((&text[skip..], offset + skip))
}

fn decode_csv(
    raw: &[u8],
    truncated: bool,
    allowed: &[String],
) -> Result<(String, &'static str), LightTextError> {
    let has_bom = raw.starts_with(&[0xef, 0xbb, 0xbf]);
    let utf_encoding = if has_bom { "utf-8-sig" } else { "utf-8" };
    if allowed.iter().any(|encoding| encoding == utf_encoding) {
        if let Ok(text) = decode_utf8(raw, truncated) {
            return Ok((text.to_owned(), utf_encoding));
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

fn decode_utf8(raw: &[u8], trim_trailing_fragment: bool) -> Result<&str, LightTextError> {
    match std::str::from_utf8(raw) {
        Ok(text) => Ok(text),
        Err(error) if trim_trailing_fragment && error.error_len().is_none() => {
            std::str::from_utf8(&raw[..error.valid_up_to()])
                .map_err(|_| LightTextError::DecodeFailed)
        }
        Err(_) => Err(LightTextError::DecodeFailed),
    }
}

fn trim_leading_utf8_fragment<'a>(raw: &'a [u8], leading_context: &[u8]) -> &'a [u8] {
    if raw.first().is_none_or(|byte| !is_utf8_continuation(*byte)) {
        return raw;
    }
    let context = &leading_context[leading_context.len().saturating_sub(3)..];
    let Some(lead) = context
        .iter()
        .rposition(|&byte| !is_utf8_continuation(byte))
    else {
        return raw;
    };
    let prefix = &context[lead..];
    let mut character = [0; 4];
    let available = raw.len().min(character.len() - prefix.len());
    character[..prefix.len()].copy_from_slice(prefix);
    character[prefix.len()..prefix.len() + available].copy_from_slice(&raw[..available]);
    let skip = (1..=available)
        .find(|&count| std::str::from_utf8(&character[..prefix.len() + count]).is_ok())
        .unwrap_or(0);
    &raw[skip..]
}

fn is_utf8_continuation(value: u8) -> bool {
    (0x80..=0xbf).contains(&value)
}
