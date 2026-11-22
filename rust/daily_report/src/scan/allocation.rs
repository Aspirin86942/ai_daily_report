//! Stable first-excerpt opportunities, followed by bounded depth rounds.
use daily_parsers::contract::{
    ContentUnit, ContentUnitKind, ParsedContentAudit, SourceRange, SourceRangeUnit,
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
};

pub const EXCERPT_POLICY_VERSION: &str = "fair_semantic_v10";
const ROUND_TARGET: usize = 512;
const FIRST_TARGET: usize = 256;

pub(super) struct ExcerptInput<'a> {
    pub relative_path: &'a str,
    pub file_identity: &'a str,
    pub extension: &'a str,
    pub content: &'a str,
    pub content_chars: usize,
    pub evidence_audit: Option<&'a ParsedContentAudit>,
    pub size_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AllocatedExcerpt {
    pub text: String,
    pub chars: usize,
    pub blocks: Vec<ExcerptBlock>,
    pub audit: Option<ParsedContentAudit>,
    pub units: Option<Vec<ContentUnit>>,
    pub read_ranges: Option<Vec<SourceRange>>,
    pub ranges: Option<Vec<SourceRange>>,
    pub omitted_ranges: Vec<SourceRange>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ExcerptBlock {
    /// Exclusive end in the mapped content units.
    pub end: usize,
    /// Indices of earlier headings, headers and units inherited by this block.
    pub inherited: Vec<usize>,
}

mod ranges;
pub(super) use ranges::range_gaps;
pub(crate) use ranges::range_spans;
use ranges::{full_excerpt, omitted_text_ranges};

pub(crate) fn ranges_for(units: &[ContentUnit]) -> Vec<SourceRange> {
    let mut ranges = Vec::new();
    for range in units.iter().flat_map(|unit| &unit.ranges) {
        if !ranges.contains(range) {
            ranges.push(range.clone());
        }
    }
    ranges
}

impl AllocatedExcerpt {
    fn finish(mut self, item: &ExcerptInput, truncated: bool) -> Self {
        self.truncated = truncated;
        if let Some(audit) = &mut self.audit {
            self.units = audit.content_units.take();
            self.read_ranges = audit.read_ranges.take();
        }
        self.ranges = self.units.as_deref().map(ranges_for);
        let emitted = self.ranges.as_deref();
        self.omitted_ranges = omitted_text_ranges(item, truncated, emitted);
        if truncated && self.omitted_ranges.is_empty() {
            for range in item
                .evidence_audit
                .iter()
                .flat_map(|audit| audit.content_units.iter().flatten())
                .flat_map(|unit| &unit.ranges)
            {
                if !emitted.unwrap_or_default().contains(range)
                    && !self.omitted_ranges.contains(range)
                {
                    self.omitted_ranges.push(range.clone());
                }
            }
        }
        self
    }
}

struct Piece<'a> {
    source_index: usize,
    text: &'a str,
    chars: usize,
    kind: ContentUnitKind,
    ranges: Cow<'a, [SourceRange]>,
}
impl Piece<'_> {
    fn mapped(&self, number: usize, start: u64) -> ContentUnit {
        ContentUnit {
            unit_id: format!("e{number}"),
            kind: self.kind,
            text_start: start,
            text_end: start + self.chars as u64,
            ranges: self.ranges.to_vec(),
        }
    }
}

fn useful(kind: ContentUnitKind) -> bool {
    !matches!(
        kind,
        ContentUnitKind::Heading
            | ContentUnitKind::TableHeader
            | ContentUnitKind::Unit
            | ContentUnitKind::Placeholder
    )
}

