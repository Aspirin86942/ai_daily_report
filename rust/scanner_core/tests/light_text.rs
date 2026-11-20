use ai_daily_scanner_contract::TextParseProfile;
use ai_daily_scanner_core::parsers::light_text::{
    parse_light_text, LightTextError, LightTextWarning,
};
use std::fs;
use tempfile::tempdir;

#[test]
fn head_read_trims_only_an_incomplete_utf8_tail() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("中文 preview.txt");
    fs::write(&path, "ab\n中cd").expect("fixture should be written");
    let profile = text_profile(5, 8, 200);

    let parsed = parse_light_text(&path, ".txt", &profile, 1024)
        .expect("split UTF-8 tail should be trimmed");

    assert!(parsed.truncated);
    assert_eq!(parsed.parser_backend, "light_text_v2");
    assert!(parsed.content.contains("excerpt_source: head"));
    assert!(parsed.content.ends_with("ab"));
    assert!(!parsed.content.contains('\u{fffd}'));
}

#[test]
fn invalid_utf8_inside_the_read_window_is_an_explicit_error() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("invalid.txt");
    fs::write(&path, b"good\xffbad").expect("fixture should be written");

    let error = parse_light_text(&path, ".txt", &text_profile(64, 64, 200), 1024)
        .expect_err("interior invalid UTF-8 must fail");

    assert_eq!(error, LightTextError::DecodeFailed);
}

#[test]
fn csv_strict_encodings_preserve_amounts_and_quoted_multiline_records() {
    let dir = tempdir().unwrap();
    let text = "项目,金额\n\"𠀀\n采购\",1234.56\n";
    let gb18030 =
        b"\xcf\xee\xc4\xbf,\xbd\xf0\xb6\xee\n\"\x95\x32\x82\x36\n\xb2\xc9\xb9\xba\",1234.56\n";
    let mut profile = text_profile(1024, 1024, 6000);
    profile.csv_encodings.push("gb18030".into());
    for (encoding, data) in [
        ("utf-8", text.as_bytes().to_vec()),
        (
            "utf-8-sig",
            [b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat(),
        ),
        ("gb18030", gb18030.to_vec()),
    ] {
        let path = dir.path().join(format!("{encoding}.csv"));
        fs::write(&path, data).unwrap();
        let result = parse_light_text(&path, ".csv", &profile, 1024).unwrap();
        assert!(result.content.contains("1234.56"));
        assert!(result.content.contains("𠀀"));
        assert!(result.content.contains("采购"));
        assert!(!result.content.contains('\u{fffd}'));
        assert_eq!(result.evidence_audit.encoding.as_deref(), Some(encoding));
        let ranges = result.evidence_audit.read_ranges.unwrap();
        assert_eq!(
            ranges
                .iter()
                .filter(|range| range.unit == ai_daily_worker_contract::SourceRangeUnit::CsvRecord)
                .count(),
            2
        );
    }
    let path = dir.path().join("disabled.csv");
    fs::write(&path, gb18030).unwrap();
    assert_eq!(
        parse_light_text(&path, ".csv", &text_profile(1024, 1024, 6000), 1024),
        Err(LightTextError::DecodeFailed)
    );
}

#[test]
fn gb18030_window_cut_and_invalid_interior_never_produce_partial_records() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cut.csv");
    let bytes = b"name,amount\nfirst,1234.56\n\x95\x32\x82\x36,9999.99\n";
    fs::write(&path, bytes).unwrap();
    let start = bytes.iter().position(|value| *value == 0x95).unwrap();
    for cut in 1..=3 {
        let mut profile = text_profile((start + cut) as u64, 1024, 6000);
        profile.csv_encodings = vec!["gb18030".into()];
        let result = parse_light_text(&path, ".csv", &profile, 1024).unwrap();
        assert!(result.truncated);
        assert!(result.content.contains("1234.56"));
        assert!(!result.content.contains("9999"));
        assert!(!result.content.contains('\u{fffd}'));
    }
    let mut profile = text_profile(1024, 1024, 6000);
    profile.csv_encodings.push("gb18030".into());
    for bytes in [
        b"name,amount\nfirst,\xff123\n".as_slice(),
        b"\xef\xbb\xbfname,amount\nfirst,\xff123\n".as_slice(),
    ] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            parse_light_text(&path, ".csv", &profile, 1024),
            Err(LightTextError::DecodeFailed)
        );
    }
}

