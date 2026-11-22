use super::*;
use std::collections::HashSet;

pub(crate) fn range_spans(ranges: &[SourceRange], unit: SourceRangeUnit) -> Vec<(u64, u64)> {
    ranges
        .iter()
        .filter(|r| r.unit == unit)
        .map(|r| (r.start, r.end))
        .collect()
}

pub(super) fn omitted_text_ranges(
    item: &ExcerptInput,
    truncated: bool,
    emitted: Option<&[SourceRange]>,
) -> Vec<SourceRange> {
    let mut gaps = Vec::new();
    if !(truncated || item.truncated) || !matches!(item.extension, ".txt" | ".md") {
        return gaps;
    }
    let Some(read) = item
        .evidence_audit
        .and_then(|audit| audit.read_ranges.as_deref())
    else {
        return gaps;
    };
    let mut bytes = range_spans(read, SourceRangeUnit::Byte);
    bytes.sort_unstable_by_key(|&(start, _)| start);
    if !bytes.is_empty() {
        range_gaps(
            0,
            item.size_bytes,
            SourceRangeUnit::Byte,
            "unread",
            bytes.iter().copied(),
            &mut gaps,
        );
    }
    if let Some(emitted) = emitted {
        let chars = read.iter().find(|r| r.unit == SourceRangeUnit::TextChar);
        let unit = chars.map_or(SourceRangeUnit::Byte, |r| r.unit);
        let mut spans = range_spans(emitted, unit);
        spans.sort_unstable();
        // Without character extents we cannot translate text positions into bytes.
        if !spans.is_empty() || emitted.is_empty() {
            let read_windows = chars.map_or(bytes, |r| vec![(r.start, r.end)]);
            for (start, end) in read_windows {
                range_gaps(
                    start,
                    end,
                    unit,
                    "not_excerpted",
                    spans.iter().copied(),
                    &mut gaps,
                );
            }
        }
    }
    gaps
}

pub(in crate::scan) fn range_gaps(
    start: u64,
    end: u64,
    unit: SourceRangeUnit,
    label: &str,
    covered: impl IntoIterator<Item = (u64, u64)>,
    gaps: &mut Vec<SourceRange>,
) {
    let mut at = start;
    for (start, through) in covered {
        if start >= end {
            break;
        }
        if at < start {
            gaps.push(SourceRange::new(unit, at, start, Some(label.into())));
        }
        at = at.max(through);
    }
    if at < end {
        gaps.push(SourceRange::new(unit, at, end, Some(label.into())));
    }
}

fn full_unit_starts(audit: &mut ParsedContentAudit, pieces: &[Piece], csv: bool) -> Vec<usize> {
    let units = audit.content_units.as_deref().unwrap_or_default();
    let mut slots = 512usize.saturating_sub(units.len());
    let mut long = units.iter().filter(|u| splittable_prose(u)).count();
    if csv || slots == 0 || long == 0 {
        return (0..=units.len()).collect();
    }
    let mut bytes = (256usize * 1024)
        .saturating_sub(serde_json::to_vec(audit).expect("serializable audit").len());
    let units = audit.content_units.take().unwrap();
    let ids: HashSet<_> = units.iter().map(|u| u.unit_id.clone()).collect();
    let mut next_id = 0;
    let mut mapped = Vec::with_capacity(units.len());
    let mut starts = Vec::with_capacity(units.len() + 1);
    for (unit, piece) in units.into_iter().zip(pieces) {
        starts.push(mapped.len());
        if !splittable_prose(&unit) {
            mapped.push(unit);
            continue;
        }
        let share = slots.div_ceil(long);
        long -= 1;
        if share == 0 {
            mapped.push(unit);
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
            ranges: part_ranges(&unit.ranges, len, len).into_owned(),
        };
        let cost = serde_json::to_vec(&upper)
            .expect("serializable content unit")
            .len()
            + 1;
        let extra = share.min(bytes / cost);
        if extra == 0 {
            mapped.push(unit);
            continue;
        }
        let cuts: Vec<_> = prose_segments(piece.text)
            .skip(1)
            .map(|(start, _)| start)
            .collect();
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
                ranges: part_ranges(&unit.ranges, start, end).into_owned(),
            });
            start = end;
        }
    }
    starts.push(mapped.len());
    audit.content_units = Some(mapped);
    starts
}

pub(super) fn full_excerpt(item: &ExcerptInput) -> AllocatedExcerpt {
    let pieces = pieces(item, false);
    let blocks = bundles(item, &pieces, false);
    if blocks.is_empty() {
        return AllocatedExcerpt {
            audit: item.evidence_audit.map(empty_audit),
            ..Default::default()
        }
        .finish(item, item.content_chars != 0);
    }
    let mut excerpt = AllocatedExcerpt {
        text: item.content.to_owned(),
        chars: item.content_chars,
        audit: item.evidence_audit.cloned(),
        ..Default::default()
    };
    if let Some(audit) = excerpt.audit.as_mut().filter(|a| a.content_units.is_some()) {
        let starts = full_unit_starts(audit, &pieces, item.extension.eq_ignore_ascii_case(".csv"));
        let mut begin = 0;
        for block in blocks {
            // Split oversized facts into separately citable sentences.
            let last = block.facts.last().unwrap().source_index;
            for end in starts[last] + 1..=starts[last + 1] {
                excerpt.blocks.push(ExcerptBlock {
                    end,
                    inherited: block
                        .context
                        .iter()
                        .map(|p| starts[p.source_index])
                        .filter(|&i| i < begin)
                        .collect(),
                });
                begin = end;
            }
        }
        // Trailing structural units or placeholders belong to the final block.
        excerpt.blocks.last_mut().unwrap().end = audit.content_units.as_ref().unwrap().len();
    }
    excerpt.finish(item, false)
}