fn splittable_prose(unit: &ContentUnit) -> bool {
    unit.text_end.saturating_sub(unit.text_start) > ROUND_TARGET as u64
        && matches!(
            unit.kind,
            ContentUnitKind::Text | ContentUnitKind::Paragraph
        )
        && unit.ranges.len() <= 1
        && unit.ranges.iter().all(|r| match r.unit {
            SourceRangeUnit::TextChar => {
                r.end.checked_sub(r.start) == Some(unit.text_end - unit.text_start)
            }
            SourceRangeUnit::Paragraph | SourceRangeUnit::PdfPage | SourceRangeUnit::Slide => true,
            _ => false,
        })
}

fn part_ranges(ranges: &[SourceRange], start: u64, end: u64) -> Cow<'_, [SourceRange]> {
    let Some(range) = ranges
        .first()
        .filter(|r| r.unit == SourceRangeUnit::TextChar)
    else {
        return Cow::Borrowed(ranges);
    };
    let mut part = range.clone();
    part.start += start;
    part.end = range.start + end;
    Cow::Owned(vec![part])
}

fn prose_segments(text: &str) -> impl Iterator<Item = (u64, &str)> {
    daily_parsers::text::text_segments(text, true)
        .scan(0, |offset, part| {
            let start = *offset;
            *offset += part.chars().count() as u64;
            Some((start, part))
        })
        .filter(|(_, part)| !part.trim().is_empty())
}

fn pieces<'a>(item: &ExcerptInput<'a>, constrained: bool) -> Vec<Piece<'a>> {
    if item
        .evidence_audit
        .as_ref()
        .is_some_and(|a| a.usable_chars == Some(0))
    {
        return vec![];
    }
    let units = item.evidence_audit.and_then(|a| a.content_units.as_deref());
    let unknown = ContentUnit {
        unit_id: String::new(),
        kind: ContentUnitKind::Text,
        text_start: 0,
        text_end: item.content_chars as u64,
        ranges: Vec::new(),
    };
    let mut output = Vec::new();
    // Both the current parser and cache validate these Unicode offsets before allocation.
    let mut offsets = item
        .content
        .char_indices()
        .map(|(i, _)| i)
        .chain([item.content.len()]);
    let (mut position, mut byte) = (0, offsets.next().expect("offset at zero"));
    for (source_index, unit) in units
        .unwrap_or(std::slice::from_ref(&unknown))
        .iter()
        .enumerate()
    {
        let ranges = units.map_or(&[][..], |units| units[source_index].ranges.as_slice());
        if unit.text_start == unit.text_end {
            continue;
        }
        let start = if unit.text_start == position {
            byte
        } else {
            offsets
                .nth((unit.text_start - position - 1) as usize)
                .expect("valid unit start")
        };
        let end = offsets
            .nth((unit.text_end - unit.text_start - 1) as usize)
            .expect("valid unit end");
        let text = &item.content[start..end];
        (position, byte) = (unit.text_end, end);
        let split = constrained
            && !item.extension.eq_ignore_ascii_case(".csv")
            && splittable_prose(unit)
            && !(matches!(item.extension, ".md" | ".txt") && text.contains('\t'));
        if split {
            for (start, part) in prose_segments(text) {
                let trimmed = part.trim_start();
                let start = start + part[..part.len() - trimmed.len()].chars().count() as u64;
                let text = trimmed.trim_end();
                let chars = text.chars().count();
                output.push(Piece {
                    source_index,
                    text,
                    chars,
                    kind: unit.kind,
                    ranges: part_ranges(ranges, start, start + chars as u64),
                });
            }
        } else {
            output.push(Piece {
                source_index,
                chars: (unit.text_end - unit.text_start) as usize,
                text,
                kind: unit.kind,
                ranges: Cow::Borrowed(ranges),
            });
        }
    }
    output
}

