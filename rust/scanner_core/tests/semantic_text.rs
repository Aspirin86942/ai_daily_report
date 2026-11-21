use ai_daily_scanner_contract::{AuditWorkerLane, CacheStatus, ParseStatus, TextParseProfile};
use ai_daily_scanner_core::{decision::ContextFileEvidence, source_allocation::allocate_excerpts};
use ai_daily_text_parser::{parse_light_text, LightTextError, ParsedLightText};
use ai_daily_worker_contract::{ContentUnitKind, SourceRange, SourceRangeUnit};
use std::{fs, path::Path};

fn parse(
    path: &Path,
    input: &[u8],
    read: u64,
    output: u64,
) -> Result<ParsedLightText, LightTextError> {
    fs::write(path, input).unwrap();
    let profile = TextParseProfile {
        backend: "light_text_v2".into(),
        csv_encodings: ai_daily_scanner_contract::default_csv_encodings(),
        read_head_bytes: read,
        read_tail_bytes: read,
        max_chars: output,
        excerpt_max_chars: output,
    };
    parse_light_text(path, ".md", &profile, input.len() as u64)
}

fn evidence(parsed: ParsedLightText, bytes: usize) -> ContextFileEvidence {
    ContextFileEvidence {
        file_identity: "synthetic".into(),
        relative_path: "sample.md".into(),
        absolute_path: r"C:\synthetic\sample.md".into(),
        extension: ".md".into(),
        size_bytes: Some(bytes as u64),
        parser_backend: parsed.parser_backend,
        content: parsed.content,
        evidence_audit: Some(parsed.evidence_audit),
        worker_lane: AuditWorkerLane::RustCore,
        cache_status: CacheStatus::Miss,
        parse_status: ParseStatus::Success,
        truncated: parsed.truncated,
        error: None,
        reason: None,
    }
}

fn original(text: &str, range: &SourceRange) -> String {
    match range.unit {
        SourceRangeUnit::Byte => text[range.start as usize..range.end as usize].to_string(),
        SourceRangeUnit::TextChar => text
            .chars()
            .skip(range.start as usize)
            .take((range.end - range.start) as usize)
            .collect(),
        _ => panic!("unexpected coordinate system"),
    }
}

const OPENING: &str = "本记录仅用于复核。";
const MIDDLE: &str = "## 第三节 复核条件";
const CONDITION: &str = "登记已完成。但未经复核，不得付款。";
const ENDING: &str = "结论：𠀀批次尚未批准。";

fn long_document() -> String {
    let filler = "常规背景材料，仅作记录。\n".repeat(400);
    format!("# 说明\n{OPENING}\n{filler}{MIDDLE}\n{CONDITION}\n{filler}# 最终判断\n{ENDING}")
}

#[test]
fn three_bounded_reads_preserve_the_opening_middle_heading_and_ending_with_exact_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let input = long_document();
    let parsed = parse(&dir.path().join("windows.md"), input.as_bytes(), 1_200, 300).unwrap();
    assert!(parsed.truncated);
    let reads = parsed.evidence_audit.read_ranges.as_deref().unwrap();
    assert_eq!(
        reads
            .iter()
            .map(|r| r.label.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["head", "middle", "tail"]
    );
    assert_eq!(reads.iter().map(|r| r.end - r.start).sum::<u64>(), 1200);
    assert_eq!(reads[0].start, 0);
    assert_eq!(reads[2].end, input.len() as u64);
    let unread = [
        (reads[0].end, reads[1].start),
        (reads[1].end, reads[2].start),
    ];
    let file = evidence(parsed, input.len());
    for limit in [1000, 160] {
        let excerpt = allocate_excerpts(&[&file], limit).pop().unwrap();
        assert!(excerpt.text.chars().count() <= limit);
        for fact in [OPENING, MIDDLE, CONDITION, ENDING] {
            assert!(
                excerpt.text.contains(fact),
                "missing complete fixture fact: {fact}"
            );
            assert!(excerpt
                .ranges
                .as_ref()
                .unwrap()
                .iter()
                .any(|r| original(&input, r) == fact));
        }
        let audit = excerpt.audit.as_ref().unwrap();
        audit.validate(&excerpt.text).unwrap();
        for unit in audit.content_units.as_ref().unwrap() {
            for r in &unit.ranges {
                assert_eq!(r.unit, SourceRangeUnit::Byte);
                assert!(reads_cover(audit.read_ranges.as_deref().unwrap(), r));
                assert!(!excerpt
                    .omitted_ranges
                    .iter()
                    .any(|gap| gap.start < r.end && r.start < gap.end));
            }
        }
        assert_eq!(
            excerpt
                .omitted_ranges
                .iter()
                .filter(|g| g.label.as_deref() == Some("unread"))
                .map(|g| (g.start, g.end))
                .collect::<Vec<_>>(),
            unread
        );
    }
}

