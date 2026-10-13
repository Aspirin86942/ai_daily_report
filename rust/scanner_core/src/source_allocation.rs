//! Stable first-excerpt opportunities, followed by bounded depth rounds.
use crate::decision::ContextFileEvidence;
use ai_daily_worker_contract::{
    ContentUnit, ContentUnitKind, ParsedContentAudit, SourceRange, SourceRangeUnit,
};
use std::collections::{BTreeMap, VecDeque};

pub const EXCERPT_POLICY_VERSION: &str = "fair_semantic_v9";
const ROUND_TARGET: usize = 512;
const FIRST_TARGET: usize = 256;

pub struct AllocatedExcerpt {
    pub text: String,
    pub audit: Option<ParsedContentAudit>,
    pub ranges: Option<Vec<SourceRange>>,
    pub truncated: bool,
}

#[derive(Clone, PartialEq)]
struct Piece {
    text: String,
    kind: ContentUnitKind,
    ranges: Vec<SourceRange>,
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

fn pieces(item: &ContextFileEvidence) -> Vec<Piece> {
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
    for unit in units {
        if unit.text_end as usize > chars.len() || unit.text_start >= unit.text_end {
            continue;
        }
        let text: String = chars[unit.text_start as usize..unit.text_end as usize]
            .iter()
            .collect();
        // Rows stay atomic. Text may split only at a complete line or sentence.
        if text.chars().count() > ROUND_TARGET
            && useful(unit.kind)
            && !matches!(
                unit.kind,
                ContentUnitKind::TableRow | ContentUnitKind::Total
            )
        {
            let mut from = 0;
            let mut char_from = 0;
            let mut char_to = 0;
            for (byte, ch) in text.char_indices() {
                char_to += 1;
                if matches!(ch, '\n' | '。' | '！' | '？') {
                    let to = byte + ch.len_utf8();
                    let mut ranges = unit.ranges.clone();
                    for range in &mut ranges {
                        if range.unit == SourceRangeUnit::TextChar
                            && range.end - range.start == unit.text_end - unit.text_start
                        {
                            let origin = range.start;
                            range.start = origin + char_from;
                            range.end = origin + char_to;
                        }
                    }
                    output.push(Piece {
                        text: text[from..to].to_string(),
                        kind: unit.kind,
                        ranges,
                    });
                    from = to;
                    char_from = char_to;
                }
            }
            if from < text.len() {
                output.push(Piece {
                    text: text[from..].into(),
                    kind: unit.kind,
                    ranges: unit.ranges,
                });
            }
        } else {
            output.push(Piece {
                text,
                kind: unit.kind,
                ranges: unit.ranges,
            });
        }
    }
    output
        .into_iter()
        .filter(|piece| {
            piece.text.chars().count() <= ROUND_TARGET
                || !useful(piece.kind)
                || matches!(
                    piece.kind,
                    ContentUnitKind::TableRow | ContentUnitKind::Total
                )
        })
        .collect()
}

fn priority(pieces: &[Piece]) -> u8 {
    if pieces.iter().any(|p| p.kind == ContentUnitKind::Total) {
        return 0;
    }
    // Complete table rows carry the relationship between fields and values.
    // Their priority must not depend on prose keywords in a particular file.
    if pieces
        .iter()
        .any(|p| p.kind == ContentUnitKind::TableRow && p.text.chars().any(|c| c.is_ascii_digit()))
    {
        return 1;
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
    if pieces.iter().any(|p| {
        p.text.chars().any(|c| c.is_ascii_digit())
            && MARKERS.iter().any(|marker| p.text.contains(marker))
    }) {
        return 1;
    }
    if pieces
        .iter()
        .any(|piece| QUALIFIERS.iter().any(|word| piece.text.contains(word)))
    {
        return 2;
    }
    if pieces.iter().any(|p| p.kind == ContentUnitKind::TableRow) {
        return 3;
    }
    if pieces
        .iter()
        .any(|p| useful(p.kind) && p.text.chars().any(|c| c.is_ascii_digit()))
    {
        3
    } else {
        4
    }
}

/// Reorder complete blocks from the already-read content, retaining their physical ranges.
/// First content is followed by totals, quantitative conditions and section openings.
fn bundles(item: &ContextFileEvidence) -> Vec<Vec<Piece>> {
    let original = pieces(item);
    let mut blocks: Vec<(u8, usize, Vec<Piece>)> = Vec::new();
    let mut context = Vec::new();
    let mut current = Vec::new();
    let mut size = 0;
    let mut section_first = true;
    let mut current_first = true;
    let mut has_useful = false;
    for piece in original {
        let cost = piece.text.chars().count() + usize::from(!current.is_empty());
        let target = if blocks.is_empty() {
            FIRST_TARGET
        } else {
            ROUND_TARGET
        };
        let current_rank = priority(&current);
        let incoming_rank = if useful(piece.kind) {
            let mut contextual = context.clone();
            contextual.push(piece.clone());
            priority(&contextual)
        } else {
            priority(std::slice::from_ref(&piece))
        };
        let changes_priority = useful(piece.kind)
            && current_rank != incoming_rank
            && (current_rank <= 2 || incoming_rank <= 2);
        if has_useful
            && (piece.kind == ContentUnitKind::Heading || size + cost > target || changes_priority)
        {
            let rank = if current_first {
                priority(&current).min(3)
            } else {
                priority(&current)
            };
            blocks.push((rank, blocks.len(), std::mem::take(&mut current)));
            size = 0;
            has_useful = false;
        }
        if piece.kind == ContentUnitKind::Heading {
            context.clear();
            section_first = true;
        }
        if !useful(piece.kind) && piece.kind != ContentUnitKind::Placeholder {
            context.push(piece.clone());
        }
        if current.is_empty() {
            current_first = section_first;
            // Prior headers and units are required when a later block starts inside a table.
            if useful(piece.kind) {
                let context_chars = context
                    .iter()
                    .map(|p: &Piece| p.text.chars().count() + 1)
                    .sum::<usize>();
                if context_chars <= ROUND_TARGET {
                    current.extend(context.iter().cloned());
                    size = context_chars;
                } else {
                    continue;
                }
            }
        }
        size += piece.text.chars().count() + usize::from(!current.is_empty());
        has_useful |= useful(piece.kind);
        if useful(piece.kind) {
            section_first = false;
        }
        current.push(piece);
    }
    if has_useful {
        let rank = if current_first {
            priority(&current).min(3)
        } else {
            priority(&current)
        };
        blocks.push((rank, blocks.len(), current));
    }
    if blocks.len() > 1 {
        // Keep the initial subject and date before any detail from later sections.
        let mut remaining = blocks.split_off(1);
        remaining.sort_by_key(|(rank, position, _)| (*rank, *position));
        let mut quantitative = VecDeque::new();
        let mut qualifications = VecDeque::new();
        let mut depth = Vec::new();
        for block in remaining {
            match block.0 {
                0 => blocks.push(block),
                1 => quantitative.push_back(block),
                2 => qualifications.push_back(block),
                _ => depth.push(block),
            }
        }
        // Numbers and their necessary qualifications share the same bounded
        // opportunity; a large table cannot consume all qualification space.
        while !quantitative.is_empty() || !qualifications.is_empty() {
            if let Some(block) = quantitative.pop_front() {
                blocks.push(block);
            }
            if let Some(block) = qualifications.pop_front() {
                blocks.push(block);
            }
        }
        blocks.extend(depth);
    }
    blocks.into_iter().map(|(_, _, pieces)| pieces).collect()
}

fn structural_context(pieces: &[Piece]) -> Vec<&Piece> {
    let begin = pieces
        .iter()
        .rposition(|piece| piece.kind == ContentUnitKind::Heading)
        .unwrap_or(0);
    pieces[begin..]
        .iter()
        .filter(|piece| !useful(piece.kind) && piece.kind != ContentUnitKind::Placeholder)
        .collect()
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
    let units: Vec<_> = evidence.iter().map(|item| bundles(item)).collect();
    let mut positions = vec![0; evidence.len()];
    let mut selected: Vec<Vec<Piece>> = vec![vec![]; evidence.len()];
    let mut sizes = vec![0; evidence.len()];
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, item) in evidence.iter().enumerate() {
        if !units[index].is_empty() {
            groups.entry(&item.extension).or_default().push(index);
        }
    }
    for group in groups.values_mut() {
        group.sort_by(|a, b| {
            (&evidence[*a].relative_path, &evidence[*a].file_identity)
                .cmp(&(&evidence[*b].relative_path, &evidence[*b].file_identity))
        });
    }
    let mut order = Vec::new();
    let max_len = groups.values().map(Vec::len).max().unwrap_or(0);
    for round in 0..max_len {
        for group in groups.values() {
            if let Some(index) = group.get(round) {
                order.push(*index);
            }
        }
    }
    let mut remaining = budget;
    let mut first_round = true;
    let is_priority: Vec<_> = evidence
        .iter()
        .map(|item| {
            priority_paths
                .binary_search(&item.relative_path.replace('\\', "/"))
                .is_ok()
        })
        .collect();
    let mut priority_depth = is_priority.iter().any(|value| *value);
    loop {
        let mut progress = false;
        let mut round_order = order.clone();
        if !first_round {
            if priority_depth {
                round_order.retain(|index| is_priority[*index]);
            }
            round_order.sort_by_key(|index| {
                units[*index]
                    .get(positions[*index])
                    .map_or(u8::MAX, |block| priority(block))
            });
        }
        for index in &round_order {
            let index = *index;
            let start = positions[index];
            if start >= units[index].len() {
                continue;
            }
            let block = &units[index][start];
            let prefix = block.iter().take_while(|piece| !useful(piece.kind)).count();
            let trim =
                if structural_context(&selected[index]) == structural_context(&block[..prefix]) {
                    prefix
                } else {
                    0
                };
            let block = &block[trim..];
            let size = block.iter().map(|p| p.text.chars().count()).sum::<usize>()
                + block.len().saturating_sub(1)
                + usize::from(sizes[index] > 0);
            if size <= remaining && selected[index].len() + block.len() <= 512 {
                selected[index].extend_from_slice(block);
                positions[index] += 1;
                sizes[index] += size;
                remaining -= size;
                progress = true;
            }
        }
        if !progress && !first_round && priority_depth {
            priority_depth = false;
            continue;
        }
        if !progress || remaining == 0 {
            break;
        }
        first_round = false;
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
            for piece in &selected[index] {
                if !text.is_empty() {
                    text.push('\n');
                    chars += 1;
                }
                let start = chars;
                text.push_str(&piece.text);
                chars += piece.text.chars().count() as u64;
                if useful(piece.kind) {
                    usable += chars - start;
                }
                for range in &piece.ranges {
                    if !ranges.contains(range) {
                        ranges.push(range.clone());
                    }
                }
                mapped.push(ContentUnit {
                    unit_id: format!("e{}", mapped.len() + 1),
                    kind: piece.kind,
                    text_start: start,
                    text_end: chars,
                    ranges: piece.ranges.clone(),
                });
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
                truncated: positions[index] != units[index].len()
                    || text.chars().count() < item.content.chars().count(),
                text,
                audit,
                ranges: known.then_some(ranges),
            }
        })
        .collect()
}