#[test]
fn log_tail_read_proves_and_trims_a_split_leading_character() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("service.log");
    fs::write(&path, "prefix中TAIL").expect("fixture should be written");

    let parsed = parse_light_text(&path, ".log", &text_profile(64, 6, 200), 1024)
        .expect("split UTF-8 head should be trimmed");

    assert!(parsed.truncated);
    assert!(parsed.content.contains("excerpt_source: tail"));
    assert!(parsed.content.ends_with("TAIL"));
}

#[test]
fn log_tail_does_not_hide_an_unproven_invalid_leading_byte() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("invalid.log");
    fs::write(&path, b"prefix\x80TAIL").expect("fixture should be written");

    let error = parse_light_text(&path, ".log", &text_profile(64, 5, 200), 1024)
        .expect_err("unproven continuation byte must remain invalid");

    assert_eq!(error, LightTextError::DecodeFailed);
}

#[test]
fn json_and_csv_previews_keep_the_existing_auditable_shape() {
    let directory = tempdir().expect("temporary root should exist");
    let json_path = directory.path().join("sample.json");
    let csv_path = directory.path().join("sample.csv");
    fs::write(&json_path, r#"{"z":1,"a":2}"#).expect("JSON fixture should be written");
    fs::write(&csv_path, "name,value\n\"甲,乙\",2\n").expect("CSV fixture should be written");
    let profile = text_profile(1024, 1024, 1000);

    let json = parse_light_text(&json_path, ".json", &profile, 4096).expect("JSON should parse");
    let csv = parse_light_text(&csv_path, ".csv", &profile, 4096).expect("CSV should parse");

    assert!(json.content.starts_with("JSON object preview\n"));
    assert!(json.content.contains("top_level_keys: a, z"));
    assert!(csv.content.starts_with("CSV preview\n"));
    assert!(csv.content.contains("header: name | value"));
    assert!(csv.content.contains("row 1: 甲,乙 | 2"));
}

#[test]
fn truncated_json_and_malformed_csv_use_explicit_preview_fallbacks() {
    let directory = tempdir().expect("temporary root should exist");
    let json_path = directory.path().join("large.json");
    let csv_path = directory.path().join("broken.csv");
    fs::write(&json_path, r#"{"message":"long"}"#).expect("JSON fixture should be written");
    fs::write(&csv_path, "name,value\n\"unterminated,2\n").expect("CSV fixture should be written");

    let json = parse_light_text(&json_path, ".json", &text_profile(8, 8, 500), 4096)
        .expect("truncated JSON should remain an auditable preview");
    let csv = parse_light_text(&csv_path, ".csv", &text_profile(1024, 1024, 500), 4096)
        .expect("malformed CSV should remain an auditable preview");

    assert!(json.truncated);
    assert!(json.content.contains("warning: JSON_PREVIEW_FALLBACK"));
    assert_eq!(json.warnings, [LightTextWarning::JsonPreviewFallback]);
    assert!(csv.content.contains("warning: CSV_PREVIEW_FALLBACK"));
    assert_eq!(csv.warnings, [LightTextWarning::CsvPreviewFallback]);
}

#[test]
fn csv_boundaries_match_the_legacy_python_reader() {
    let directory = tempdir().expect("temporary root should exist");
    let legal_path = directory.path().join("legal.csv");
    let bare_cr_path = directory.path().join("bare-cr.csv");
    fs::write(&legal_path, "a\"b,c\n").expect("legal CSV fixture should be written");
    fs::write(&bare_cr_path, "a,b\rc,d").expect("bare-CR fixture should be written");
    let profile = text_profile(1024, 1024, 500);

    let legal = parse_light_text(&legal_path, ".csv", &profile, 4096)
        .expect("quote inside an unquoted field is legal in the legacy parser");
    let bare_cr = parse_light_text(&bare_cr_path, ".csv", &profile, 4096)
        .expect("malformed CSV should remain an auditable preview");

    assert!(legal.content.is_empty());
    assert_eq!(legal.evidence_audit.usable_chars, Some(0));
    assert!(legal
        .evidence_audit
        .read_ranges
        .as_ref()
        .unwrap()
        .iter()
        .any(
            |range| range.unit == ai_daily_worker_contract::SourceRangeUnit::CsvRecord
                && range.start == 1
        ));
    assert!(legal.warnings.is_empty());
    assert_eq!(bare_cr.warnings, [LightTextWarning::CsvPreviewFallback]);
}

#[test]
fn json_python_extensions_and_arbitrary_numbers_keep_legacy_types() {
    let directory = tempdir().expect("temporary root should exist");
    let cases = [
        ("nan.json", "NaN", "top_level_type: float"),
        ("infinite.json", "1e400", "top_level_type: float"),
        (
            "large-int.json",
            "999999999999999999999999999999999999999999",
            "top_level_type: int",
        ),
        ("nested.json", "[NaN]", "top_level_type: list"),
    ];
    let profile = text_profile(1024, 1024, 500);

    for (name, content, expected_type) in cases {
        let path = directory.path().join(name);
        fs::write(&path, content).expect("JSON fixture should be written");
        let parsed = parse_light_text(&path, ".json", &profile, 4096)
            .expect("legacy-compatible JSON should parse");

        assert!(
            parsed.content.contains(expected_type),
            "unexpected preview for {content}: {}",
            parsed.content
        );
        assert!(parsed.warnings.is_empty(), "{content} must not fallback");
    }
}

#[test]
fn malformed_unicode_json_falls_back_without_panicking() {
    let directory = tempdir().expect("temporary root should exist");
    let profile = text_profile(1024, 1024, 500);
    for (name, content) in [("bare.json", "中"), ("bom.json", "\u{feff}{}")] {
        let path = directory.path().join(name);
        fs::write(&path, content).expect("malformed JSON fixture should be written");

        let parsed = parse_light_text(&path, ".json", &profile, 4096)
            .expect("malformed Unicode JSON should remain an auditable preview");

        assert_eq!(parsed.warnings, [LightTextWarning::JsonPreviewFallback]);
    }
}

#[test]
fn csv_field_size_boundary_matches_the_legacy_python_limit() {
    let directory = tempdir().expect("temporary root should exist");
    let accepted_path = directory.path().join("accepted.csv");
    let rejected_path = directory.path().join("rejected.csv");
    fs::write(&accepted_path, format!("{},b\n", "a".repeat(131_072)))
        .expect("accepted CSV fixture should be written");
    fs::write(&rejected_path, format!("{},b\n", "a".repeat(131_073)))
        .expect("rejected CSV fixture should be written");
    let profile = text_profile(200_000, 200_000, 500);

    let accepted = parse_light_text(&accepted_path, ".csv", &profile, 300_000)
        .expect("legacy boundary field should parse");
    let rejected = parse_light_text(&rejected_path, ".csv", &profile, 300_000)
        .expect("oversize CSV field should remain an auditable preview");

    assert!(accepted.warnings.is_empty());
    assert_eq!(rejected.warnings, [LightTextWarning::CsvPreviewFallback]);
}

#[test]
fn output_budget_and_large_file_guard_are_enforced_before_parse() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("bounded.md");
    fs::write(&path, "0123456789").expect("fixture should be written");
    let profile = text_profile(1024, 1024, 20);

    let parsed = parse_light_text(&path, ".md", &profile, 1024).expect("small file should parse");
    let error = parse_light_text(&path, ".md", &profile, 9)
        .expect_err("large file must be rejected before content parsing");

    assert!(parsed.truncated);
    assert!(parsed.content.chars().count() <= 20);
    assert_eq!(error, LightTextError::FileTooLarge);
}

#[test]
fn markdown_table_is_a_complete_unit_with_original_cell_ranges() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("stats.md");
    let text = "# 验收数字\n| 指标 | 值 |\n| --- | --- |\n| 组数 | 41 |\n| 金额档 | 95 |\n| 覆盖节点 | 167 |\n| 空组合 | 32 |\n";
    fs::write(&path, text).unwrap();
    let parsed = parse_light_text(&path, ".md", &text_profile(4096, 4096, 1000), 4096).unwrap();
    parsed.evidence_audit.validate(&parsed.content).unwrap();
    let chars = parsed.content.chars().collect::<Vec<_>>();
    let units = parsed.evidence_audit.content_units.unwrap();
    let table = units
        .iter()
        .find(|unit| unit.kind == ai_daily_worker_contract::ContentUnitKind::TableRow)
        .unwrap();
    let rendered = chars[table.text_start as usize..table.text_end as usize]
        .iter()
        .collect::<String>();
    for value in ["41", "95", "167", "32"] {
        assert!(rendered.contains(value));
    }
    assert_eq!(table.ranges.len(), 6);
    assert!(table
        .ranges
        .iter()
        .all(|range| range.end <= text.chars().count() as u64));
}

