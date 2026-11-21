use super::{
    byte_ranges, decode_head, is_attention_fact, markdown_separator, read_head, text_segments,
    trim_leading_utf8_fragment, LightTextError, LightTextWarning, RawExcerpt,
};
use ai_daily_worker_contract::{
    AuditTextBuilder, ContentUnit, ContentUnitKind, ParsedContentAudit, SourceRange,
    SourceRangeUnit,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;

pub(super) fn read_windows(
    file: &mut File,
    size: u64,
    budget: u64,
) -> Result<Vec<RawExcerpt>, LightTextError> {
    // Very small windows cannot hold alignment bytes and a complete sentence.
    if size <= budget || budget / 3 < 8 {
        return Ok(vec![read_head(file, size, budget, None)?]);
    }
    let third = budget / 3;
    let gap = size - budget;
    let middle = third + gap / 2;
    let tail = size - third;
    let head_end = if middle == third { tail - gap } else { third };
    let mut windows = vec![read_head(file, size, head_end, None)?];
    for (start, end, source) in [
        (middle, middle + budget - 2 * third, "middle"),
        (tail, size, "tail"),
    ] {
        if start < head_end {
            continue; // Adjacent windows were read together.
        }
        file.seek(SeekFrom::Start(start))
            .map_err(|_| LightTextError::ReadFailed)?;
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact(&mut bytes)
            .map_err(|_| LightTextError::ReadFailed)?;
        // The lookbehind is inside the byte budget. It proves whether a leading
        // continuation belongs to a valid UTF-8 character straddling the boundary.
        let guard = bytes[..4]
            .iter()
            .rposition(|&b| matches!(b, b'\n' | 0x0c))
            .map_or(4, |i| i + 1);
        let body = trim_leading_utf8_fragment(&bytes[guard..], &bytes[..guard]);
        let offset = bytes.len() - body.len();
        let partial = bytes[..guard]
            .iter()
            .take(3)
            .take_while(|&&b| super::is_utf8_continuation(b))
            .count();
        std::str::from_utf8(&bytes[partial..offset]).map_err(|_| LightTextError::DecodeFailed)?;
        let mut text = decode_head(body, end < size)?;
        let mut skip = 0;
        if !matches!(bytes[offset - 1], b'\n' | 0x0c) {
            // A partial table row cannot establish its own columns. Otherwise
            // the first complete sentence boundary also works on a one-line file.
            let line = text.split_inclusive(['\n', '\x0c']).next().unwrap_or("");
            skip = if line.contains(['|', '\t']) {
                line.len()
            } else {
                text_segments(line, true).next().map_or(0, str::len)
            };
            text.drain(..skip);
        }
        windows.push(RawExcerpt {
            text,
            encoding: "utf-8",
            source,
            truncated: true,
            byte_start: start,
            byte_end: end,
            text_byte_start: start + (offset + skip) as u64,
        });
    }
    Ok(windows)
}

struct Piece<'a> {
    text: &'a str,
    chars: u64,
    page: usize,
    margin: bool,
    unit: SourceRangeUnit,
    span: Range<u64>,
    // Only repeated, explicitly labelled page furniture has additional locations.
    repeats: Vec<Range<u64>>,
}

impl Piece<'_> {
    fn ranges(&self) -> impl Iterator<Item = SourceRange> + '_ {
        std::iter::once(&self.span)
            .chain(&self.repeats)
            .map(|s| SourceRange::new(self.unit, s.start, s.end, None))
    }
}

const UNIT_LABELS: &[&str] = &["单位", "金额单位", "数量单位", "unit", "units"];

pub(super) fn label_value<'a>(text: &'a str, labels: &[&str]) -> Option<&'a str> {
    let (key, value) = text.split_once([':', '：'])?;
    labels
        .iter()
        .any(|s| key.trim().eq_ignore_ascii_case(s))
        .then(|| value.trim())
}