fn priority(piece: &Piece) -> u8 {
    if piece.kind == ContentUnitKind::Total {
        return 0;
    }
    if daily_parsers::text::is_attention_fact(piece.text) {
        return 1;
    }
    // Complete table rows carry the relationship between fields and values.
    // Their priority must not depend on prose keywords in a particular file.
    let numeric = piece.text.chars().any(|c| c.is_ascii_digit());
    if piece.kind == ContentUnitKind::TableRow && numeric {
        return 3;
    }
    const QUALIFIERS: &[&str] = &[
        "不得",
        "除非",
        "仅当",
        "如果",
        "否则",
        "尚未",
        "不能",
        "不代表",
        "不等于",
        "不说明",
        "未经",
        "未确认",
        "待确认",
        "例外",
        "除外",
        "无效",
        "必须",
        "断言",
        "为空",
        "两行",
        "不可",
        "不再",
        "相对",
        "基准",
        "并行",
        "二选一",
        "都要",
        "assert",
        "must not",
        "does not prove",
        "unconfirmed",
    ];
    const MARKERS: &[&str] = &[
        "累计",
        "超过",
        "阈值",
        "统计",
        "生成时间",
        "成立日期",
        "成立时间",
        "社会信用代码",
        "编制单位",
        "报告时间",
        "实测",
        "共",
        "数量",
        "测试",
        "覆盖",
        "节点",
        "档位",
        "金额档",
        "通过",
        "例外",
        "除外",
        "不得",
        "threshold",
        ">=",
        "<=",
        "≥",
        "≤",
        "subtotal",
        "total",
        "count",
        "passed",
    ];
    if numeric && MARKERS.iter().any(|marker| piece.text.contains(marker)) {
        return 3;
    }
    if QUALIFIERS.iter().any(|word| piece.text.contains(word)) {
        return 4;
    }
    if piece.kind == ContentUnitKind::TableRow || numeric {
        5
    } else {
        6
    }
}

struct Block<'a> {
    context: Vec<&'a Piece<'a>>,
    facts: Vec<&'a Piece<'a>>,
    rank: u8,
}

impl Block<'_> {
    fn minimum_chars(&self) -> usize {
        let context = if self.context.first().is_some_and(|p| preview_heading(p)) {
            &self.context[1..]
        } else {
            &self.context
        };
        self.facts.iter().map(|piece| piece.chars).min().unwrap() + charged(context)
    }
}