#[test]
fn cramped_csv_keeps_complete_tail_totals_actions_negatives_and_their_unit() {
    use ai_daily_worker_contract::{ContentUnitKind, SourceRangeUnit};
    let dir = tempdir().unwrap();
    let path = dir.path().join("ledger.csv");
    let mut data = String::from("项目,金额\n单位,万元\n");
    for i in 1..=35 {
        data += &format!("采购项目{i}{},100.00\n", "附录".repeat(30));
    }
    data += "\"返工\n退款\",-1234.56\n异常,45.6\n下一步,11\n合计,6789.01\n已签收,7.80\n";
    fs::write(&path, &data).unwrap();
    let parsed = parse_light_text(&path, ".csv", &text_profile(16_000, 1024, 166), 16_000).unwrap();
    assert!(parsed.truncated);
    assert!(parsed.content.chars().count() <= 166 && parsed.warnings.is_empty());
    parsed.evidence_audit.validate(&parsed.content).unwrap();
    let units = parsed.evidence_audit.content_units.as_ref().unwrap();
    let chars: Vec<_> = parsed.content.chars().collect();
    for (record, expected, kind) in [
        (1, "header: 项目 | 金额", ContentUnitKind::TableHeader),
        (2, "row 1: 单位 | 万元", ContentUnitKind::Unit),
        (
            38,
            "row 37: 返工\n退款 | -1234.56",
            ContentUnitKind::TableRow,
        ),
        (39, "row 38: 异常 | 45.6", ContentUnitKind::TableRow),
        (40, "row 39: 下一步 | 11", ContentUnitKind::TableRow),
        (41, "row 40: 合计 | 6789.01", ContentUnitKind::Total),
        (42, "row 41: 已签收 | 7.80", ContentUnitKind::TableRow),
    ] {
        let unit = units
            .iter()
            .find(|u| {
                u.ranges.iter().any(|r| {
                    r.unit == SourceRangeUnit::CsvRecord && (r.start, r.end) == (record, record + 1)
                })
            })
            .unwrap_or_else(|| panic!("record {record}: {}", parsed.content));
        assert_eq!(unit.kind, kind);
        assert_eq!(
            chars[unit.text_start as usize..unit.text_end as usize]
                .iter()
                .collect::<String>(),
            expected
        );
    }
    assert_eq!(
        parsed.evidence_audit.read_ranges.as_ref().unwrap().len(),
        43
    );
}

