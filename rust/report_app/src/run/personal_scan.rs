//! Adapt the native scanner's bounded semantic excerpts to the personal report.
//! Scan identities describe a frozen observation, never an archive version.
use super::{input, App};
use crate::{
    evidence::{EvidencePermissions, EvidenceRef},
    reports::{
        data::ReportSource,
        personal::{Material, Selection, MAX_INPUT_CHARS},
    },
    snapshots::digest,
    types::ReportRequest,
    ReportError,
};
use ai_daily_scanner_contract::EngineStatus;
use ai_daily_worker_contract::{ContentUnit, ContentUnitKind, SourceRange, SourceRangeUnit};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

fn invalid_units() -> ReportError {
    ReportError::new("SCAN_SOURCE_INVALID", "扫描摘录的结构位置与原文不一致")
}

fn short_text_has_gap(
    units: &[ContentUnit],
    read_ranges: Option<&Value>,
    omitted_ranges: Option<&Value>,
) -> bool {
    let span = |r: &Value| {
        let (start, end) = (r["start"].as_u64()?, r["end"].as_u64()?);
        (start < end).then_some((start, end))
    };
    let extent = |kind| {
        units
            .iter()
            .flat_map(|u| &u.ranges)
            .filter(|r| r.unit == kind)
            .map(|r| (r.start, r.end))
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
    };
    let bytes = extent(SourceRangeUnit::Byte);
    let chars = extent(SourceRangeUnit::TextChar);
    // A missing prefix or suffix cannot separate two retained statements.
    // With no comparable positions, keep the native boundaries for a known gap.
    let separates = |start, end, extent: Option<(u64, u64)>| {
        extent.is_none_or(|(first, last)| first < start && end < last)
    };
    if omitted_ranges
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|r| {
            let extent = match r["unit"].as_str() {
                Some("byte") => bytes,
                Some("text_char") => chars,
                _ => return false,
            };
            span(r).is_some_and(|(a, b)| separates(a, b, extent))
        })
    {
        return true;
    }
    // A full read can report both bytes and characters for the same text. Touching
    // or nested byte windows also cover a single continuous interval.
    let mut windows: Vec<_> = read_ranges
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|r| r["unit"] == "byte")
        .filter_map(span)
        .collect();
    windows.sort_unstable();
    let mut windows = windows.into_iter();
    let Some((_, mut through)) = windows.next() else {
        return false;
    };
    windows.any(|(start, end)| {
        let disjoint = start > through && separates(through, start, bytes);
        through = through.max(end);
        disjoint
    })
}

