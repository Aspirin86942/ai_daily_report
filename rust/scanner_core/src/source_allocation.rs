//! Stable first-excerpt opportunities, followed by bounded depth rounds.
use crate::decision::ContextFileEvidence;
use ai_daily_worker_contract::{
    ContentUnit, ContentUnitKind, ParsedContentAudit, SourceRange, SourceRangeUnit,
};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::rc::Rc;

pub const EXCERPT_POLICY_VERSION: &str = "fair_semantic_v9";
const ROUND_TARGET: usize = 512;
const FIRST_TARGET: usize = 256;

pub struct AllocatedExcerpt {
    pub text: String,
    /// Exclusive ends in the mapped content units, one per emitted complete block.
    pub block_ends: Vec<usize>,
    /// Earlier heading, header and unit indices reused by each corresponding block.
    pub block_context: Vec<Vec<usize>>,
    pub audit: Option<ParsedContentAudit>,
    pub ranges: Option<Vec<SourceRange>>,
    pub truncated: bool,
}

#[derive(Clone)]
struct Piece {
    source_index: usize,
    text: Rc<str>,
    chars: usize,
    kind: ContentUnitKind,
    ranges: Vec<SourceRange>,
}
impl Piece {
    fn mapped(&self, number: usize, start: u64) -> ContentUnit {
        ContentUnit {
            unit_id: format!("e{number}"),
            kind: self.kind,
            text_start: start,
            text_end: start + self.chars as u64,
            ranges: self.ranges.clone(),
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

fn part_ranges(unit: &ContentUnit, start: u64, end: u64) -> Vec<SourceRange> {
    let mut ranges = unit.ranges.clone();
    if let Some(range) = ranges
        .first_mut()
        .filter(|r| r.unit == SourceRangeUnit::TextChar)
    {
        range.end = range.start + end;
        range.start += start;
    }
    ranges
}

fn pieces(item: &ContextFileEvidence, constrained: bool) -> Vec<Piece> {
    if item
        .evidence_audit
        .as_ref()
        .is_some_and(|a| a.usable_chars == Some(0))
    {
        return vec![];
    }
    let chars: Vec<char> = item.content.chars().collect();
    let units = item
        .evidence_audit
        .as_ref()
        .and_then(|a| a.content_units.clone())
        .unwrap_or_else(|| {
            if chars.is_empty() {
                vec![]
            } else {
                vec![ContentUnit {
                    unit_id: "unknown".into(),
                    kind: ContentUnitKind::Text,
                    text_start: 0,
                    text_end: chars.len() as u64,
                    ranges: vec![],
                }]
            }
        });
    let mut output = Vec::new();
    for (source_index, unit) in units.into_iter().enumerate() {
        if unit.text_end as usize > chars.len() || unit.text_start >= unit.text_end {
            continue;
        }
        let text: String = chars[unit.text_start as usize..unit.text_end as usize]
            .iter()
            .collect();
        let split =
            constrained && !item.extension.eq_ignore_ascii_case(".csv") && splittable_prose(&unit);
        if split {
            let mut offset = 0;
            for part in ai_daily_text_parser::text_segments(&text, true) {
                let trimmed = part.trim();
                if !trimmed.is_empty() {
                    let chars = trimmed.chars().count();
                    let leading = part.chars().take_while(|c| c.is_whitespace()).count() as u64;
                    let start = offset + leading;
                    output.push(Piece {
                        source_index,
                        text: trimmed.into(),
                        chars,
                        kind: unit.kind,
                        ranges: part_ranges(&unit, start, start + chars as u64),
                    });
                }
                offset += part.chars().count() as u64;
            }
        } else {
            output.push(Piece {
                source_index,
                chars: (unit.text_end - unit.text_start) as usize,
                text: text.into(),
                kind: unit.kind,
                ranges: unit.ranges,
            });
        }
    }
    output
}

fn priority(piece: &Piece) -> u8 {
    if piece.kind == ContentUnitKind::Total {
        return 0;
    }
    if ai_daily_text_parser::is_attention_fact(&piece.text) {
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
        "exception",
    ];
    const MARKERS: &[&str] = &[
        "#REF!",
        "#VALUE!",
        "#DIV/0!",
        "累计",
        "超过",
        "阈值",
        "合计",
        "总计",
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
        "失败",
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
        "failed",
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

struct Block {
    context: Vec<Piece>,
    facts: Vec<Piece>,
    rank: u8,
}

fn bundles(item: &ContextFileEvidence, constrained: bool) -> VecDeque<Block> {
    let pieces = pieces(item, constrained);
    let last_fact = pieces.iter().rposition(|p| useful(p.kind));
    let mut blocks: Vec<Block> = Vec::new();
    let mut context: Vec<Piece> = Vec::new();
    let mut size = 0;
    let mut new_context = true;
    let mut section_rank = 6;
    let mut new_headers = false;
    let mut new_units = false;
    for (i, piece) in pieces.into_iter().enumerate() {
        if !useful(piece.kind) {
            if piece.kind == ContentUnitKind::Placeholder {
                continue;
            }
            if piece.kind == ContentUnitKind::Heading {
                context.clear();
                new_headers = false;
                new_units = false;
                // A qualification in a section heading applies to its facts.
                section_rank = if priority(&piece) == 4 { 4 } else { 6 };
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
        let mut rank = priority(&piece).min(section_rank);
        if constrained && Some(i) == last_fact {
            rank = rank.min(2);
        }
        let cost = piece.chars + 1;
        let target = if blocks.len() < 2 {
            FIRST_TARGET
        } else {
            ROUND_TARGET
        };
        if new_context
            || size + cost > target
            || blocks.last().is_none_or(|block| block.rank != rank)
        {
            size = charged(&context);
            blocks.push(Block {
                context: context.clone(),
                facts: vec![],
                rank,
            });
            new_context = false;
        }
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

#[derive(Default)]
struct Selection {
    pieces: Vec<Piece>,
    context: Vec<usize>,
    ends: Vec<usize>,
    reused: Vec<Vec<usize>>,
    omitted: bool,
    audit_bytes: Option<usize>,
    chars: usize,
    usable: usize,
}

fn preview_heading(piece: &Piece) -> bool {
    piece.kind == ContentUnitKind::Heading
        && piece.ranges.is_empty()
        && ["CSV preview\n", "Text preview\n", "Log tail preview\n"]
            .iter()
            .any(|prefix| piece.text.starts_with(prefix))
}

fn charged(pieces: &[Piece]) -> usize {
    pieces.iter().map(|p| p.chars + 1).sum()
}

impl Selection {
    fn new(item: &ContextFileEvidence) -> Self {
        let audit_bytes = item.evidence_audit.as_ref().and_then(|original| {
            original.content_units.as_ref()?;
            let audit = ParsedContentAudit {
                content_units: Some(vec![]),
                usable_chars: Some(0),
                ..original.clone()
            };
            Some(
                serde_json::to_vec(&audit)
                    .expect("serializable audit")
                    .len(),
            )
        });
        Self {
            audit_bytes,
            ..Self::default()
        }
    }

    // Repeated source ranges after sentence splitting or reattaching a header can
    // reach the wire limit even when the original parser audit fit.
    fn charge_audit(&mut self, context: &[Piece], facts: &[Piece]) -> bool {
        let Some(mut bytes) = self.audit_bytes else {
            return true;
        };
        let usable = self.usable + facts.iter().map(|p| p.chars).sum::<usize>();
        bytes += usable.to_string().len() - self.usable.to_string().len();
        let mut chars = self.chars as u64;
        for (offset, piece) in context.iter().chain(facts).enumerate() {
            let index = self.pieces.len() + offset;
            let separator = usize::from(index != 0);
            let unit = piece.mapped(index + 1, chars + separator as u64);
            bytes += separator
                + serde_json::to_vec(&unit)
                    .expect("serializable content unit")
                    .len();
            if bytes > 256 * 1024 {
                return false;
            }
            chars = unit.text_end;
        }
        self.audit_bytes = Some(bytes);
        self.usable = usable;
        true
    }

    fn common_context(&self, context: &[Piece]) -> usize {
        self.context
            .iter()
            .zip(context)
            .take_while(|(i, p)| self.pieces[**i].source_index == p.source_index)
            .count()
    }

    // Unaffordable atomic facts never block later candidates. A first-round reservation
    // is temporary, so defer its rejected facts for a later unrestricted round.
    fn take(&mut self, pending: &mut VecDeque<Block>, budget: usize, defer: bool) -> Option<usize> {
        let mut skipped = VecDeque::new();
        let mut used = None;
        while let Some(mut block) = pending.pop_front() {
            let facts_cost = charged(&block.facts);
            let mut context = block.context.as_slice();
            let mut reuse = self.common_context(context);
            let first = usize::from(self.pieces.is_empty());
            let fits = |context: &[Piece], reuse: usize| {
                facts_cost + charged(&context[reuse..]) <= budget.saturating_add(first)
                    && self.pieces.len() + block.facts.len() + context.len() - reuse <= 512
            };
            if !fits(context, reuse) && context.first().is_some_and(preview_heading) {
                context = &context[1..];
                reuse = self.common_context(context);
            }
            if fits(context, reuse) && self.charge_audit(&context[reuse..], &block.facts) {
                let cost = facts_cost + charged(&context[reuse..]) - first;
                self.chars += cost;
                self.omitted |= context.len() < block.context.len();
                self.context.truncate(reuse);
                self.reused.push(self.context.clone());
                let start = self.pieces.len();
                self.pieces.extend_from_slice(&context[reuse..]);
                self.context.extend(start..self.pieces.len());
                self.pieces.append(&mut block.facts);
                self.ends.push(self.pieces.len());
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

// Add model-selectable boundaries to the audit, without rendering or editing the
// frozen text. The index map also relocates structural context and trailing units.
fn full_unit_starts(text: &str, audit: &mut ParsedContentAudit, csv: bool) -> Vec<usize> {
    let units = audit.content_units.as_deref().unwrap_or_default();
    let mut slots = 512usize.saturating_sub(units.len());
    let mut long = units.iter().filter(|u| splittable_prose(u)).count();
    if csv || slots == 0 || long == 0 {
        return (0..=units.len()).collect();
    }
    let mut bytes = (256usize * 1024)
        .saturating_sub(serde_json::to_vec(audit).expect("serializable audit").len());
    let offsets: Vec<_> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let ids: HashSet<_> = units.iter().map(|u| u.unit_id.as_str()).collect();
    let mut next_id = 0;
    let mut mapped = Vec::with_capacity(units.len());
    let mut starts = Vec::with_capacity(units.len() + 1);
    for unit in units {
        starts.push(mapped.len());
        if !splittable_prose(unit) {
            mapped.push(unit.clone());
            continue;
        }
        let share = slots.div_ceil(long);
        long -= 1;
        if share == 0 || unit.text_end as usize >= offsets.len() {
            mapped.push(unit.clone());
            continue;
        }
        // A new unit's offsets are at most the original ends; this reserves enough
        // serialized bytes even when the original paragraph has a long range label.
        let len = unit.text_end - unit.text_start;
        let upper = ContentUnit {
            unit_id: "s512".into(),
            text_start: unit.text_end,
            text_end: unit.text_end,
            kind: unit.kind,
            ranges: part_ranges(unit, len, len),
        };
        let cost = serde_json::to_vec(&upper)
            .expect("serializable content unit")
            .len()
            + 1;
        let extra = share.min(bytes / cost);
        if extra == 0 {
            mapped.push(unit.clone());
            continue;
        }
        let original = &text[offsets[unit.text_start as usize]..offsets[unit.text_end as usize]];
        let mut cuts = Vec::new();
        let mut offset = 0;
        let mut seen = false;
        for part in ai_daily_text_parser::text_segments(original, true) {
            if !part.trim().is_empty() {
                if seen {
                    cuts.push(offset);
                }
                seen = true;
            }
            offset += part.chars().count() as u64;
        }
        // When there are more sentences than audit slots, adjacent sentences stay
        // together. Every original character, including whitespace, belongs to one span.
        let group = (cuts.len() + 1).div_ceil(extra + 1);
        let mut start = 0;
        for end in cuts.into_iter().skip(group - 1).step_by(group).chain([len]) {
            let unit_id = if start == 0 {
                unit.unit_id.clone()
            } else {
                slots -= 1;
                bytes -= cost;
                loop {
                    next_id += 1;
                    let id = format!("s{next_id}");
                    if !ids.contains(id.as_str()) {
                        break id;
                    }
                }
            };
            mapped.push(ContentUnit {
                unit_id,
                kind: unit.kind,
                text_start: unit.text_start + start,
                text_end: unit.text_start + end,
                ranges: part_ranges(unit, start, end),
            });
            start = end;
        }
    }
    starts.push(mapped.len());
    audit.content_units = Some(mapped);
    starts
}

fn full_excerpt(item: &ContextFileEvidence) -> AllocatedExcerpt {
    let blocks = bundles(item, false);
    let mut audit = item.evidence_audit.clone();
    let known = audit.as_ref().is_some_and(|a| a.content_units.is_some());
    let mut ranges = known.then(Vec::new);
    let mut block_ends = Vec::new();
    let mut block_context = Vec::new();
    let text = if blocks.is_empty() {
        if let Some(a) = &mut audit {
            if let Some(units) = &mut a.content_units {
                units.clear();
            }
            a.usable_chars = Some(0);
        }
        String::new()
    } else {
        if let Some(audit) = audit.as_mut().filter(|a| a.content_units.is_some()) {
            let starts = full_unit_starts(
                &item.content,
                audit,
                item.extension.eq_ignore_ascii_case(".csv"),
            );
            let units = audit.content_units.as_deref().unwrap();
            let mut begin = 0;
            for block in blocks {
                // An oversized fact already occupies its own native block. Its
                // sentence slices can now be selected separately by a narrower model.
                let last = block.facts.last().unwrap().source_index;
                for end in starts[last] + 1..=starts[last + 1] {
                    block_context.push(
                        block
                            .context
                            .iter()
                            .map(|p| starts[p.source_index])
                            .filter(|&i| i < begin)
                            .collect(),
                    );
                    block_ends.push(end);
                    begin = end;
                }
            }
            // Trailing structural units or placeholders belong to the final block.
            *block_ends.last_mut().unwrap() = units.len();
            let emitted = ranges.as_mut().unwrap();
            for range in units.iter().flat_map(|u| &u.ranges) {
                if !emitted.contains(range) {
                    emitted.push(range.clone());
                }
            }
        }
        item.content.clone()
    };
    AllocatedExcerpt {
        truncated: text.len() < item.content.len(),
        text,
        audit,
        ranges,
        block_ends,
        block_context,
    }
}

pub fn allocate_excerpts(
    evidence: &[&ContextFileEvidence],
    budget: usize,
) -> Vec<AllocatedExcerpt> {
    allocate_excerpts_with_priority(evidence, budget, &[])
}

pub fn allocate_excerpts_with_priority(
    evidence: &[&ContextFileEvidence],
    budget: usize,
    priority_paths: &[String],
) -> Vec<AllocatedExcerpt> {
    let total = evidence
        .iter()
        .map(|item| item.content.chars().count())
        .sum::<usize>();
    if total <= budget {
        return evidence.iter().map(|item| full_excerpt(item)).collect();
    }
    let mut units: Vec<_> = evidence.iter().map(|item| bundles(item, true)).collect();
    let minimum: Vec<_> = units
        .iter()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    let context = if b.context.first().is_some_and(preview_heading) {
                        &b.context[1..]
                    } else {
                        &b.context
                    };
                    b.facts
                        .iter()
                        .map(|fact| fact.chars)
                        .min()
                        .map(|n| n + charged(context))
                })
                .min()
        })
        .collect();
    let mut selected: Vec<_> = evidence.iter().map(|item| Selection::new(item)).collect();
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, item) in evidence.iter().enumerate() {
        if minimum[i].is_some() {
            groups.entry(&item.extension).or_default().push(i);
        }
    }
    for group in groups.values_mut() {
        group.sort_by_key(|&i| (&evidence[i].relative_path, &evidence[i].file_identity));
    }
    let mut order = Vec::new();
    for round in 0..groups.values().map(Vec::len).max().unwrap_or(0) {
        order.extend(
            groups
                .values()
                .filter_map(|group| group.get(round))
                .copied(),
        );
    }

    // Reserve enough for one complete fact and its required context from each
    // feasible file. If even the minima exceed the cap, cover as many sources as fit.
    let mut first = order.clone();
    let mut reserved: usize = first.iter().map(|&i| minimum[i].unwrap()).sum();
    if reserved > budget {
        first.sort_by_key(|&i| minimum[i]);
        reserved = 0;
        first.retain(|&i| {
            let size = minimum[i].unwrap();
            if size > budget - reserved {
                return false;
            }
            reserved += size;
            true
        });
    }
    let mut remaining = budget;
    let fair_share = budget.checked_div(first.len()).unwrap_or(0);
    for i in first {
        let minimum = minimum[i].unwrap();
        reserved -= minimum;
        let available = remaining - reserved;
        let allowance = available.min(fair_share.max(minimum));
        if let Some(cost) = selected[i].take(&mut units[i], allowance, true) {
            remaining -= cost;
        }
    }
    let is_priority: Vec<_> = evidence
        .iter()
        .map(|item| {
            priority_paths
                .binary_search(&item.relative_path.replace('\\', "/"))
                .is_ok()
        })
        .collect();
    for priority_only in [true, false] {
        loop {
            let mut active: Vec<_> = order
                .iter()
                .copied()
                .filter(|&i| !units[i].is_empty() && (!priority_only || is_priority[i]))
                .collect();
            if active.is_empty() || remaining == 0 {
                break;
            }
            active.sort_by_key(|&i| units[i].front().unwrap().rank);
            for i in active {
                if let Some(cost) = selected[i].take(&mut units[i], remaining, false) {
                    remaining -= cost;
                }
            }
        }
    }
    evidence
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let mut text = String::new();
            let mut chars = 0;
            let mut mapped = Vec::new();
            let mut ranges = Vec::new();
            let mut usable = 0;
            for piece in &selected[index].pieces {
                if !text.is_empty() {
                    text.push('\n');
                    chars += 1;
                }
                let start = chars;
                text.push_str(&piece.text);
                chars += piece.chars as u64;
                if useful(piece.kind) {
                    usable += chars - start;
                }
                for range in &piece.ranges {
                    if !ranges.contains(range) {
                        ranges.push(range.clone());
                    }
                }
                mapped.push(piece.mapped(mapped.len() + 1, start));
            }
            let audit = item.evidence_audit.clone().map(|mut audit| {
                if audit.content_units.is_some() {
                    audit.content_units = Some(mapped);
                }
                audit.usable_chars = Some(usable);
                audit
            });
            let known = item
                .evidence_audit
                .as_ref()
                .is_some_and(|a| a.content_units.is_some());
            AllocatedExcerpt {
                block_ends: if known {
                    std::mem::take(&mut selected[index].ends)
                } else {
                    vec![]
                },
                block_context: if known {
                    std::mem::take(&mut selected[index].reused)
                } else {
                    vec![]
                },
                truncated: selected[index].omitted
                    || !units[index].is_empty()
                    || text.chars().count() < item.content.chars().count(),
                text,
                audit,
                ranges: known.then_some(ranges),
            }
        })
        .collect()
}