#[test]
fn plentiful_budget_keeps_more_than_512_csv_records_with_exact_coverage() {
    use ai_daily_worker_contract::SourceRangeUnit;
    let dir = tempdir().unwrap();
    let path = dir.path().join("ledger.csv");
    let mut data = String::from("单位,金额\n");
    for i in 1..=619 {
        data += &format!("采购{i},{i}.02\n");
    }
    data += "合计,620.02\n";
    fs::write(&path, &data).unwrap();
    let parsed =
        parse_light_text(&path, ".csv", &text_profile(65_536, 1024, 50_000), 65_536).unwrap();
    assert!(!parsed.truncated);
    assert_eq!(parsed.content.matches("\nrow ").count(), 620);
    assert!(parsed.content.ends_with("row 620: 合计 | 620.02"));
    let audit = &parsed.evidence_audit;
    audit.validate(&parsed.content).unwrap();
    let total = audit.content_units.as_ref().unwrap().last().unwrap();
    assert_eq!(total.kind, ai_daily_worker_contract::ContentUnitKind::Total);
    assert_eq!((total.ranges[0].start, total.ranges[0].end), (621, 622));
    let records: Vec<_> = audit
        .content_units
        .as_ref()
        .unwrap()
        .iter()
        .flat_map(|u| &u.ranges)
        .filter(|r| r.unit == SourceRangeUnit::CsvRecord)
        .flat_map(|r| r.start..r.end)
        .collect();
    assert_eq!(records, (1..=621).collect::<Vec<_>>());
    assert!(audit
        .read_ranges
        .as_ref()
        .unwrap()
        .iter()
        .any(|r| r.unit == SourceRangeUnit::CsvRecord && (r.start, r.end) == (1, 622)));
}