/// Preserve native excerpt boundaries. The report and request can then choose
/// different blocks without parsing Markdown or opening the source file again.
fn materials(
    mut source: ReportSource,
    text: &str,
    path: &Path,
    run_id: &str,
) -> Result<Vec<Material>, ReportError> {
    let file_id = source.source_id.clone();
    let raw_hash = source.content_sha256.clone().ok_or_else(invalid_units)?;
    if digest(text.as_bytes()) != source.excerpt_sha256 {
        return Err(invalid_units());
    }
    let audit = source.coverage_limits["evidence_audit"]
        .as_object_mut()
        .ok_or_else(invalid_units)?;
    let units: Option<Vec<ContentUnit>> =
        serde_json::from_value(audit.remove("content_units").unwrap_or(Value::Null))?;
    let ends: Option<Vec<usize>> =
        serde_json::from_value(audit.remove("excerpt_block_ends").unwrap_or(Value::Null))?;
    let inherited: Option<Vec<Vec<usize>>> =
        serde_json::from_value(audit.remove("excerpt_block_context").unwrap_or(Value::Null))?;
    let excerpt_ranges: Option<Vec<SourceRange>> =
        serde_json::from_value(audit.remove("excerpt_ranges").unwrap_or(Value::Null))?;
    // Read and omitted ranges describe the file once. Excerpt ranges belong to each block.
    let mut read_ranges = audit.remove("read_ranges");
    let mut omitted_ranges = audit.remove("omitted_ranges");
    let units = units.unwrap_or_default();
    let mut ends = ends.unwrap_or_else(|| vec![units.len()]);
    let mut inherited = inherited.unwrap_or_else(|| vec![Vec::new(); ends.len()]);
    if ends.is_empty()
        || ends.last() != Some(&units.len())
        || ends.len() != inherited.len()
        || ends.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(invalid_units());
    }
    let offsets: Vec<usize> = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain([text.len()])
        .collect();
    let total = offsets.len() - 1;
    for unit in &units {
        if unit.text_start >= unit.text_end || unit.text_end > total as u64 {
            return Err(invalid_units());
        }
    }
    if units.windows(2).any(|w| w[0].text_end > w[1].text_start) {
        return Err(invalid_units());
    }
    // Keep a continuous short note and its qualifiers together. Sampled or
    // partially omitted text keeps its native boundaries and inherited context.
    if total <= 512
        && matches!(
            source.extension.as_deref(),
            Some(".md" | ".txt" | ".log" | ".json")
        )
        && units.iter().all(|u| {
            matches!(
                u.kind,
                ContentUnitKind::Heading | ContentUnitKind::Paragraph | ContentUnitKind::Text
            )
        })
        && !short_text_has_gap(&units, read_ranges.as_ref(), omitted_ranges.as_ref())
    {
        ends = vec![units.len()];
        inherited = vec![Vec::new()];
    }
    let mut result = Vec::with_capacity(ends.len());
    let mut first = 0;
    let mut start_char = 0;
    for (end, parents) in ends.into_iter().zip(inherited) {
        let end_char = units
            .get(end)
            .map_or(total, |unit| unit.text_start as usize);
        if end_char <= start_char || end > units.len() {
            return Err(invalid_units());
        }
        let quote = &text[offsets[start_char]..offsets[end_char]];
        let quote_hash = digest(quote.as_bytes());
        let mut block = source.clone();
        if first != 0 {
            block.source_id = format!(
                "src_{}",
                digest(format!("{file_id}/{start_char}").as_bytes())
            );
        }
        let mut ranges = Vec::new();
        for range in units[first..end].iter().flat_map(|unit| &unit.ranges) {
            if !ranges.contains(range) {
                ranges.push(range.clone());
            }
        }
        if units.is_empty() {
            ranges = excerpt_ranges.clone().unwrap_or_default();
        }
        let mut context = Vec::with_capacity(parents.len());
        for index in parents {
            let unit = units
                .get(index)
                .filter(|_| index < first)
                .ok_or_else(invalid_units)?;
            context.push(json!({
                "text": &text[offsets[unit.text_start as usize]..offsets[unit.text_end as usize]],
                "ranges": unit.ranges,
            }));
        }
        block.coverage_limits["personal"] = json!({
            "absolute_path": path, "input_mode": "scan", "selection_basis": "modified_at",
            "file_source_id": file_id, "excerpt_start": start_char, "excerpt_end": end_char,
            "context": context,
        });
        block.coverage_limits["evidence_audit"]["excerpt_ranges"] = json!(ranges);
        if first == 0 {
            if let Some(read) = read_ranges.take() {
                block.coverage_limits["evidence_audit"]["read_ranges"] = read;
            }
            if let Some(omitted) = omitted_ranges.take() {
                block.coverage_limits["evidence_audit"]["omitted_ranges"] = omitted;
            }
        }
        block.excerpt_chars = end_char - start_char;
        block.excerpt_sha256 = quote_hash.clone();
        let evidence = EvidenceRef {
            source_id: block.source_id.clone(),
            version_id: format!("scan-content:{raw_hash}"),
            extraction_id: format!("scan:{run_id}:{file_id}"),
            unit_id: format!("scan-excerpt:{quote_hash}"),
            side: "current".into(),
            raw_sha256: raw_hash.clone(),
            unit_sha256: quote_hash.clone(),
            quote: quote.to_owned(),
            quote_sha256: quote_hash,
            unit_char_start: 0,
            input_start: 0,
            input_end: block.excerpt_chars,
            ranges,
        };
        let priority = if units[first..end]
            .iter()
            .any(|u| u.kind == ContentUnitKind::Total)
        {
            1
        } else {
            2
        };
        result.push(Material {
            source: block,
            text: quote.to_owned(),
            evidence: vec![evidence],
            priority,
        });
        first = end;
        start_char = end_char;
    }
    Ok(result)
}

