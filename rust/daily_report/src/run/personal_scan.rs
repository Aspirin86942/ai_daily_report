//! Adapt the native scanner's bounded semantic excerpts to the personal report.
//! Scan identities describe a frozen observation, never an archive version.
use super::{input, App};
use crate::{
    evidence::{EvidencePermissions, EvidenceRef},
    infra::json::digest,
    reports::{
        data::ReportSource,
        personal::{Material, Selection, MAX_FILE_CHARS},
    },
    scan::sources::{range_spans, ranges_for, AllocatedExcerpt, ExcerptBlock},
    types::ReportRequest,
    ReportError,
};
use daily_parsers::contract::{ContentUnit, ContentUnitKind, SourceRangeUnit};
use daily_parsers::settings::EngineStatus;
use serde_json::json;
use std::path::Path;

fn invalid_units() -> ReportError {
    ReportError::new("SCAN_SOURCE_INVALID", "扫描摘录的结构位置与原文不一致")
}

fn short_text_has_gap(units: &[ContentUnit], excerpt: &AllocatedExcerpt) -> bool {
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
    // A missing prefix or suffix cannot separate retained statements.
    let separates = |start, end, extent: Option<(u64, u64)>| {
        extent.is_none_or(|(first, last)| first < start && end < last)
    };
    if excerpt.omitted_ranges.iter().any(|range| {
        let extent = match range.unit {
            SourceRangeUnit::Byte => bytes,
            SourceRangeUnit::TextChar => chars,
            _ => return false,
        };
        separates(range.start, range.end, extent)
    }) {
        return true;
    }
    let mut windows = range_spans(
        excerpt.read_ranges.as_deref().unwrap_or_default(),
        SourceRangeUnit::Byte,
    );
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

fn materials(
    source: ReportSource,
    excerpt: &AllocatedExcerpt,
    path: &Path,
    run_id: &str,
) -> Result<Vec<Material>, ReportError> {
    let text = &excerpt.text;
    let file_id = source.source_id.clone();
    let raw_hash = source.content_sha256.clone().ok_or_else(invalid_units)?;
    let extraction_id = format!("scan:{run_id}:{file_id}");
    let version_id = format!("scan-content:{raw_hash}");
    let units = excerpt.units.as_deref().unwrap_or_default();
    let offsets: Vec<_> = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain([text.len()])
        .collect();
    let total = offsets.len() - 1;
    if units
        .iter()
        .any(|u| u.text_start >= u.text_end || u.text_end > total as u64)
        || units
            .windows(2)
            .any(|pair| pair[0].text_end > pair[1].text_start)
        || excerpt
            .blocks
            .last()
            .is_some_and(|block| block.end != units.len())
        || excerpt
            .blocks
            .windows(2)
            .any(|pair| pair[0].end >= pair[1].end)
    {
        return Err(invalid_units());
    }

    // Keep a continuous short note and its qualifiers together.
    let continuous_note = total <= 512
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
        && !short_text_has_gap(units, excerpt);
    let file_block = ExcerptBlock {
        end: units.len(),
        inherited: Vec::new(),
    };
    let blocks = if excerpt.blocks.is_empty() || continuous_note {
        std::slice::from_ref(&file_block)
    } else {
        &excerpt.blocks
    };

    let mut result = Vec::with_capacity(blocks.len());
    let (mut first, mut start_char) = (0, 0);
    for boundary in blocks {
        let end = boundary.end;
        let end_char = units.get(end).map_or(total, |u| u.text_start as usize);
        if end_char <= start_char {
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
        let block_units = &units[first..end];
        let ranges = if units.is_empty() {
            excerpt.ranges.clone().unwrap_or_default()
        } else {
            ranges_for(block_units)
        };
        let context: Vec<_> = boundary.inherited.iter().map(|&index| {
            let unit = units.get(index).filter(|_| index < first).ok_or_else(invalid_units)?;
            Ok(json!({
                "text": &text[offsets[unit.text_start as usize]..offsets[unit.text_end as usize]],
                "ranges": unit.ranges,
            }))
        }).collect::<Result<_, ReportError>>()?;
        block.coverage_limits["personal"] = json!({
            "absolute_path": path, "input_mode": "scan", "selection_basis": "modified_at",
            "file_source_id": file_id, "excerpt_start": start_char, "excerpt_end": end_char,
            "context": context,
        });
        block.coverage_limits["evidence_audit"]["excerpt_ranges"] = json!(ranges);
        block.excerpt_chars = end_char - start_char;
        block.excerpt_sha256 = quote_hash.clone();
        let evidence = EvidenceRef {
            source_id: block.source_id.clone(),
            version_id: version_id.clone(),
            extraction_id: extraction_id.clone(),
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
        let priority = if block_units.iter().any(|u| u.kind == ContentUnitKind::Total) {
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
        (first, start_char) = (end, end_char);
    }
    // File-wide ranges travel with the first block.
    excerpt.write_file_ranges(&mut result[0].source.coverage_limits["evidence_audit"]);
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
    select_files(
        &app.config,
        &request.context.run_id,
        selection,
        app.scan(request)?,
    )
}

fn select_files(
    config: &crate::config::AppConfig,
    run_id: &str,
    mut selection: Selection,
    mut scan: crate::scan::ScanResult,
) -> Result<Selection, ReportError> {
    selection.summary.incomplete_inventory = scan.status != EngineStatus::Ok;
    selection.summary.observed_files = scan.files.len() as u64;
    input::scan_gaps(&scan, &mut selection.gaps)?;
    scan.files.sort_by(|a, b| {
        a.source_id
            .cmp(&b.source_id)
            .then_with(|| a.relative_path.cmp(&b.relative_path))
    });
    let permissions = EvidencePermissions::new(config);
    let mut selection_gaps = Vec::new();
    let mut file_chars = 0;
    for file in scan.files {
        let mut source = input::report_source(&file, &scan.source_profile)?;
        let tags = input::file_gaps(&file, &mut selection.gaps);
        selection.summary.inspected_files += 1;
        let path = &file.relative_path;
        let absolute = config.paths.work_dir.join(path);
        let mut omit = |reason, message| {
            selection.summary.omit(reason, path);
            selection_gaps.push(input::file_gap(
                &file,
                "SCAN_SOURCE_OMITTED",
                "selection",
                message,
            ));
        };
        if !permissions
            .as_ref()
            .is_some_and(|policy| policy.permits(&absolute))
        {
            omit("out_of_scope", "使用权限范围外，未采纳。");
            continue;
        }
        if !source.usable() {
            let (reason, message) = match file.reason {
                Some("file_size_policy") => {
                    ("file_size_policy", "超过单文件大小限制，未读取正文。")
                }
                Some("office_content_missing") => {
                    ("office_content_missing", "缺少可提取的 Office 正文。")
                }
                Some(
                    "pdf_image_ocr_file_quota_exhausted" | "pdf_image_ocr_page_quota_exhausted",
                ) => ("ocr_budget", "图片内容的识别额度不足，未采纳。"),
                _ if source.parse_status == "success" => ("no_usable_text", "未取得可用的文字。"),
                _ => ("read_failed", "无法读取或确认稳定的文件版本，未采纳。"),
            };
            omit(reason, message);
            continue;
        }
        let chars = source.excerpt_chars;
        if file_chars + chars > MAX_FILE_CHARS {
            // Keep the complete scanner excerpt; never cut a sentence or table row here.
            omit("budget", "超过本次 180,000 字文件材料预算，未采纳。");
            continue;
        }
        source.coverage_limits = file.report_limits(&scan.source_profile, tags);
        selection
            .materials
            .extend(materials(source, &file.excerpt, &absolute, run_id)?);
        selection.summary.selected_files += 1;
        selection.summary.selected_chars += chars;
        file_chars += chars;
    }
    input::sort_gaps(&mut selection.gaps);
    selection.gaps.extend(selection_gaps);
    selection.validate()?;
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use daily_parsers::contract::{ParsedContentAudit, SourceRange};

    #[test]
    fn frozen_reply_produces_identical_publication_bytes_at_each_worker_count() {
        use crate::{
            config::AppConfig,
            model::personal::PersonalOutput,
            run::personal::new_input,
            scan::{cache::CACHE_FILENAME, ScanRequest, Scanner, ScannerConfig},
            types::RunContext,
        };
        use std::{collections::BTreeMap, fs};

        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        fs::create_dir(&work).unwrap();
        fs::create_dir(root.path().join("config")).unwrap();
        fs::write(
            root.path().join("config/settings.yaml"),
            "paths:\n  work_dir: work\narchive:\n  allow_work_dir_for_model: true\n",
        )
        .unwrap();
        let config = AppConfig::load_with_env(root.path(), None, &BTreeMap::new()).unwrap();
        let clock = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let app = App::open(config, clock).unwrap();
        let context =
            RunContext::new("1".repeat(32), clock, clock.date_naive(), String::new()).unwrap();
        for i in 0..8 {
            fs::write(
                work.join(format!("项目{i}.md")),
                format!("项目{i} 金额 120 元，尚未批准。"),
            )
            .unwrap();
        }
        let mut reply = None;
        let mut expected = None;
        for workers in [1, 2, 4, 8] {
            let scanner = Scanner::open(ScannerConfig {
                work_dir: work.clone(),
                scan_db_path: root
                    .path()
                    .join(format!("cache-{workers}"))
                    .join(CACHE_FILENAME),
                scanner_settings: serde_json::from_value(json!({"max_workers":workers})).unwrap(),
                pdf_worker_path: app.config.pdf_worker_path.clone(),
            })
            .unwrap();
            for _ in 0..2 {
                let scan = scanner
                    .build_context_at(
                        &ScanRequest {
                            start_date: "1970-01-01".parse().unwrap(),
                            end_date: "2099-12-31".parse().unwrap(),
                            utc_window: None,
                        },
                        clock,
                    )
                    .unwrap();
                let selection =
                    select_files(&app.config, &context.run_id, Selection::default(), scan).unwrap();
                assert_eq!(selection.materials.len(), 8);
                let items = reply.get_or_insert_with(|| json!([{
                    "title":"核对金额","text":"金额 120 元，尚未批准。","kind":"attention",
                    "statement_kind":"observed","source_ids":[selection.materials[0].source.source_id]
                }]));
                let input = new_input(&app, context.clone(), selection).unwrap();
                let output = PersonalOutput {
                    items: serde_json::from_value(items.clone()).unwrap(),
                    omitted: 0,
                    compaction_error: None,
                };
                let mut report = input.report("work", Some(Ok(output)));
                let summary = input.render_output(&mut report).unwrap();
                report.validate_output(&summary).unwrap();
                let attachment = report.evidence_attachment.unwrap().markdown;
                assert!(summary.contains("120 元") && attachment.contains("项目7"));
                let files = (summary, attachment);
                assert_eq!(expected.get_or_insert_with(|| files.clone()), &files);
            }
        }
    }

    #[test]
    fn short_text_audit_merges_continuous_windows_and_preserves_internal_gaps() {
        let text = "甲\n乙";
        let range = |unit, start, end| SourceRange::new(unit, start, end, None);
        let byte = |a, b| range(SourceRangeUnit::Byte, a, b);
        let chars = |a, b| range(SourceRangeUnit::TextChar, a, b);
        let blocks = |reads: Vec<SourceRange>, omitted: Vec<SourceRange>| {
            let mut source = ReportSource::user(text, chrono::Utc::now());
            source.extension = Some(".md".into());
            let units: Vec<_> = [(0, byte(0, 3)), (2, byte(9, 12))]
                .into_iter()
                .map(|(start, range)| ContentUnit {
                    unit_id: format!("u{start}"),
                    kind: ContentUnitKind::Text,
                    text_start: start,
                    text_end: start + 1,
                    ranges: vec![range],
                })
                .collect();
            let excerpt = AllocatedExcerpt {
                text: text.to_owned(),
                chars: text.chars().count(),
                blocks: [1, 2]
                    .into_iter()
                    .map(|end| ExcerptBlock {
                        end,
                        inherited: Vec::new(),
                    })
                    .collect(),
                audit: Some(ParsedContentAudit::default()),
                units: Some(units),
                read_ranges: Some(reads),
                omitted_ranges: omitted,
                ..Default::default()
            };
            materials(source, &excerpt, Path::new("fixture.md"), "fixture").unwrap()
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