fn reads_cover(reads: &[SourceRange], range: &SourceRange) -> bool {
    reads
        .iter()
        .any(|r| r.unit == range.unit && r.start <= range.start && range.end <= r.end)
}

#[test]
fn unbroken_unicode_has_an_explicit_gap_and_only_a_complete_tail_sentence_can_enter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("line.md");
    let mut input = "𠀀".repeat(5_000);
    let parsed = parse(&path, input.as_bytes(), 601, 80).unwrap();
    let file = evidence(parsed, input.len());
    let omitted = allocate_excerpts(&[&file], 100).pop().unwrap();
    assert!(file.truncated && omitted.text.is_empty());
    assert_eq!(
        omitted
            .omitted_ranges
            .iter()
            .map(|r| r.end - r.start)
            .sum::<u64>(),
        input.len() as u64
    );
    input.push('。');
    input.push_str(ENDING);
    let parsed = parse(&path, input.as_bytes(), 601, 80).unwrap();
    assert!(parsed.content.ends_with(ENDING));
    let ranges = parsed
        .evidence_audit
        .content_units
        .unwrap()
        .into_iter()
        .flat_map(|u| u.ranges)
        .collect::<Vec<_>>();
    assert_eq!(ranges.len(), 1);
    assert_eq!(original(&input, &ranges[0]), ENDING);
    let mut malformed = input.into_bytes();
    let interior = malformed.len() / 2;
    malformed[interior] = 0xff;
    assert_eq!(
        parse(&path, &malformed, 601, 80).unwrap_err(),
        LightTextError::DecodeFailed
    );
}

#[test]
fn explicit_page_furniture_keeps_every_location_and_repeated_body_stays_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let page = "页眉：合成样本\n\n\n共同正文。\n页眉：合成样本\n逐页确认。\n页脚：仅供复核";
    let input = [page; 3].join("\n\x0c\n");
    let parsed = parse(&dir.path().join("pages.md"), input.as_bytes(), 4_000, 2_000).unwrap();
    assert!(!parsed.truncated);
    assert_eq!(parsed.content.matches("共同正文。").count(), 3);
    assert_eq!(parsed.content.matches("逐页确认。").count(), 3);
    assert_eq!(parsed.content.matches("页眉：合成样本").count(), 4);
    assert_eq!(parsed.content.matches("页脚：仅供复核").count(), 1);
    assert!(!parsed.content.contains("\n\n\n"));
    parsed.evidence_audit.validate(&parsed.content).unwrap();
    let units = parsed.evidence_audit.content_units.unwrap();
    for margin in ["页眉：合成样本", "页脚：仅供复核"] {
        let unit = units
            .iter()
            .find(|u| u.ranges.len() == 3 && original(&input, &u.ranges[0]) == margin)
            .unwrap();
        assert!(unit.ranges.windows(2).all(|p| p[0].end < p[1].start));
        assert!(unit.ranges.iter().all(|r| original(&input, r) == margin));
    }
    let unmarked = "固定文字\n一。\n固定文字\n二。\n固定文字";
    let parsed = parse(
        &dir.path().join("ordinary.md"),
        unmarked.as_bytes(),
        1_000,
        1_000,
    )
    .unwrap();
    assert!(parsed.content.ends_with(unmarked));
}