#[test]
fn cut_window_keeps_unquoted_literal_quote_and_excludes_unfinished_quoted_record() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cut.csv");
    let complete = "字段,值\nab\"cd,-1.25\n";
    fs::write(&path, format!("{complete}\"第一行\n未完\",999\n")).unwrap();
    let parsed = parse_light_text(
        &path,
        ".csv",
        &text_profile((complete.len() + 11) as u64, 1024, 4096),
        4096,
    )
    .unwrap();
    assert!(parsed.warnings.is_empty() && parsed.truncated);
    assert!(parsed.content.ends_with("row 1: ab\"cd | -1.25"));
    assert!(!parsed.content.contains("第一行"));
}

#[test]
fn bounded_text_keeps_late_action_and_precise_source_characters() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("notes.md");
    let mut input = String::from("# 差额复核\n");
    for i in 1..=30 {
        input += &format!("说明{i}{}。\n", "背景".repeat(30));
    }
    let fact = "下一步核对－1234.56万元，含𠀀项目。";
    input += fact;
    fs::write(&path, &input).unwrap();
    let parsed = parse_light_text(&path, ".md", &text_profile(16_000, 1024, 190), 16_000).unwrap();
    assert!(parsed.truncated && parsed.content.contains("# 差额复核"));
    assert!(parsed.content.contains(fact));
    parsed.evidence_audit.validate(&parsed.content).unwrap();
    let chars: Vec<_> = input.chars().collect();
    assert!(parsed
        .evidence_audit
        .content_units
        .unwrap()
        .iter()
        .flat_map(|u| &u.ranges)
        .any(|r| chars[r.start as usize..r.end as usize]
            .iter()
            .collect::<String>()
            == fact));
}