fn declares_units(value: &str) -> bool {
    value
        .split(['(', '（', '[', '【', '|', '，', ',', '；', ';'])
        .any(|field| {
            field.split_once([':', '：']).is_some_and(|(key, value)| {
                !value.trim_matches([' ', '）', ')', '】', ']']).is_empty()
                    && key.split_whitespace().next_back().is_some_and(|key| {
                        UNIT_LABELS
                            .iter()
                            .any(|label| label.eq_ignore_ascii_case(key))
                    })
            })
        })
}

struct PageContext<'a> {
    header: Option<&'a str>,
    footer: Option<&'a str>,
    unit_header: Option<usize>,
}

impl PageContext<'_> {
    fn continues(&self, previous: &Self) -> bool {
        self.header
            .filter(|h| !h.is_empty())
            .is_some_and(|current| previous.header.or(previous.footer) == Some(current))
    }
}

struct WindowPieces<'a> {
    lines: Vec<Piece<'a>>,
    pages: Vec<PageContext<'a>>,
    chars_read: Option<u64>,
}

fn pieces(raw: &RawExcerpt, bytes: bool, ordinary: bool) -> WindowPieces<'_> {
    let eof = !raw.truncated || raw.source == "tail";
    let page_breaks = ordinary && raw.text.contains('\x0c');
    let mut pages = raw.text.split(|c| page_breaks && c == '\x0c').peekable();
    let mut seen = HashMap::<(bool, &str), usize>::new();
    let mut output: Vec<Piece<'_>> = Vec::new();
    let mut contexts = Vec::new();
    let (mut previous_byte, mut previous_char) = (0, 0);
    let mut page_start = raw.text_byte_start == 0;
    let mut page_number = 0;
    while let Some(page) = pages.next() {
        let complete_end = pages.peek().is_some() || eof;
        let page_body = page.trim();
        let first = page_body.lines().next().unwrap_or("").trim();
        let last = page_body.lines().next_back().unwrap_or("").trim();
        let mut context = PageContext {
            header: (ordinary && page_start)
                .then(|| label_value(first, &["页眉", "header"]))
                .flatten(),
            footer: (ordinary && complete_end)
                .then(|| label_value(last, &["页脚", "footer"]))
                .flatten(),
            unit_header: None,
        };
        for part in text_segments(page, complete_end) {
            // Tabs delimit fields even when either field is empty.
            let text = if ordinary && part.contains('\t') {
                part.trim_end_matches(['\r', '\n'])
            } else {
                part.trim()
            };
            if text.is_empty() && output.last().is_none_or(|p| p.text.is_empty()) {
                continue;
            }
            let start = text.as_ptr() as usize - raw.text.as_ptr() as usize;
            let end = start + text.len();
            let chars = text.chars().count() as u64;
            let (unit, span) = if bytes {
                let base = raw.text_byte_start;
                (
                    SourceRangeUnit::Byte,
                    base + start as u64..base + end as u64,
                )
            } else {
                previous_char += raw.text[previous_byte..start].chars().count() as u64;
                let span = previous_char..previous_char + chars;
                previous_byte = end;
                previous_char = span.end;
                (SourceRangeUnit::TextChar, span)
            };
            let trimmed = text.trim();
            let header = context.header.is_some() && std::ptr::eq(trimmed, first);
            let footer = context.footer.is_some() && std::ptr::eq(trimmed, last);
            let margin = header || footer;
            if header && context.header.is_some_and(declares_units) {
                // Each table needs the actual unit declaration for its own page.
                context.unit_header = Some(output.len());
            } else if page_breaks && margin {
                let key = (header, text);
                if let Some(previous) = seen.get(&key).map(|&i| &mut output[i]) {
                    if previous.repeats.len() < 127 {
                        previous.repeats.push(span);
                        continue;
                    }
                }
                seen.insert(key, output.len());
            }
            output.push(Piece {
                text,
                chars,
                page: page_number,
                margin,
                unit,
                span,
                repeats: vec![],
            });
        }
        contexts.push(context);
        page_start = true;
        page_number += 1;
    }
    let chars_read =
        (!bytes).then(|| previous_char + raw.text[previous_byte..].chars().count() as u64);
    WindowPieces {
        lines: output,
        pages: contexts,
        chars_read,
    }
}

