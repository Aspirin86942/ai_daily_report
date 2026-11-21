use super::*;

pub(super) fn omitted_text_ranges(
    item: &ContextFileEvidence,
    truncated: bool,
    emitted: Option<&[SourceRange]>,
) -> Vec<SourceRange> {
    let mut gaps = Vec::new();
    if !(truncated || item.truncated) || !matches!(item.extension.as_str(), ".txt" | ".md") {
        return gaps;
    }
    let Some(read) = item
        .evidence_audit
        .as_ref()
        .and_then(|a| a.read_ranges.as_deref())
    else {
        return gaps;
    };
    let mut bytes: Vec<_> = read
        .iter()
        .filter(|r| r.unit == SourceRangeUnit::Byte)
        .collect();
    bytes.sort_unstable_by_key(|r| r.start);
    if let Some(size) = item.size_bytes.filter(|_| !bytes.is_empty()) {
        range_gaps(0, size, SourceRangeUnit::Byte, "unread", &bytes, &mut gaps);
    }
    if let Some(emitted) = emitted {
        let chars = read.iter().find(|r| r.unit == SourceRangeUnit::TextChar);
        let unit = if chars.is_some() {
            SourceRangeUnit::TextChar
        } else {
            SourceRangeUnit::Byte
        };
        let mut spans: Vec<_> = emitted.iter().filter(|r| r.unit == unit).collect();
        spans.sort_unstable_by_key(|r| (r.start, r.end));
        // Earlier parser audits may not have character extents. Their text
        // coordinates cannot be translated to bytes without the original.
        if !spans.is_empty() || emitted.is_empty() {
            for window in chars
                .into_iter()
                .chain(bytes.into_iter().filter(|_| unit == SourceRangeUnit::Byte))
            {
                range_gaps(
                    window.start,
                    window.end,
                    unit,
                    "not_excerpted",
                    &spans,
                    &mut gaps,
                );
            }
        }
    }
    gaps
}

pub(super) fn range_gaps(
    start: u64,
    end: u64,
    unit: SourceRangeUnit,
    label: &str,
    covered: &[&SourceRange],
    gaps: &mut Vec<SourceRange>,
) {
    let mut at = start;
    for range in covered {
        if range.start >= end {
            break;
        }
        if at < range.start {
            gaps.push(SourceRange::new(unit, at, range.start, Some(label.into())));
        }
        at = at.max(range.end);
    }
    if at < end {
        gaps.push(SourceRange::new(unit, at, end, Some(label.into())));
    }
}

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

pub(super) fn full_excerpt(item: &ContextFileEvidence) -> AllocatedExcerpt {
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
    let truncated = text.len() < item.content.len();
    AllocatedExcerpt {
        truncated,
        omitted_ranges: omitted_text_ranges(item, truncated, ranges.as_deref()),
        text,
        audit,
        ranges,
        block_ends,
        block_context,
    }
}
