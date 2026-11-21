use super::*;
use ai_daily_worker_contract::ContentUnitKind;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn local_office_worker_build_uses_the_shared_source_fingerprint() {
    let build = worker_hello().worker_build;
    if let Some(ci_build) =
        option_env!("AI_DAILY_BUILD_ID").filter(|value| !value.trim().is_empty())
    {
        assert_eq!(build, ci_build);
    } else {
        let digest = build
            .strip_prefix("sha256-source-v1:")
            .expect("local build must use the source hash prefix");
        assert_eq!(digest.len(), 64);
        assert!(digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }
}

fn write_test_xlsx(parts: &[(&str, &str)]) -> PathBuf {
    static NEXT_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    let sequence = NEXT_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "bounded-xlsx-{}-{unique}-{sequence}.xlsx",
        std::process::id()
    ));
    let file = File::create_new(&path).expect("test xlsx should be creatable");
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();

    for (name, body) in parts {
        zip.start_file(name, options)
            .expect("zip entry should be writable");
        zip.write_all(body.as_bytes())
            .expect("zip entry body should be writable");
    }
    zip.finish().expect("zip should finish");
    path
}

fn minimal_xlsx_parts() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Data" sheetId="1" r:id="rId1"/>
    <sheet name="Second" sheetId="2" r:id="rId2"/>
  </sheets>
</workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="worksheet" Target="worksheets/sheet2.xml"/>
</Relationships>"#,
        ),
        (
            "xl/sharedStrings.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <si><t>Alpha</t></si>
  <si><t>unused-over-budget</t></si>
</sst>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><t>Name</t></is></c>
      <c r="B1" t="inlineStr"><is><t>Amount</t></is></c>
      <c r="C1" t="inlineStr"><is><t>hidden-over-budget</t></is></c>
    </row>
    <row r="2">
      <c r="A2" t="s"><v>0</v></c>
      <c r="B2"><v>10</v></c>
    </row>
    <row r="3">
      <c r="A3" t="inlineStr"><is><t>row-over-budget</t></is></c>
    </row>
  </sheetData>
</worksheet>"#,
        ),
        (
            "xl/worksheets/sheet2.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>second-sheet-over-budget</t></is></c></row>
  </sheetData>
</worksheet>"#,
        ),
    ]
}

fn bounded_test_request(path: &Path) -> OfficeParseRequest {
    OfficeParseRequest {
        file_path: path.to_path_buf(),
        file_type: ".xlsx".into(),
        limits: BTreeMap::from([
            ("excel_max_sheets".into(), serde_json::json!(1)),
            ("excel_max_rows".into(), serde_json::json!(5)),
            ("excel_max_columns".into(), serde_json::json!(2)),
            ("document_excerpt_max_chars".into(), serde_json::json!(2000)),
        ]),
        parser_backend: RUST_OFFICE_BACKEND.into(),
    }
}

#[test]
fn lowercase_xlsx_parts_resolve_shared_strings_with_original_row_limits() {
    let mut parts = minimal_xlsx_parts();
    parts[2].0 = "xl/sharedstrings.xml";
    parts[3].0 = "xl/worksheets/SHEET1.XML";
    let path = write_test_xlsx(&parts);
    let mut request = bounded_test_request(&path);
    request
        .limits
        .insert("excel_max_rows".into(), serde_json::json!(2));
    let parsed = parse_office_file(&request);
    std::fs::remove_file(path).ok();
    assert!(parsed.error.is_none());
    assert!(parsed.content.contains("| Alpha | 10 |"));
    assert!(!parsed.content.contains("#SHARED_STRING"));
    assert!(!parsed.content.contains("row-over-budget"));
    assert_eq!(parsed.evidence_audit.unwrap().read_ranges.unwrap().len(), 2);
}

#[test]
fn missing_and_ambiguous_shared_strings_fail_without_placeholder_content() {
    for ambiguous in [false, true] {
        let mut parts = minimal_xlsx_parts();
        if ambiguous {
            parts.push(("xl/sharedstrings.xml", parts[2].1));
        } else {
            parts.remove(2);
        }
        let path = write_test_xlsx(&parts);
        let parsed = parse_office_file(&bounded_test_request(&path));
        std::fs::remove_file(path).ok();
        let error = parsed.error.unwrap();
        assert!(error.contains(if ambiguous {
            "ambiguous case"
        } else {
            "missing"
        }));
        assert!(parsed.content.is_empty());
        assert!(parsed.evidence_audit.is_none());
    }
}