struct Candidate {
    window: usize,
    page_unit: Option<usize>,
    units: Range<usize>,
    header: Range<usize>,
    rows: Range<usize>,
    kind: ContentUnitKind,
    heading: Option<usize>,
    level: usize,
    cost: u64, // includes one separator
    rank: u8,
}

impl Candidate {
    fn line(window: usize, index: usize, piece: &Piece<'_>) -> Self {
        Self {
            window,
            page_unit: None,
            units: 0..0,
            header: 0..0,
            rows: index..index + 1,
            kind: ContentUnitKind::Text,
            heading: None,
            level: 0,
            cost: piece.chars + 1,
            rank: 0,
        }
    }
}

fn heading_level(text: &str) -> usize {
    let hashes = text.bytes().take_while(|&b| b == b'#').count();
    if hashes != 0 {
        return hashes.min(6);
    }
    let ordinal = text
        .strip_prefix('第')
        .and_then(|s| s.split_once(['章', '节']));
    let numbered = ordinal.is_some_and(|(n, title)| {
        !n.is_empty()
            && !title.trim().is_empty()
            && n.chars()
                .all(|c| c.is_ascii_digit() || "一二三四五六七八九十百零〇".contains(c))
            && !title.contains(['。', '！', '？', '；', '，'])
    });
    usize::from(
        numbered
            || matches!(
                text.trim_end_matches([':', '：']),
                "结论" | "结语" | "审批条件" | "最终结论"
            )
            || (text.starts_with('【') && text.ends_with('】')),
    )
}

fn fact_rank(text: &str) -> u8 {
    if is_attention_fact(text)
        || [
            "结论",
            "综上",
            "仅当",
            "除非",
            "不得",
            "不能",
            "未经",
            "例外",
            "除外",
            "仅限",
            "如果",
            "否则",
            "未通过",
            "尚未",
            "不代表",
            "conclusion",
            "unless",
            "only if",
            "must not",
        ]
        .iter()
        .any(|s| text.contains(s))
    {
        1
    } else {
        4
    }
}