fn bundles<'a>(
    item: &ExcerptInput,
    pieces: &'a [Piece<'a>],
    constrained: bool,
) -> VecDeque<Block<'a>> {
    let last_fact = pieces.iter().rposition(|p| useful(p.kind));
    let ordinary = matches!(item.extension, ".txt" | ".md");
    let first_fact = pieces.iter().position(|p| useful(p.kind));
    let middle_section = pieces
        .windows(2)
        .enumerate()
        .filter(|(_, pair)| {
            pair[0].kind == ContentUnitKind::Heading
                && !pair[0].ranges.is_empty()
                && useful(pair[1].kind)
        })
        .map(|(i, _)| i + 1)
        .min_by_key(|&i| i.abs_diff(pieces.len() / 2));
    let windows = item
        .evidence_audit
        .as_ref()
        .and_then(|a| a.read_ranges.as_deref())
        .unwrap_or_default();
    let sampled = ordinary && windows.iter().any(|r| r.label.as_deref() == Some("tail"));
    let mut previous_window = None;
    let mut blocks: Vec<Block> = Vec::new();
    let mut context: Vec<&Piece> = Vec::new();
    let mut size = 0;
    let mut new_context = true;
    let mut section_rank = 6;
    let mut new_headers = false;
    let mut new_units = false;
    for (i, piece) in pieces.iter().enumerate() {
        if sampled {
            if let Some(range) = piece.ranges.first() {
                let window = windows.iter().position(|r| {
                    r.unit == SourceRangeUnit::Byte
                        && matches!(r.label.as_deref(), Some("head" | "middle" | "tail"))
                        && r.start <= range.start
                        && range.end <= r.end
                });
                if window != previous_window {
                    context.clear();
                    section_rank = 6;
                    new_context = true;
                    new_units = false;
                    new_headers = false;
                    previous_window = window;
                }
            }
        }
        if !useful(piece.kind) {
            if piece.kind == ContentUnitKind::Placeholder {
                continue;
            }
            if piece.kind == ContentUnitKind::Heading {
                let level = |p: &Piece| {
                    p.text
                        .bytes()
                        .take_while(|&b| b == b'#')
                        .count()
                        .clamp(1, 6)
                };
                context.retain(|p| {
                    ordinary
                        && p.kind == ContentUnitKind::Heading
                        && !preview_heading(p)
                        && level(p) < level(piece)
                });
                new_headers = false;
                new_units = false;
                // A qualification in a section heading applies to its facts.
                section_rank = if priority(piece) == 4 || context.iter().any(|p| priority(p) == 4) {
                    4
                } else {
                    6
                };
            } else if !new_headers && piece.kind == ContentUnitKind::TableHeader {
                context.retain(|p| {
                    p.kind == ContentUnitKind::Heading
                        || (p.kind == ContentUnitKind::Unit && new_units)
                });
                new_headers = true;
            } else if !new_units && piece.kind == ContentUnitKind::Unit {
                context.retain(|p| p.kind != ContentUnitKind::Unit);
                new_units = true;
            }
            context.push(piece);
            new_context = true;
            continue;
        }
        let table = matches!(
            piece.kind,
            ContentUnitKind::TableRow | ContentUnitKind::Total
        );
        if !table && context.iter().any(|p| p.kind != ContentUnitKind::Heading) {
            context.retain(|p| p.kind == ContentUnitKind::Heading);
            new_context = true;
        }
        new_headers = false;
        new_units = false;
        let mut rank = priority(piece).min(section_rank);
        if constrained
            && (Some(i) == last_fact
                || ordinary && (Some(i) == first_fact || Some(i) == middle_section))
        {
            rank = rank.min(2);
        }
        let cost = piece.chars + 1;
        let target = if blocks.len() < 2 {
            FIRST_TARGET
        } else {
            ROUND_TARGET
        };
        // Text-parser table groups already include their own exact unit and columns.
        // Keep each independently citable when a following page changes the scale.
        let plain_table = ordinary && piece.kind == ContentUnitKind::TableRow;
        if new_context
            || plain_table
            || size + cost > target
            || blocks.last().is_none_or(|block| block.rank != rank)
        {
            size = charged(&context);
            blocks.push(Block {
                context: context.clone(),
                facts: vec![],
                rank,
            });
        }
        new_context = plain_table;
        size += cost;
        blocks.last_mut().unwrap().facts.push(piece);
    }
    if !constrained {
        return blocks.into();
    }
    blocks.sort_by_key(|block| block.rank);
    let depth = blocks.split_off(blocks.partition_point(|b| b.rank <= 4));
    let mut qualifiers: VecDeque<_> = blocks
        .split_off(blocks.partition_point(|b| b.rank < 4))
        .into();
    let mut quantitative: VecDeque<_> = blocks
        .split_off(blocks.partition_point(|b| b.rank < 3))
        .into();
    while !quantitative.is_empty() || !qualifiers.is_empty() {
        blocks.extend(quantitative.pop_front());
        blocks.extend(qualifiers.pop_front());
    }
    blocks.extend(depth);
    blocks.into()
}

fn preview_heading(piece: &Piece) -> bool {
    piece.kind == ContentUnitKind::Heading
        && piece.ranges.is_empty()
        && ["CSV preview\n", "Text preview\n", "Log tail preview\n"]
            .iter()
            .any(|prefix| piece.text.starts_with(prefix))
}

fn charged(pieces: &[&Piece]) -> usize {
    pieces.iter().map(|p| p.chars + 1).sum()
}

fn empty_audit(original: &ParsedContentAudit) -> ParsedContentAudit {
    ParsedContentAudit {
        version: original.version,
        archive: original.archive,
        encoding: original.encoding.clone(),
        detected_format: original.detected_format.clone(),
        read_ranges: original.read_ranges.clone(),
        content_units: original.content_units.as_ref().map(|_| Vec::new()),
        usable_chars: Some(0),
    }
}