#[test]
fn missing_referenced_shared_string_fails_instead_of_returning_usable_placeholder() {
    let original = minimal_xlsx_parts();
    let invalid_sheet = original[3].1.replace("<v>0</v>", "<v>9</v>");
    let parts: Vec<_> = original
        .iter()
        .enumerate()
        .map(|(index, (name, body))| {
            (
                *name,
                if index == 3 {
                    invalid_sheet.as_str()
                } else {
                    *body
                },
            )
        })
        .collect();
    let path = write_test_xlsx(&parts);
    let parsed = parse_office_file(&bounded_test_request(&path));
    std::fs::remove_file(path).ok();
    assert!(parsed
        .error
        .unwrap()
        .contains("referenced shared string is missing"));
    assert!(parsed.content.is_empty());
}

#[test]
fn header_with_total_column_stays_header_and_vertical_sum_keeps_cached_value() {
    let mut parts = minimal_xlsx_parts();
    parts[3].1 = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>姓名</t></is></c><c r="B1" t="inlineStr"><is><t>代扣代缴合计</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>员工甲</t></is></c><c r="B2"><v>20.25</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>员工乙</t></is></c><c r="B3"><v>30.5</v></c></row>
          <row r="4"><c r="B4"><f>SUM(B2:B3)</f><v>50.75</v></c></row>
          <row r="5"><c r="A5"><v>2</v></c><c r="B5"><f>SUM(A5:A5)</f><v>2</v></c></row>
        </sheetData></worksheet>"#;
    let path = write_test_xlsx(&parts);
    let parsed = parse_office_file(&bounded_test_request(&path));
    std::fs::remove_file(path).ok();
    assert!(parsed.error.is_none());
    let audit = parsed.evidence_audit.unwrap();
    let units = audit.content_units.unwrap();
    assert!(units
        .iter()
        .filter(|u| u.ranges.first().is_some_and(|r| r.start == 1))
        .all(|u| u.kind == ContentUnitKind::TableHeader));
    assert!(units
        .iter()
        .any(|u| u.kind == ContentUnitKind::TableRow
            && u.ranges.first().is_some_and(|r| r.start == 2)));
    let total = units
        .iter()
        .find(|u| {
            u.kind == ContentUnitKind::Total
                && u.ranges.first().is_some_and(|range| range.start == 4)
        })
        .unwrap();
    assert_eq!(total.ranges[0].start, 4);
    assert!(parsed
        .content
        .chars()
        .skip(total.text_start as usize)
        .take((total.text_end - total.text_start) as usize)
        .collect::<String>()
        .contains("50.75"));
    assert!(units
        .iter()
        .any(|u| u.kind == ContentUnitKind::TableRow
            && u.ranges.first().is_some_and(|r| r.start == 5)));
}