#[test]
fn a_cross_page_total_requires_the_same_columns_units_and_complete_row() {
    let dir = tempfile::tempdir().unwrap();
    let header = "| 项目 | 金额 |\n| --- | --- |";
    let total = "| 合计（未批准） | -21.75 |";
    let page = format!(
        "# 复核记录\n单位：万元\n{header}\n{}\n页脚：合成\n\x0c页眉：合成\n{header}\n{total}",
        (0..40)
            .map(|i| format!("| 合同{i} | 32 |"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let parsed = parse(&dir.path().join("table.md"), page.as_bytes(), 10_000, 140).unwrap();
    let file = evidence(parsed, page.len());
    for budget in [500, 85] {
        let excerpt = allocate_excerpts(&[&file], budget).pop().unwrap();
        let audit = excerpt.audit.unwrap();
        audit.validate(&excerpt.text).unwrap();
        let row = audit
            .content_units
            .unwrap()
            .into_iter()
            .find(|u| {
                u.kind == ContentUnitKind::TableRow
                    && u.ranges.iter().any(|r| original(&page, r) == total)
            })
            .unwrap();
        let sources = row
            .ranges
            .iter()
            .map(|r| original(&page, r))
            .collect::<Vec<_>>();
        assert_eq!(
            sources,
            ["单位：万元", "| 项目 | 金额 |", "| --- | --- |", total]
        );
    }
    let tiny = allocate_excerpts(&[&file], 10).pop().unwrap();
    assert!(!tiny.text.contains("-21"));
    assert!(tiny.truncated && !tiny.omitted_ranges.is_empty());
    let changed = page.replace(
        &format!("{header}\n{total}"),
        &format!("| 项目 | 数量（台） |\n| --- | --- |\n{total}"),
    );
    let parsed = parse(
        &dir.path().join("new-columns.md"),
        changed.as_bytes(),
        10_000,
        140,
    )
    .unwrap();
    let row = parsed
        .evidence_audit
        .content_units
        .unwrap()
        .into_iter()
        .find(|u| u.ranges.iter().any(|r| original(&changed, r) == total))
        .unwrap();
    assert!(row
        .ranges
        .iter()
        .all(|r| original(&changed, r) != "单位：万元"));
}

#[test]
fn each_page_table_uses_its_own_unit_header_and_source_position() {
    let dir = tempfile::tempdir().unwrap();
    let columns = "| 项目 | 金额 |\n| --- | --- |";
    let row = "| 乙 | 7 |";
    for (first, second, explicit) in [
        ("页眉：甲项目\n单位：万元", "页眉：乙项目（单位：元）", true),
        (
            "页眉：同项目（单位：万元）",
            "页眉：同项目（单位：万元）",
            true,
        ),
        ("页眉：甲项目\n单位：万元", "页眉：乙项目", false),
    ] {
        let input = format!("{first}\n{columns}\n| 甲 | 11 |\n\x0c{second}\n{columns}\n{row}");
        let page_start = input[..input.find('\x0c').unwrap()].chars().count() as u64 + 1;
        for limit in [500, 110] {
            let parsed = parse(&dir.path().join("unit.md"), input.as_bytes(), 9000, limit).unwrap();
            if limit == 500 {
                assert!(!parsed.truncated);
            }
            let file = evidence(parsed, input.len());
            let excerpt = allocate_excerpts(&[&file], 500).pop().unwrap();
            let audit = excerpt.audit.unwrap();
            audit.validate(&excerpt.text).unwrap();
            let (i, table) = audit
                .content_units
                .as_ref()
                .unwrap()
                .iter()
                .enumerate()
                .find(|(_, u)| {
                    u.kind == ContentUnitKind::TableRow
                        && u.ranges.iter().any(|r| original(&input, r) == row)
                })
                .unwrap_or_else(|| panic!("missing second table with {second} at budget {limit}"));
            let raw: Vec<_> = table.ranges.iter().map(|r| original(&input, r)).collect();
            let body = excerpt
                .text
                .chars()
                .skip(table.text_start as usize)
                .take((table.text_end - table.text_start) as usize)
                .collect::<String>();
            assert_eq!(body, raw.join("\n"));
            assert!(table.ranges.iter().all(|r| r.start >= page_start));
            assert_eq!(raw.contains(&second.to_owned()), explicit);
            assert!(excerpt.block_ends.contains(&(i + 1)));
        }
    }
}

#[test]
fn tsv_retains_every_delimiter_and_raw_line_at_both_edges_of_the_unit() {
    let dir = tempfile::tempdir().unwrap();
    for columns in ["项目\t金额\t说明", "\t金额\t说明"] {
        let lines = [
            columns,
            "\t75\t合成行",
            "项目乙\t\t核对",
            "尾项\t7\t",
            "\t\t",
        ];
        let input = lines.join("\r\n");
        let parsed = parse(
            &dir.path().join("empty-columns.md"),
            input.as_bytes(),
            9000,
            500,
        )
        .unwrap();
        assert!(!parsed.truncated);
        let file = evidence(parsed, input.len());
        for limit in [500, 45] {
            let excerpt = allocate_excerpts(&[&file], limit).pop().unwrap();
            let audit = excerpt.audit.unwrap();
            audit.validate(&excerpt.text).unwrap();
            let table = audit
                .content_units
                .unwrap()
                .into_iter()
                .find(|u| u.kind == ContentUnitKind::TableRow)
                .unwrap();
            let body = excerpt
                .text
                .chars()
                .skip(table.text_start as usize)
                .take((table.text_end - table.text_start) as usize)
                .collect::<String>();
            assert_eq!(body, lines.join("\n"));
            assert_eq!(
                table
                    .ranges
                    .iter()
                    .map(|r| original(&input, r))
                    .collect::<Vec<_>>(),
                lines
            );
            assert!(body.lines().all(|line| line.split('\t').count() == 3));
        }
    }
}

#[test]
fn short_notes_keep_their_preview_bytes_and_character_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let input = "# 摘要\n小组已核对𠀀项目。";
    let parsed = parse(&dir.path().join("short.md"), input.as_bytes(), 500, 500).unwrap();
    assert_eq!(
        parsed.content,
        format!("Text preview\nexcerpt_source: head\nread_window_truncated: false\n{input}")
    );
    assert!(!parsed.truncated);
    let file = evidence(parsed, input.len());
    let result = allocate_excerpts(&[&file], 500).pop().unwrap();
    assert_eq!(result.text, file.content);
    assert!(result.omitted_ranges.is_empty() && !result.truncated);
    for (r, text) in result.ranges.unwrap().iter().zip(input.lines()) {
        assert_eq!(r.unit, SourceRangeUnit::TextChar);
        assert_eq!(original(input, r), text);
    }
}

#[test]
fn wrapped_conditions_and_tabular_or_unframed_headers_survive_a_tight_budget() {
    let dir = tempfile::tempdir().unwrap();
    let condition = "已完成检查。\n但仅当双方复核后方可执行。";
    let input = format!("常规记录。\n{}\n{condition}", "独立说明。\n".repeat(60));
    let parsed = parse(&dir.path().join("wrapped.md"), input.as_bytes(), 10_000, 80).unwrap();
    assert!(parsed
        .evidence_audit
        .content_units
        .as_ref()
        .unwrap()
        .iter()
        .flat_map(|u| &u.ranges)
        .any(|r| original(&input, r) == condition));
    let f = evidence(parsed, input.len());
    let excerpt = allocate_excerpts(&[&f], 13).pop().unwrap();
    assert!(!excerpt.text.contains("已完成检查"));
    for (header, row) in [
        ("项目\t数量", "合计。\t21"),
        ("项目 | 数量\n--- | ---", "合计。 | 21"),
    ] {
        let input = format!(
            "说明。\n{}\n单位：台\n\n{header}\n{row}",
            "附注。\n".repeat(20)
        );
        let parsed = parse(&dir.path().join("columns.md"), input.as_bytes(), 2000, 55).unwrap();
        assert!(parsed
            .content
            .contains(&format!("单位：台\n{header}\n{row}")));
        parsed.evidence_audit.validate(&parsed.content).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn native_text_scan_and_cache_replay_keep_semantics_and_coordinates() {
    use ai_daily_scanner_contract::BuildContextRequest;
    use ai_daily_scanner_core::{ScanRequest, Scanner, ScannerConfig};
    use serde_json::json;

    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();
    fs::write(work.join("sample.md"), long_document()).unwrap();
    let workers = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        });
    let scan = || {
        let request: BuildContextRequest = serde_json::from_value(json!({
            "contract": "ai_daily_context", "protocol_version": 1,
            "request_id": "71444444-7144-4144-8144-714444444444",
            "work_dir": work, "scan_db_path": temp.path().join(ai_daily_scanner_core::store::SCAN_DB_FILENAME),
            "start_date": "2000-01-01", "end_date": "2099-12-31", "report_mode": "daily",
            "compression_profile": null,
            "scanner_settings": {"allowed_extensions": [".md"], "max_workers": 1,
                "direct_text_read_bytes": 1200, "text_max_chars": 300, "total_max_chars": 160},
            "adapters": {"office_worker_path": workers.join("ai-daily-office-parser.exe"),
                "pdf_worker_path": workers.join("ai-daily-pdf-parser.exe")}
        })).unwrap();
        let scanner = Scanner::open(ScannerConfig::from_build_request(&request)).unwrap();
        let result = scanner
            .build_context(&ScanRequest::from_build_request(&request))
            .unwrap();
        assert_eq!(result.exit_code, 0, "{:?}", result.value.envelope.error);
        result.value
    };
    let cold = scan();
    assert_eq!(
        cold.evidence.unwrap().execution_metrics.parse_attempt_count,
        1
    );
    assert_eq!(cold.sources.len(), 1);
    let source = &cold.sources[0];
    assert!(source.parse_truncated && source.excerpt_truncated);
    for fact in [OPENING, MIDDLE, CONDITION, ENDING] {
        assert!(source.excerpt.contains(fact));
    }
    let audit = &source.coverage_limits["evidence_audit"];
    assert_eq!(audit["read_ranges"].as_array().unwrap().len(), 3);
    assert!(audit["excerpt_ranges"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["unit"] == "byte"));
    let warm = scan();
    assert_eq!(
        warm.evidence.unwrap().execution_metrics.parse_attempt_count,
        0
    );
    assert_eq!(source.excerpt, warm.sources[0].excerpt);
    assert_eq!(source.coverage_limits, warm.sources[0].coverage_limits);
    assert_eq!(
        audit["omitted_ranges"]
            .as_array()
            .expect("native source must publish the computed omission ranges")
            .iter()
            .filter(|g| g["label"] == "unread")
            .count(),
        2
    );
}