pub fn prepare(app: &App, request: &ReportRequest) -> Result<Selection, ReportError> {
    let mut selection = Selection::default();
    let notes = input::personal_notes(&app.config, &request.context, &request.input)?;
    if !notes.is_empty() {
        selection.summary.selected_chars = notes.chars().count();
        selection.materials.push(Material {
            source: ReportSource::user(&notes, request.context.started_at),
            text: notes,
            evidence: vec![],
            priority: 0,
        });
    }
    let scan = app.scan(request)?;
    let (sources, gaps) = input::scan_sources(&scan)?;
    selection.gaps = gaps;
    selection.summary.incomplete_inventory = scan.envelope.status != EngineStatus::Ok;
    selection.summary.observed_files = sources.len() as u64;
    let raw: BTreeMap<_, _> = scan
        .sources
        .iter()
        .map(|s| (s.source_id.as_str(), s))
        .collect();
    let permissions = EvidencePermissions::new(&app.config);
    for source in sources {
        selection.summary.inspected_files += 1;
        let path = source
            .relative_path
            .as_deref()
            .unwrap_or_default()
            .to_owned();
        let absolute = app.config.paths.work_dir.join(&path);
        if !permissions
            .as_ref()
            .is_some_and(|policy| policy.permits(&absolute))
        {
            selection.summary.omit("out_of_scope", &path);
            continue;
        }
        if !source.usable() {
            let reason = match source.coverage_limits["evidence_audit"]["primary_reason"].as_str() {
                Some("file_size_policy") => "file_size_policy",
                Some("office_content_missing") => "office_content_missing",
                Some(
                    "pdf_image_ocr_file_quota_exhausted" | "pdf_image_ocr_page_quota_exhausted",
                ) => "ocr_budget",
                _ if source.parse_status == "success" => "no_usable_text",
                _ => "read_failed",
            };
            selection.summary.omit(reason, &path);
            continue;
        }
        let original = raw[source.source_id.as_str()];
        let chars = source.excerpt_chars;
        if selection.summary.selected_chars + chars > MAX_INPUT_CHARS {
            // Keep the complete scanner excerpt; never cut a sentence or table row here.
            selection.summary.omit("budget", &path);
            continue;
        }
        selection.materials.extend(materials(
            source,
            &original.excerpt,
            &absolute,
            &request.context.run_id,
        )?);
        selection.summary.selected_files += 1;
        selection.summary.selected_chars += chars;
    }
    selection.validate()?;
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_audit_merges_continuous_windows_and_preserves_internal_gaps() {
        let text = "甲\n乙";
        let range = |unit, start, end| SourceRange::new(unit, start, end, None);
        let byte = |a, b| range(SourceRangeUnit::Byte, a, b);
        let chars = |a, b| range(SourceRangeUnit::TextChar, a, b);
        let blocks = |reads: Vec<SourceRange>, omitted: Vec<SourceRange>| {
            let mut source = ReportSource::user(text, chrono::Utc::now());
            source.extension = Some(".md".into());
            source.coverage_limits["evidence_audit"] = json!({
                "content_units": [
                    {"unit_id":"a","kind":"text","text_start":0,"text_end":1,"ranges":[byte(0, 3)]},
                    {"unit_id":"b","kind":"text","text_start":2,"text_end":3,"ranges":[byte(9, 12)]}
                ],
                "excerpt_block_ends":[1, 2], "excerpt_block_context":[[], []],
                "read_ranges":reads, "omitted_ranges":omitted
            });
            materials(source, text, Path::new("fixture.md"), "fixture").unwrap()
        };
        let full = vec![byte(3, 9), byte(0, 9), chars(0, 4), byte(9, 12)];
        assert_eq!(blocks(full.clone(), vec![]).len(), 1);
        assert_eq!(blocks(full.clone(), vec![byte(12, 15)]).len(), 1);
        assert_eq!(blocks(vec![byte(9, 12), byte(0, 3)], vec![]).len(), 2);
        let internal = blocks(full, vec![byte(3, 9)]);
        assert_eq!(internal.len(), 2);
        assert_eq!(
            internal[0].source.coverage_limits["evidence_audit"]["omitted_ranges"],
            json!([byte(3, 9)])
        );
        for field in ["read_ranges", "omitted_ranges"] {
            assert!(internal[1].source.coverage_limits["evidence_audit"]
                .get(field)
                .is_none());
        }
    }
}