fn table_header(pieces: &[Piece<'_>], at: usize, page: usize) -> Option<(Range<usize>, char)> {
    let line = pieces.get(at).filter(|p| p.page == page && !p.margin)?.text;
    let next = pieces
        .get(at + 1)
        .filter(|p| p.page == page && !p.margin)?
        .text;
    if line.contains('|') && markdown_separator(next) {
        return Some((at..at + 2, '|'));
    }
    // Plain text can have explicitly named tabular columns. A data row in a
    // sampled window cannot establish its own header just by containing tabs.
    if line.contains('\t')
        && next.contains('\t')
        && line.split('\t').any(|cell| {
            let label = cell.split(['（', '(']).next().unwrap_or("").trim();
            [
                "项目", "名称", "指标", "数量", "金额", "单位", "item", "quantity", "amount",
                "unit",
            ]
            .iter()
            .any(|s| label.eq_ignore_ascii_case(s))
        })
    {
        return Some((at..at + 1, '\t'));
    }
    None
}

fn candidates(
    all: &mut Vec<Candidate>,
    pieces: &[Piece<'_>],
    pages: &[PageContext<'_>],
    window: usize,
    sampled: bool,
    budget: u64,
) -> bool {
    let mut bounded = false;
    let mut section = None;
    let mut first = true;
    let mut index = 0;
    let mut units_end = 0;
    let mut leading_rows = sampled && window != 0;
    let mut previous_table: Option<(Range<usize>, Range<usize>, usize)> = None;
    let last_line = pieces.iter().rposition(|p| !p.text.is_empty());
    while index < pieces.len() {
        let text = pieces[index].text;
        if text.trim().is_empty() {
            index += 1;
            continue;
        }
        if index >= units_end {
            units_end = index
                + pieces[index..]
                    .iter()
                    .enumerate()
                    .take_while(|(offset, p)| {
                        p.page == pieces[index].page
                            && (p.text.is_empty()
                                || Some(index + offset) == pages[p.page].unit_header
                                || label_value(p.text, UNIT_LABELS).is_some())
                    })
                    .count();
        }
        let columns = units_end;
        if let Some((header, separator)) = table_header(pieces, columns, pieces[index].page) {
            leading_rows = false;
            let page = &pages[pieces[columns].page];
            let mut units = index + usize::from(Some(index) == page.unit_header)..columns;
            let local_units = pieces[units.clone()].iter().any(|p| !p.text.is_empty());
            let page_unit = page.unit_header.filter(|_| !local_units);
            if !local_units && page_unit.is_none() {
                // Repeated columns alone cannot establish that two projects
                // measure amounts in the same units across a page break.
                if let Some((previous, prev_units, prev_end)) = &previous_table {
                    if pieces[previous.start].page + 1 == pieces[columns].page
                        && page.continues(&pages[pieces[previous.start].page])
                        && previous
                            .clone()
                            .map(|i| pieces[i].text)
                            .eq(header.clone().map(|i| pieces[i].text))
                        && pieces[*prev_end..index]
                            .iter()
                            .all(|p| p.margin || p.text.is_empty())
                    {
                        units = prev_units.clone();
                    }
                }
            }
            let mut row = header.end;
            let end = row
                + pieces[row..]
                    .iter()
                    .take_while(|p| {
                        p.page == pieces[columns].page && !p.margin && p.text.contains(separator)
                    })
                    .count();
            let prefix_rows = page_unit
                .into_iter()
                .chain(units.clone())
                .chain(header.clone())
                .filter(|&i| !pieces[i].text.is_empty());
            let prefix: u64 = prefix_rows.clone().map(|i| pieces[i].chars + 1).sum();
            let prefix_lines = prefix_rows.count();
            if row < end && prefix < budget && prefix_lines < 128 {
                while row < end {
                    let start = row;
                    let rank = fact_rank(pieces[row].text);
                    let mut cost = prefix;
                    while row < end {
                        let next = cost + pieces[row].chars + 1;
                        if row > start
                            && (next > budget.min(512)
                                || prefix_lines + row - start == 128
                                || fact_rank(pieces[row].text) != rank)
                        {
                            break;
                        }
                        cost = next;
                        row += 1;
                    }
                    all.push(Candidate {
                        window,
                        page_unit,
                        units: units.clone(),
                        header: header.clone(),
                        rows: start..row,
                        kind: ContentUnitKind::TableRow,
                        heading: section,
                        level: 0,
                        cost,
                        rank: if first { rank.min(3) } else { rank },
                    });
                    first = false;
                }
                previous_table = Some((header, units, end));
                index = end;
                continue;
            } else if row < end || sampled {
                bounded = true;
                index = end;
                continue;
            }
        }
        if (sampled && text.starts_with('|') && text.ends_with('|'))
            || (leading_rows && text.contains(['|', '\t']))
        {
            // The column header can be in an unread region.
            bounded = true;
            index += 1;
            continue;
        }
        leading_rows = false;
        let level = if Some(index) == last_line && !text.starts_with('#') {
            0
        } else {
            heading_level(text)
        };
        let kind = if level > 0 {
            ContentUnitKind::Heading
        } else {
            ContentUnitKind::Text
        };
        if level > 0 {
            while let Some(i) = section {
                if all[i].level < level {
                    break;
                }
                section = all[i].heading;
            }
        }
        let rank = fact_rank(text);
        all.push(Candidate {
            kind,
            heading: section,
            level,
            rank: if first { rank.min(3) } else { rank },
            ..Candidate::line(window, index, &pieces[index])
        });
        first = kind == ContentUnitKind::Heading;
        if first {
            section = Some(all.len() - 1);
        }
        index += 1;
    }
    bounded
}

/// Emit the already priced slices verbatim. A trailing tab can be the last
/// field of the last row, and is part of both the character and source span.
struct TextOutput {
    text: String,
    max_chars: u64,
    audit: ParsedContentAudit,
    audit_bytes: usize,
}

impl TextOutput {
    fn new(audit: ParsedContentAudit, max_chars: u64) -> Self {
        let audit_bytes = serde_json::to_vec(&audit)
            .expect("serializable audit")
            .len();
        Self {
            text: String::new(),
            max_chars,
            audit,
            audit_bytes,
        }
    }

    fn push(
        &mut self,
        text: &str,
        chars: u64,
        kind: ContentUnitKind,
        ranges: Vec<SourceRange>,
    ) -> bool {
        let units = self.audit.content_units.as_mut().unwrap();
        let separator = usize::from(!units.is_empty());
        let start = units.last().map_or(0, |last| last.text_end + 1);
        let end = start + chars;
        if end > self.max_chars || units.len() >= 512 || ranges.len() > 128 {
            return false;
        }
        let unit = ContentUnit {
            unit_id: format!("u{}", units.len() + 1),
            kind,
            text_start: start,
            text_end: end,
            ranges,
        };
        let old_usable = self.audit.usable_chars.unwrap();
        let usable = old_usable
            + if matches!(
                kind,
                ContentUnitKind::Heading
                    | ContentUnitKind::TableHeader
                    | ContentUnitKind::Unit
                    | ContentUnitKind::Placeholder
            ) {
                0
            } else {
                chars
            };
        // The selector has already charged every character and unit slot. Price
        // just this unit's JSON, its comma and the usable_chars digit change.
        let bytes = self.audit_bytes
            + separator
            + serde_json::to_vec(&unit)
                .expect("serializable content unit")
                .len()
            + usable.to_string().len()
            - old_usable.to_string().len();
        if bytes > 256 * 1024 {
            return false;
        }
        if separator != 0 {
            self.text.push('\n');
        }
        self.text.push_str(text);
        self.audit_bytes = bytes;
        self.audit.usable_chars = Some(usable);
        units.push(unit);
        true
    }
}

pub(super) fn audited_text(
    windows: &[RawExcerpt],
    max_chars: u64,
    log: bool,
    warning: Option<LightTextWarning>,
) -> (String, bool, Option<LightTextWarning>, ParsedContentAudit) {
    let sampled = windows.len() > 1;
    let raw = &windows[0];
    let mut builder = AuditTextBuilder::new(max_chars, "text");
    let mut all = Vec::new();
    let mut parts = Vec::new();
    let mut bounded = raw.truncated;
    let budget = max_chars + 1;
    for (w, window) in windows.iter().enumerate() {
        for range in byte_ranges(window) {
            builder.read(range);
        }
        let ordinary = !log && warning.is_none();
        let WindowPieces {
            lines: pieces,
            pages,
            chars_read,
        } = pieces(window, log || ordinary && raw.truncated, ordinary);
        if let Some(end) = chars_read.filter(|&n| n > 0 && warning.is_none()) {
            // A decoded extent lets later budgets take the complement of exact
            // character ranges without reopening the file or guessing UTF-8 widths.
            builder.read(SourceRange::new(SourceRangeUnit::TextChar, 0, end, None));
        }
        if log {
            for (i, p) in pieces
                .iter()
                .enumerate()
                .filter(|(_, p)| !p.text.is_empty())
            {
                all.push(Candidate::line(w, i, p));
            }
        } else {
            bounded |= candidates(&mut all, &pieces, &pages, w, sampled, budget);
        }
        parts.push(pieces);
    }
    let title = if log {
        "Log tail preview"
    } else {
        "Text preview"
    };
    let source = if sampled {
        "head_middle_tail"
    } else {
        raw.source
    };
    let mut metadata = format!(
        "{title}\nexcerpt_source: {source}\nread_window_truncated: {}",
        raw.truncated
    );
    if let Some(warning) = warning {
        metadata.push_str(&format!("\nwarning: {}", warning.code()));
    }
    let meta_cost = metadata.chars().count() as u64 + 1;
    let reserve_meta = log && meta_cost <= budget;
    let mut remaining = if reserve_meta {
        budget - meta_cost
    } else {
        budget
    };
    let mut slots = 512 - usize::from(reserve_meta);
    let mut kept = vec![true; all.len()];
    let total: u64 = all.iter().map(|c| c.cost).sum();
    if total <= remaining && all.len() <= slots {
        remaining -= total;
        slots -= all.len();
    } else {
        bounded = true;
        kept.fill(false);
        let mut order: Vec<_> = (0..all.len())
            .filter(|&i| all[i].kind != ContentUnitKind::Heading)
            .collect();
        if log {
            order.reverse();
        } else {
            let mut body = order
                .iter()
                .copied()
                .filter(|&i| !parts[all[i].window][all[i].rows.start].margin);
            let first = body.next();
            let last = body.next_back().or(first);
            if let Some(first) = first {
                all[first].rank = 0;
            }
            if let Some(last) = last {
                all[last].rank = 0;
            }
            // Give a representative headed section near the middle an opportunity
            // even when an early section has a long run of ordinary facts.
            if let Some(&i) = order
                .iter()
                .filter(|&&i| all[i].heading.is_some_and(|h| h + 1 == i))
                .min_by_key(|&&i| i.abs_diff(all.len() / 2))
            {
                all[i].rank = 0;
            }
            order.sort_by_key(|&i| (all[i].rank, Some(i) != last));
        }
        for i in order {
            let required =
                std::iter::successors(Some(i), |&i| all[i].heading).take_while(|&i| !kept[i]);
            let cost = required.clone().map(|i| all[i].cost).sum::<u64>();
            let count = required.clone().count();
            if cost <= remaining && count <= slots {
                for i in std::iter::successors(Some(i), |&i| all[i].heading).take(count) {
                    kept[i] = true;
                }
                remaining -= cost;
                slots -= count;
            }
        }
    }
    let (_, mut audit, _) = builder.finish();
    audit.encoding = Some(raw.encoding.into());
    let mut output = TextOutput::new(audit, max_chars);
    if reserve_meta || (meta_cost <= remaining && slots > 0) {
        bounded |= !output.push(&metadata, meta_cost - 1, ContentUnitKind::Heading, vec![]);
    } else {
        bounded = true;
    }
    for (i, c) in all.into_iter().enumerate() {
        if !kept[i] || c.heading.is_some_and(|h| !kept[h]) {
            kept[i] = false;
            continue;
        }
        let pieces = &parts[c.window];
        if c.header.is_empty() {
            let p = &pieces[c.rows.start];
            kept[i] = output.push(p.text, p.chars, c.kind, p.ranges().collect());
        } else {
            let lines = c
                .page_unit
                .into_iter()
                .chain(c.units)
                .chain(c.header)
                .chain(c.rows)
                .map(|i| &pieces[i])
                .filter(|p| !p.text.is_empty());
            let text = lines.clone().map(|p| p.text).collect::<Vec<_>>().join("\n");
            kept[i] = output.push(
                &text,
                c.cost - 1,
                c.kind,
                lines.flat_map(Piece::ranges).collect(),
            );
        }
        bounded |= !kept[i];
    }
    if output.audit.usable_chars == Some(0) {
        output.text.clear();
        output.audit.content_units = Some(Vec::new());
    }
    (output.text, bounded, warning, output.audit)
}