struct Selection {
    excerpt: AllocatedExcerpt,
    context: Vec<(usize, usize)>,
    unit_count: usize,
    audit_bytes: Option<usize>,
    omitted: bool,
}

impl Selection {
    fn new(item: &ExcerptInput) -> Self {
        let audit = item.evidence_audit.map(empty_audit);
        let audit_bytes = audit.as_ref().and_then(|audit| {
            audit.content_units.as_ref()?;
            Some(serde_json::to_vec(audit).expect("serializable audit").len())
        });
        Self {
            excerpt: AllocatedExcerpt {
                audit,
                ..Default::default()
            },
            context: Vec::new(),
            unit_count: 0,
            audit_bytes,
            omitted: false,
        }
    }

    // Repeated ranges after splitting or reattaching context count toward the wire limit.
    fn append(&mut self, context: &[&Piece], facts: &[&Piece], reuse: usize, cost: usize) -> bool {
        let new_context = &context[reuse..];
        let pieces = || new_context.iter().chain(facts);
        if let Some(audit) = &mut self.excerpt.audit {
            let previous = audit.usable_chars.unwrap_or(0);
            let usable = previous + facts.iter().map(|p| p.chars as u64).sum::<u64>();
            if let Some(mut bytes) = self.audit_bytes {
                bytes += usable.to_string().len() - previous.to_string().len();
                let mut through = self.excerpt.chars as u64;
                let mut mapped = Vec::with_capacity(new_context.len() + facts.len());
                for (offset, piece) in pieces().enumerate() {
                    let index = self.unit_count + offset;
                    let separator = usize::from(index != 0);
                    let unit = piece.mapped(index + 1, through + separator as u64);
                    bytes +=
                        separator + serde_json::to_vec(&unit).expect("serializable unit").len();
                    if bytes > 256 * 1024 {
                        return false;
                    }
                    through = unit.text_end;
                    mapped.push(unit);
                }
                audit.content_units.as_mut().unwrap().extend(mapped);
                self.audit_bytes = Some(bytes);
            }
            audit.usable_chars = Some(usable);
        }

        for piece in pieces() {
            if !self.excerpt.text.is_empty() {
                self.excerpt.text.push('\n');
            }
            self.excerpt.text.push_str(piece.text);
        }
        self.excerpt.chars += cost;
        self.context.truncate(reuse);
        if self.audit_bytes.is_some() {
            self.excerpt.blocks.push(ExcerptBlock {
                end: self.unit_count + new_context.len() + facts.len(),
                inherited: self.context.iter().map(|&(_, index)| index).collect(),
            });
        }
        self.context.extend(
            new_context
                .iter()
                .enumerate()
                .map(|(offset, piece)| (piece.source_index, self.unit_count + offset)),
        );
        self.unit_count += new_context.len() + facts.len();
        true
    }

    fn common_context(&self, context: &[&Piece]) -> usize {
        self.context
            .iter()
            .zip(context)
            .take_while(|((index, _), piece)| *index == piece.source_index)
            .count()
    }

    // A rejected first-round fact can compete again in the unrestricted rounds.
    fn take(
        &mut self,
        pending: &mut VecDeque<Block<'_>>,
        budget: usize,
        defer: bool,
    ) -> Option<usize> {
        let mut skipped = VecDeque::new();
        let mut used = None;
        while let Some(block) = pending.pop_front() {
            let facts_cost = charged(&block.facts);
            let mut context = block.context.as_slice();
            let mut reuse = self.common_context(context);
            let first = usize::from(self.unit_count == 0);
            let fits = |context: &[&Piece], reuse: usize| {
                facts_cost + charged(&context[reuse..]) <= budget.saturating_add(first)
                    && self.unit_count + block.facts.len() + context.len() - reuse <= 512
            };
            if !fits(context, reuse) && context.first().is_some_and(|p| preview_heading(p)) {
                context = &context[1..];
                reuse = self.common_context(context);
            }
            let cost = facts_cost + charged(&context[reuse..]) - first;
            if fits(context, reuse) && self.append(context, &block.facts, reuse, cost) {
                self.omitted |= context.len() < block.context.len();
                used = Some(cost);
                break;
            }
            if block.facts.len() > 1 {
                for fact in block.facts.into_iter().rev() {
                    pending.push_front(Block {
                        context: block.context.clone(),
                        facts: vec![fact],
                        rank: block.rank,
                    });
                }
            } else if defer {
                skipped.push_back(block);
            } else {
                self.omitted = true;
            }
        }
        skipped.append(pending);
        *pending = skipped;
        used
    }
}