#[test]
fn xlsx_character_budget_keeps_later_sheet_data_and_total_before_bulk_rows() {
    let original = minimal_xlsx_parts();
    let bulk = (2..=35).map(|row| format!(r#"<row r="{row}"><c r="A{row}" t="inlineStr"><is><t>条目{row}附带较长的常规说明</t></is></c><c r="B{row}"><v>10</v></c></row>"#)).collect::<String>();
    let first = format!(
        r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>{bulk}</sheetData></worksheet>"#
    );
    let second = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>另一表甲</t></is></c><c r="B2"><v>1234.56</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>另一表乙</t></is></c><c r="B3"><v>20.25</v></c></row>
          <row r="4"><c r="B4"><f>SUM(B2:B3)</f><v>1254.81</v></c></row>
        </sheetData></worksheet>"#;
    let parts = [
        original[0],
        original[1],
        original[2],
        (original[3].0, first.as_str()),
        (original[4].0, second),
    ];
    let path = write_test_xlsx(&parts);
    let mut request = bounded_test_request(&path);
    request
        .limits
        .insert("excel_max_sheets".into(), serde_json::json!(2));
    request
        .limits
        .insert("excel_max_rows".into(), serde_json::json!(50));
    request
        .limits
        .insert("document_excerpt_max_chars".into(), serde_json::json!(600));
    let parsed = parse_office_file(&request);
    std::fs::remove_file(path).ok();
    assert!(parsed.error.is_none() && parsed.truncated, "{parsed:?}");
    assert!(parsed.content.chars().count() <= 600);
    assert!(parsed.content.contains("## Sheet: Second"));
    assert!(parsed.content.contains("1234.56") && parsed.content.contains("1254.81"));
    let audit = parsed.evidence_audit.unwrap();
    audit.validate(&parsed.content).unwrap();
    assert!(audit
        .content_units
        .unwrap()
        .iter()
        .any(|unit| unit.kind == ContentUnitKind::Total
            && unit
                .ranges
                .iter()
                .any(|range| range.label.as_deref() == Some("Second") && range.start == 4)));
}

#[test]
fn xlsx_window_summary_states_only_read_rows_and_repeated_values() {
    let original = minimal_xlsx_parts();
    let sheet = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>月份</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>2024年1月</t></is></c><c r="B2"><v>2360</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>2024年2月</t></is></c><c r="B3"><v>2360</v></c></row>
          <row r="4"><c r="A4" t="inlineStr"><is><t>2024年3月</t></is></c><c r="B4"><v>9999</v></c></row>
        </sheetData></worksheet>"#;
    let parts = [
        original[0],
        original[1],
        original[2],
        (original[3].0, sheet),
        original[4],
    ];
    let path = write_test_xlsx(&parts);
    let mut request = bounded_test_request(&path);
    request
        .limits
        .insert("excel_max_rows".into(), serde_json::json!(3));
    let parsed = parse_office_file(&request);
    std::fs::remove_file(path).ok();
    assert!(parsed.error.is_none() && parsed.truncated);
    assert!(parsed.content.contains("已读数据行 2 条"));
    assert!(parsed.content.contains("这些行的共同值：Amount = 2360"));
    assert!(parsed.content.contains("逐行 月份 = 2024年1月、2024年2月"));
    assert!(!parsed.content.contains("9999") && !parsed.content.contains("2024年3月"));
    let audit = parsed.evidence_audit.unwrap();
    audit.validate(&parsed.content).unwrap();
    assert!(audit
        .content_units
        .unwrap()
        .iter()
        .any(|unit| unit.kind == ContentUnitKind::TableRow
            && unit
                .ranges
                .iter()
                .any(|range| range.start == 2 && range.end == 4)));
}

#[test]
fn small_workbook_keeps_every_source_row_before_optional_statistics() {
    let mut parts = minimal_xlsx_parts();
    parts[0].1 = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#;
    parts[3].1 = r#"<worksheet><sheetData>
          <row><c t="str"><v>Name</v></c><c t="str"><v>Amount</v></c></row>
          <row><c t="str"><v>A</v></c><c><v>10</v></c></row>
          <row><c t="str"><v>B</v></c><c><v>20</v></c></row>
          <row><c t="str"><v>C</v></c><c><v>30</v></c></row>
        </sheetData></worksheet>"#;
    let path = write_test_xlsx(&parts);
    let mut request = bounded_test_request(&path);
    request
        .limits
        .insert("document_excerpt_max_chars".into(), 105.into());
    let parsed = parse_office_file(&request);
    std::fs::remove_file(path).unwrap();
    assert!(parsed.error.is_none() && !parsed.truncated);
    for line in [
        "| Name | Amount |",
        "| A | 10 |",
        "| B | 20 |",
        "| C | 30 |",
    ] {
        assert!(parsed.content.contains(line), "{}", parsed.content);
    }
}

#[test]
fn supported_office_type_is_case_insensitive() {
    assert!(is_supported_office_type(".DOCX"));
    assert!(is_supported_office_type(".xlsx"));
    assert!(is_supported_office_type(".PPT"));
    assert!(!is_supported_office_type(".pdf"));
}

#[test]
fn formula_zero_padding_does_not_hide_months_or_a_named_zero_amount() {
    let original = minimal_xlsx_parts();
    let sheet = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>月份</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>2024年1月</t></is></c><c r="B2"><v>2360</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>2024年2月</t></is></c><c r="B3"><v>2360</v></c></row>
          <row r="4"><c r="B4"><f>SUM(C4:H4)</f><v>0</v></c></row>
          <row r="5"><c r="A5" t="inlineStr"><is><t>2024年3月</t></is></c><c r="B5"><v>0</v></c></row>
        </sheetData></worksheet>"#;
    let parts = [
        original[0],
        original[1],
        original[2],
        (original[3].0, sheet),
        original[4],
    ];
    let path = write_test_xlsx(&parts);
    let parsed = parse_office_file(&bounded_test_request(&path));
    std::fs::remove_file(path).ok();
    assert!(parsed.error.is_none());
    assert!(parsed.content.contains("已读数据行 3 条"));
    assert!(parsed.content.contains("| 2024年3月 | 0 |"));
    let audit = parsed.evidence_audit.unwrap();
    audit.validate(&parsed.content).unwrap();
    assert!(audit.read_ranges.unwrap().iter().any(|r| r.start == 4));
    assert!(!audit
        .content_units
        .unwrap()
        .iter()
        .any(|unit| unit.kind == ContentUnitKind::TableRow
            && unit.ranges.iter().any(|r| r.start == 4)));
}

#[test]
fn repeated_names_get_separate_bounded_summaries_with_nonadjacent_ranges() {
    let original = minimal_xlsx_parts();
    let sheet = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c><c r="B1" t="inlineStr"><is><t>月份</t></is></c><c r="C1" t="inlineStr"><is><t>Amount</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>Alpha</t></is></c><c r="B2" t="inlineStr"><is><t>2024-01</t></is></c><c r="C2"><v>2360</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>Beta</t></is></c><c r="B3" t="inlineStr"><is><t>2024-01</t></is></c><c r="C3"><v>5000</v></c></row>
          <row r="4"><c r="A4" t="inlineStr"><is><t>Alpha</t></is></c><c r="B4" t="inlineStr"><is><t>2024-02</t></is></c><c r="C4"><v>2360</v></c></row>
          <row r="5"><c r="A5" t="inlineStr"><is><t>Beta</t></is></c><c r="B5" t="inlineStr"><is><t>2024-02</t></is></c><c r="C5"><v>5000</v></c></row>
          <row r="6"><c r="A6" t="inlineStr"><is><t>Alpha</t></is></c><c r="B6" t="inlineStr"><is><t>2024-03</t></is></c><c r="C6"><v>9999</v></c></row>
        </sheetData></worksheet>"#;
    let parts = [
        original[0],
        original[1],
        original[2],
        (original[3].0, sheet),
        original[4],
    ];
    let path = write_test_xlsx(&parts);
    let mut request = bounded_test_request(&path);
    request
        .limits
        .insert("excel_max_rows".into(), serde_json::json!(5));
    request
        .limits
        .insert("excel_max_columns".into(), serde_json::json!(3));
    let parsed = parse_office_file(&request);
    std::fs::remove_file(path).ok();
    assert!(parsed.error.is_none() && parsed.truncated);
    let audit = parsed.evidence_audit.unwrap();
    audit.validate(&parsed.content).unwrap();
    let chars: Vec<_> = parsed.content.chars().collect();
    let group = audit
        .content_units
        .unwrap()
        .into_iter()
        .find(|unit| {
            unit.kind == ContentUnitKind::TableRow
                && chars[unit.text_start as usize..unit.text_end as usize]
                    .iter()
                    .collect::<String>()
                    .contains("Name = Alpha")
        })
        .unwrap();
    let text: String = chars[group.text_start as usize..group.text_end as usize]
        .iter()
        .collect();
    assert!(text.contains("已读数据行 2 条"));
    assert!(text.contains("逐行 月份 = 2024-01、2024-02"));
    assert!(text.contains("Amount = 2360"));
    assert!(!text.contains("5000") && !parsed.content.contains("9999"));
    assert_eq!(
        group
            .ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect::<Vec<_>>(),
        vec![(2, 3), (4, 5)]
    );
}

#[test]
fn positive_limit_uses_default_for_missing_invalid_or_zero_values() {
    let mut limits = BTreeMap::new();
    limits.insert("good".to_string(), serde_json::json!(12));
    limits.insert("zero".to_string(), serde_json::json!(0));
    limits.insert("text".to_string(), serde_json::json!("bad"));

    assert_eq!(positive_limit(&limits, "good", 6), 12);
    assert_eq!(positive_limit(&limits, "zero", 6), 6);
    assert_eq!(positive_limit(&limits, "text", 6), 6);
    assert_eq!(positive_limit(&limits, "missing", 6), 6);
}

#[test]
fn truncate_content_preserves_utf8_boundaries() {
    let (content, truncated) = truncate_content("甲乙丙丁", 3);

    assert_eq!(content, "甲乙丙");
    assert!(truncated);
}

#[test]
fn unsupported_context_is_file_context_compatible() {
    let request = OfficeParseRequest {
        file_path: PathBuf::from("/tmp/report.pdf"),
        file_type: ".PDF".to_string(),
        limits: BTreeMap::new(),
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
    };

    let context = unsupported_context(&request);

    assert_eq!(context.file_path, "/tmp/report.pdf");
    assert_eq!(context.file_type, ".pdf");
    assert_eq!(
        context.error,
        Some("RUST_OFFICE_UNSUPPORTED_EXTENSION: .pdf".to_string())
    );
    assert_eq!(context.parser_backend, RUST_OFFICE_BACKEND);
    assert!(!context.truncated);
}

#[test]
fn xlsx_uses_bounded_backend_and_respects_sheet_row_column_limits() {
    let path = write_test_xlsx(&minimal_xlsx_parts());
    let mut limits = BTreeMap::new();
    limits.insert("excel_max_sheets".to_string(), serde_json::json!(1));
    limits.insert("excel_max_rows".to_string(), serde_json::json!(2));
    limits.insert("excel_max_columns".to_string(), serde_json::json!(2));
    limits.insert(
        "document_excerpt_max_chars".to_string(),
        serde_json::json!(2000),
    );
    let request = OfficeParseRequest {
        file_path: path.clone(),
        file_type: ".xlsx".to_string(),
        limits,
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
    };

    let context = parse_office_file(&request);

    std::fs::remove_file(path).ok();
    assert_eq!(context.error, None);
    assert_eq!(context.parser_backend, RUST_XLSX_BOUNDED_BACKEND);
    assert!(context.content.contains("# XLSX preview"));
    assert!(context.content.contains("## Sheet: Data"));
    assert!(context.content.contains("| Name | Amount |"));
    assert!(context.content.contains("| Alpha | 10 |"));
    assert!(!context.content.contains("hidden-over-budget"));
    assert!(!context.content.contains("row-over-budget"));
    assert!(!context.content.contains("second-sheet-over-budget"));
    assert!(context.truncated);
}

#[test]
fn xlsx_decodes_inline_string_numeric_character_references() {
    let mut parts = minimal_xlsx_parts();
    parts[3] = (
        "xl/worksheets/sheet1.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><t>&#39033;&#30446;</t></is></c>
      <c r="B1" t="inlineStr"><is><t>&#29366;&#24577;</t></is></c>
    </row>
    <row r="2">
      <c r="A2" t="inlineStr"><is><t>Rust XLSX &#20013;&#25991;</t></is></c>
      <c r="B2" t="inlineStr"><is><t>&#23436;&#25104;</t></is></c>
    </row>
  </sheetData>
</worksheet>"#,
    );
    let path = write_test_xlsx(&parts);
    let mut limits = BTreeMap::new();
    limits.insert("excel_max_sheets".to_string(), serde_json::json!(1));
    limits.insert("excel_max_rows".to_string(), serde_json::json!(2));
    limits.insert("excel_max_columns".to_string(), serde_json::json!(2));
    limits.insert(
        "document_excerpt_max_chars".to_string(),
        serde_json::json!(2000),
    );
    let request = OfficeParseRequest {
        file_path: path.clone(),
        file_type: ".xlsx".to_string(),
        limits,
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
    };

    let context = parse_office_file(&request);

    std::fs::remove_file(path).ok();
    assert_eq!(context.error, None);
    assert_eq!(context.parser_backend, RUST_XLSX_BOUNDED_BACKEND);
    assert!(context.content.contains("| 项目 | 状态 |"));
    assert!(context.content.contains("| Rust XLSX 中文 | 完成 |"));
}

#[test]
fn xlsx_bounded_backend_sets_truncated_when_char_budget_is_exhausted() {
    let path = write_test_xlsx(&minimal_xlsx_parts());
    let mut limits = BTreeMap::new();
    limits.insert("excel_max_sheets".to_string(), serde_json::json!(1));
    limits.insert("excel_max_rows".to_string(), serde_json::json!(2));
    limits.insert("excel_max_columns".to_string(), serde_json::json!(2));
    limits.insert(
        "document_excerpt_max_chars".to_string(),
        serde_json::json!(32),
    );
    let request = OfficeParseRequest {
        file_path: path.clone(),
        file_type: ".xlsx".to_string(),
        limits,
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
    };

    let context = parse_office_file(&request);

    std::fs::remove_file(path).ok();
    assert_eq!(context.error, None);
    assert_eq!(context.parser_backend, RUST_XLSX_BOUNDED_BACKEND);
    assert!(context.content.chars().count() <= 32);
    assert!(context.truncated);
}