#[test]
fn tight_csv_keeps_both_amount_and_quantity_units_with_the_total() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("quantities.csv");
    let input = format!(
        "项目,金额,数量\n金额单位,万元,\n计量单位,件,\n{},5,6\n合计,-12.30,17\n",
        "日常说明".repeat(100)
    );
    fs::write(&path, input).unwrap();
    let parsed = parse_light_text(&path, ".csv", &text_profile(4096, 1024, 90), 4096).unwrap();
    assert!(parsed.truncated);
    for line in [
        "header: 项目 | 金额 | 数量",
        "金额单位 | 万元",
        "计量单位 | 件",
        "合计 | -12.30 | 17",
    ] {
        assert!(parsed.content.contains(line), "{}", parsed.content);
    }
    assert!(!parsed.content.contains("日常说明"));
    parsed.evidence_audit.validate(&parsed.content).unwrap();
}

#[test]
fn standalone_unit_and_company_amounts_keep_their_distinct_record_numbers() {
    use ai_daily_worker_contract::{ContentUnitKind, SourceRangeUnit};
    let dir = tempdir().unwrap();
    let path = dir.path().join("companies.csv");
    std::fs::write(
        &path,
        "公司,金额\n单位：万元,\n单位：甲公司,100\n单位：乙公司,200\n",
    )
    .unwrap();
    let parsed = parse_light_text(&path, ".csv", &text_profile(1024, 1024, 1000), 1024).unwrap();
    let chars: Vec<_> = parsed.content.chars().collect();
    parsed.evidence_audit.validate(&parsed.content).unwrap();
    let expected = [
        (1, ContentUnitKind::TableHeader, "header: 公司 | 金额"),
        (2, ContentUnitKind::Unit, "row 1: 单位：万元 |"),
        (3, ContentUnitKind::TableRow, "row 2: 单位：甲公司 | 100"),
        (4, ContentUnitKind::TableRow, "row 3: 单位：乙公司 | 200"),
    ];
    let units: Vec<_> = parsed
        .evidence_audit
        .content_units
        .unwrap()
        .into_iter()
        .filter(|u| u.kind != ContentUnitKind::Heading)
        .collect();
    assert_eq!(units.len(), expected.len());
    for (unit, (record, kind, line)) in units.iter().zip(expected) {
        assert_eq!(unit.kind, kind);
        assert_eq!(unit.ranges.len(), 1);
        let r = &unit.ranges[0];
        assert_eq!(
            (r.unit, r.start, r.end),
            (SourceRangeUnit::CsvRecord, record, record + 1)
        );
        assert_eq!(
            chars[unit.text_start as usize..unit.text_end as usize]
                .iter()
                .collect::<String>(),
            line
        );
    }
    assert!(!parsed.truncated);
}

#[test]
fn tight_sectioned_csv_total_uses_the_current_quantity_header() {
    use ai_daily_worker_contract::ContentUnitKind;
    let dir = tempdir().unwrap();
    let path = dir.path().join("sections.csv");
    std::fs::write(&path, "项目,金额(元)\n甲,10\n项目,数量(台)\n合计,8\n").unwrap();
    let expected = "header: 项目 | 数量(台)\nrow 3: 合计 | 8";
    let parsed = parse_light_text(
        &path,
        ".csv",
        &text_profile(1024, 1024, expected.chars().count() as u64),
        1024,
    )
    .unwrap();
    assert_eq!(parsed.content, expected);
    assert!(parsed.truncated);
    parsed.evidence_audit.validate(&parsed.content).unwrap();
    let units = parsed.evidence_audit.content_units.unwrap();
    assert_eq!(
        units
            .iter()
            .map(|u| (u.kind, u.ranges[0].start, u.ranges[0].end))
            .collect::<Vec<_>>(),
        [
            (ContentUnitKind::TableHeader, 3, 4),
            (ContentUnitKind::Total, 4, 5)
        ]
    );
}

fn text_profile(read_head_bytes: u64, read_tail_bytes: u64, max_chars: u64) -> TextParseProfile {
    TextParseProfile {
        csv_encodings: ai_daily_scanner_contract::default_csv_encodings(),
        backend: "light_text_v2".to_string(),
        read_head_bytes,
        read_tail_bytes,
        max_chars,
        excerpt_max_chars: max_chars,
    }
}