pub(super) fn allocate_excerpts(
    evidence: &[ExcerptInput],
    budget: usize,
    priority_paths: &[String],
) -> Vec<AllocatedExcerpt> {
    if evidence
        .iter()
        .map(|item| item.content_chars)
        .sum::<usize>()
        <= budget
    {
        return evidence.iter().map(full_excerpt).collect();
    }
    let mut groups: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
    let parts: Vec<_> = evidence.iter().map(|item| pieces(item, true)).collect();
    let mut allocations: Vec<_> = evidence
        .iter()
        .zip(&parts)
        .enumerate()
        .map(|(index, (item, parts))| {
            let pending = bundles(item, parts, true);
            if let Some(minimum) = pending.iter().map(Block::minimum_chars).min() {
                groups
                    .entry(item.extension)
                    .or_default()
                    .push((index, minimum));
            }
            (pending, Selection::new(item))
        })
        .collect();
    for group in groups.values_mut() {
        group.sort_by_key(|&(i, _)| (&evidence[i].relative_path, &evidence[i].file_identity));
    }
    let order: Vec<_> = (0..groups.values().map(Vec::len).max().unwrap_or(0))
        .flat_map(|round| {
            groups
                .values()
                .filter_map(move |group| group.get(round))
                .copied()
        })
        .collect();

    // Reserve one complete fact per feasible file, then give each file a fair share.
    let mut first = order.clone();
    let mut reserved: usize = first.iter().map(|&(_, size)| size).sum();
    if reserved > budget {
        first.sort_by_key(|&(_, size)| size);
        reserved = 0;
        first.retain(|&(_, size)| {
            if size > budget - reserved {
                return false;
            }
            reserved += size;
            true
        });
    }
    let mut remaining = budget;
    let fair_share = budget.checked_div(first.len()).unwrap_or(0);
    for (index, minimum) in first {
        reserved -= minimum;
        let allowance = (remaining - reserved).min(fair_share.max(minimum));
        let (pending, selection) = &mut allocations[index];
        remaining -= selection.take(pending, allowance, true).unwrap_or(0);
    }

    let preferred: Vec<_> = order
        .iter()
        .copied()
        .filter(|&(i, _)| {
            priority_paths
                .binary_search_by(|path| path.as_str().cmp(evidence[i].relative_path))
                .is_ok()
        })
        .collect();
    for group in [&preferred, &order] {
        while remaining != 0 {
            let mut active: Vec<_> = group
                .iter()
                .filter_map(|&(i, _)| Some((allocations[i].0.front()?.rank, i)))
                .collect();
            if active.is_empty() {
                break;
            }
            active.sort_by_key(|&(rank, _)| rank);
            for (_, i) in active {
                let (pending, selection) = &mut allocations[i];
                remaining -= selection.take(pending, remaining, false).unwrap_or(0);
            }
        }
    }
    allocations
        .into_iter()
        .zip(evidence)
        .map(|((pending, selected), item)| {
            let truncated = selected.omitted
                || !pending.is_empty()
                || selected.excerpt.chars < item.content_chars;
            selected.excerpt.finish(item, truncated)
        })
        .collect()
}
