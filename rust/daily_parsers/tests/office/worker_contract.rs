use daily_parsers::contract::{ParseRequest, ParserBackend, WorkerLane};
use daily_parsers::office::{parse, MAX_XLSX_FILE_BYTES};
use daily_parsers::settings::OfficeParseProfile;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::time::UNIX_EPOCH;
use tempfile::tempdir;

#[test]
fn strict_worker_request_parses_bounded_xlsx() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("工作 表.xlsx");
    write_minimal_xlsx(&path);
    let request = request_for(&path, source_version(&path), 1_000_000);

    let response = parse(&request).expect("xlsx should parse");

    assert_eq!(response.parser_backend, ParserBackend::RustXlsxBoundedV2);
    assert_eq!(response.worker_lane, WorkerLane::RustOfficeInProcess);
    assert!(response.content.contains("XLSX strict worker"));
    assert_eq!(response.observed_source_version, source_version(&path));
}

#[test]
fn strict_worker_request_parses_valid_docx_and_pptx() {
    for (file_name, extension, expected_text) in [
        ("modern_sample.docx", ".docx", "Rust DOCX worker content"),
        ("modern_sample.pptx", ".pptx", "Rust PPTX worker content"),
    ] {
        let path = fixture_path(file_name);
        let request = request_for_route(
            &path,
            extension,
            ParserBackend::RustOfficeOxideV2,
            source_version(&path),
            1_000_000,
        );

        let response = parse(&request).expect("office file should parse");

        assert_eq!(
            response.parser_backend,
            ParserBackend::RustOfficeOxideV2,
            "{extension}"
        );
        assert_eq!(
            response.worker_lane,
            WorkerLane::RustOfficeInProcess,
            "{extension}"
        );
        assert!(response.content.contains(expected_text), "{extension}");
        assert_eq!(response.observed_source_version, source_version(&path));
    }
}

#[test]
fn native_worker_reads_binary_legacy_office_with_audited_ranges() {
    for (name, extension, backend, expected) in [
        (
            "legacy_sample.doc",
            ".doc",
            ParserBackend::RustLegacyOfficeV3,
            "Legacy DOC worker content",
        ),
        (
            "legacy_sample.ppt",
            ".ppt",
            ParserBackend::RustLegacyOfficeV3,
            "Legacy PPT worker content",
        ),
        (
            "legacy_sample.xls",
            ".xls",
            ParserBackend::RustXlsCalamineV3,
            "Legacy XLS worker content",
        ),
    ] {
        let path = fixture_path(name);
        let original = fs::read(&path).unwrap();
        let mut request =
            request_for_route(&path, extension, backend, source_version(&path), 1_000_000);
        request.parser_limits.excel_max_rows = 1;
        request.parser_limits.excel_max_columns = 2;
        request.parser_limits.excel_max_sheets = 1;
        let result = parse(&request).unwrap();
        result.validate().unwrap();
        assert_eq!(result.parser_backend, backend);
        assert_eq!(result.worker_lane, WorkerLane::RustOfficeInProcess);
        assert!(
            result.content.contains(expected),
            "{name}: {}",
            result.content
        );
        assert!(!result.content.contains("Row beyond budget"));
        let audit = result.evidence_audit.unwrap();
        assert_eq!(
            audit.detected_format.as_deref(),
            Some(extension.trim_start_matches('.'))
        );
        assert!(!audit.read_ranges.unwrap().is_empty());
        assert!(audit
            .content_units
            .unwrap()
            .iter()
            .all(|unit| !unit.ranges.is_empty()
                || unit.kind == daily_parsers::contract::ContentUnitKind::Heading));
        assert_eq!(fs::read(path).unwrap(), original);
    }
}

#[test]
fn xls_utf16_formula_preserves_cached_text_and_audited_rows() {
    // Synthetic BIFF8 formula ="测试9号"; calamine 0.36.1 misread its UTF-16 bytes.
    let path = fixture_path("legacy_unicode_formula.xls");
    let original = fs::read(&path).unwrap();
    let request = request_for_route(
        &path,
        ".xls",
        ParserBackend::RustXlsCalamineV3,
        source_version(&path),
        1_000_000,
    );

    let result = parse(&request).unwrap();
    result.validate().unwrap();
    assert!(result.content.contains("测试9号"));
    assert!(result.content.contains("Legacy XLS worker content"));
    assert_eq!(result.parser_backend, ParserBackend::RustXlsCalamineV3);
    assert!(result
        .evidence_audit
        .unwrap()
        .read_ranges
        .unwrap()
        .iter()
        .any(|range| range.start == 4 && range.end == 5));
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn format_detection_selects_one_native_parser_and_keeps_the_original_name() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("mislabeled.xls");
    write_minimal_xlsx(&path);
    let request = request_for_route(
        &path,
        ".xls",
        ParserBackend::RustXlsCalamineV3,
        source_version(&path),
        1_000_000,
    );
    let result = parse(&request).unwrap();
    result.validate().unwrap();
    assert_eq!(result.file_type, ".xls");
    assert_eq!(result.parser_backend, ParserBackend::RustXlsxBoundedV2);
    assert_eq!(
        result.evidence_audit.unwrap().detected_format.as_deref(),
        Some("xlsx")
    );
    assert!(result.content.contains("XLSX strict worker"));

    fs::copy(fixture_path("modern_sample.docx"), &path).unwrap();
    let request = request_for_route(
        &path,
        ".xls",
        ParserBackend::RustXlsCalamineV3,
        source_version(&path),
        1_000_000,
    );
    let result = parse(&request).unwrap();
    result.validate().unwrap();
    assert_eq!(result.parser_backend, ParserBackend::RustOfficeOxideV2);
    assert!(result.content.contains("Rust DOCX worker content"));
}

#[test]
fn legacy_enablement_limits_and_corruption_are_enforced_by_the_worker() {
    let directory = tempdir().unwrap();
    for (name, extension) in [("legacy_sample.doc", ".doc"), ("legacy_sample.ppt", ".ppt")] {
        let path = fixture_path(name);
        let mut request = request_for_route(
            &path,
            extension,
            ParserBackend::RustLegacyOfficeV3,
            source_version(&path),
            1_000_000,
        );
        request.parser_limits.legacy_extensions_enabled = false;
        assert_eq!(
            parse(&request).unwrap_err().error_code,
            "UNSUPPORTED_EXTENSION"
        );
        request.parser_limits.legacy_extensions_enabled = true;
        request.parser_limits.document_excerpt_max_chars = 8;
        let result = parse(&request).unwrap();
        assert!(result.truncated);
        assert!(result.content.chars().count() <= 8);
        result.validate().unwrap();

        let corrupt = directory.path().join(name);
        fs::write(&corrupt, b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1private-123456").unwrap();
        let request = request_for_route(
            &corrupt,
            extension,
            ParserBackend::RustLegacyOfficeV3,
            source_version(&corrupt),
            1_000_000,
        );
        let error = parse(&request).unwrap_err();
        assert_eq!(error.error_code, "PARSER_FAILED");
        assert!(!error.retryable);
        assert!(!error.message.contains("123456"));
    }
}

#[test]
fn corrupt_xlsx_is_a_non_retryable_structured_failure() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("corrupt.xlsx");
    fs::write(&path, b"not a zip").expect("corrupt fixture should be written");
    let request = request_for(&path, source_version(&path), 1_000_000);

    let error = parse(&request).expect_err("corrupt xlsx must fail");
    assert_eq!(error.error_code, "PARSER_FAILED");
    assert!(!error.retryable);
}

#[test]
fn corrupt_docx_and_pptx_are_non_retryable_structured_failures() {
    let directory = tempdir().expect("temporary root should exist");
    for (extension, backend) in [
        (".docx", ParserBackend::RustOfficeOxideV2),
        (".pptx", ParserBackend::RustOfficeOxideV2),
    ] {
        let path = directory.path().join(format!("corrupt{extension}"));
        fs::write(&path, b"not a zip").expect("corrupt fixture should be written");
        let request =
            request_for_route(&path, extension, backend, source_version(&path), 1_000_000);

        let error = parse(&request).expect_err("corrupt office must fail");
        assert_eq!(error.error_code, "PARSER_FAILED");
        assert!(!error.retryable, "{extension}");
    }
}

#[test]
fn worker_size_guard_runs_before_office_parser() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("large.xlsx");
    fs::write(&path, b"not a zip").expect("fixture should be written");
    let request = request_for(&path, source_version(&path), 8);

    let error = parse(&request).expect_err("size guard must reject");
    assert_eq!(error.error_code, "FILE_TOO_LARGE");
}

#[test]
fn worker_rejects_a_stale_expected_source_version() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("changed.xlsx");
    write_minimal_xlsx(&path);
    let request = request_for(&path, "mtime_ns=1:size=2".to_string(), 1_000_000);

    let error = parse(&request).expect_err("stale source must reject");
    assert_eq!(error.error_code, "SOURCE_VERSION_CHANGED");
    assert!(error.retryable);
}

#[test]
fn exact_xlsx_size_limit_is_inclusive_even_with_a_larger_request_budget() {
    fn append_padding(path: &std::path::Path, payload: u64) {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut zip = zip::ZipWriter::new_append(file).unwrap();
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("customXml/unused.xml", options).unwrap();
        if payload > 0 {
            zip.write_all(b"<padding/>").unwrap();
            std::io::copy(&mut std::io::repeat(b' ').take(payload - 10), &mut zip).unwrap();
        }
        zip.finish().unwrap();
    }

    let dir = tempdir().unwrap();
    let path = dir.path().join("limit.xlsx");
    write_minimal_xlsx(&path);
    append_padding(&path, 0);
    let overhead = fs::metadata(&path).unwrap().len();
    for size in [
        MAX_XLSX_FILE_BYTES - 1,
        MAX_XLSX_FILE_BYTES,
        MAX_XLSX_FILE_BYTES + 1,
    ] {
        write_minimal_xlsx(&path);
        append_padding(&path, size - overhead);
        assert_eq!(fs::metadata(&path).unwrap().len(), size);
        let request = request_for(&path, source_version(&path), MAX_XLSX_FILE_BYTES + 1);
        let result = parse(&request);
        if size <= MAX_XLSX_FILE_BYTES {
            let parsed = result.unwrap();
            assert!(parsed.content.contains("XLSX strict worker"));
            parsed.validate().unwrap();
        } else {
            assert_eq!(result.unwrap_err().error_code, "FILE_TOO_LARGE");
        }
    }
}

#[test]
fn oversized_xlsx_cell_keeps_only_complete_rows_and_later_files_parse() {
    let dir = tempdir().unwrap();
    let valid = dir.path().join("valid.xlsx");
    let oversized = dir.path().join("oversized.xlsx");
    write_minimal_xlsx(&valid);
    edit_member(&valid, &oversized, "xl/worksheets/sheet1.xml", |_| {
        format!(
        "<worksheet><sheetData><row r='1'><c><v>complete</v></c></row>+        <row r='2'><c><v>{}</v></c><c><v>incomplete row</v></c></row>+        </sheetData></worksheet>", "x".repeat(129 * 1024)
    )
    });
    let request = request_for(&oversized, source_version(&oversized), 5 * 1024 * 1024);
    let parsed = parse(&request).unwrap();
    assert!(parsed.truncated && parsed.content.contains("complete"));
    assert!(units(&parsed).all(|(_, ranges)| ranges.iter().all(|range| range.start != 2)));
    assert!(!parsed.content.contains("incomplete row"));
    let request = request_for(&valid, source_version(&valid), 1_000_000);
    assert!(parse(&request)
        .unwrap()
        .content
        .contains("XLSX strict worker"));
}

#[test]
fn deep_word_xml_and_excessive_zip_entries_fail_before_document_parsing() {
    let dir = tempdir().unwrap();
    let deep = dir.path().join("deep.docx");
    let original = fixture_path("modern_sample.docx");
    edit_member(&original, &deep, "word/document.xml", |_| {
        format!(
        "<w:document xmlns:w='http://schemas.openxmlformats.org/wordprocessingml/2006/main'><w:body>{}{}</w:body></w:document>",
        "<w:sdt>".repeat(110), "</w:sdt>".repeat(110)
    )
    });
    let request = request_for_route(
        &deep,
        ".docx",
        ParserBackend::RustOfficeOxideV2,
        source_version(&deep),
        1_000_000,
    );
    assert_eq!(parse(&request).unwrap_err().error_code, "PARSER_FAILED");

    let many = dir.path().join("many.xlsx");
    let mut zip = zip::ZipWriter::new(File::create(&many).unwrap());
    for i in 0..4097 {
        zip.start_file(
            format!("part{i}.xml"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    }
    zip.finish().unwrap();
    let request = request_for(&many, source_version(&many), 2_000_000);
    assert_eq!(parse(&request).unwrap_err().error_code, "PARSER_FAILED");
}

fn request_for(
    path: &std::path::Path,
    expected_source_version: String,
    max_file_size_bytes: u64,
) -> ParseRequest<OfficeParseProfile> {
    request_for_route(
        path,
        ".xlsx",
        ParserBackend::RustXlsxBoundedV2,
        expected_source_version,
        max_file_size_bytes,
    )
}

#[test]
fn in_process_request_rejects_drive_relative_or_nul_paths() {
    for path in [r"C:relative.xlsx", r"*:relative.xlsx", "C:\\test\0.xlsx"] {
        let request = request_for_route(
            std::path::Path::new(path),
            ".xlsx",
            ParserBackend::RustXlsxBoundedV2,
            "mtime_ns=1:size=1".into(),
            1024,
        );
        assert!(request.validate().is_err());
    }
}

pub(super) fn request_for_route(
    path: &std::path::Path,
    file_type: &str,
    backend: ParserBackend,
    expected_source_version: String,
    max_file_size_bytes: u64,
) -> ParseRequest<OfficeParseProfile> {
    ParseRequest {
        file_path: path.to_string_lossy().to_string(),
        file_type: file_type.to_string(),
        backend,
        remaining_timeout_ms: 30_000,
        max_file_size_bytes,
        parser_limits: OfficeParseProfile {
            primary_backend: ParserBackend::RustOfficeOxideV2.as_str().into(),
            legacy_extensions_enabled: true,
            excel_max_sheets: 2,
            excel_max_rows: 10,
            excel_max_columns: 12,
            docx_max_paragraphs: 80,
            docx_max_tables: 8,
            docx_table_max_rows: 20,
            docx_table_max_cols: 8,
            pptx_max_slides: 15,
            pptx_include_notes: true,
            document_excerpt_max_chars: 4000,
        },
        expected_source_version,
    }
}

pub(super) fn source_version(path: &std::path::Path) -> String {
    let metadata = fs::metadata(path).expect("fixture metadata should exist");
    let mtime_ns = metadata
        .modified()
        .expect("fixture mtime should exist")
        .duration_since(UNIX_EPOCH)
        .expect("fixture mtime should be after epoch")
        .as_nanos();
    format!("mtime_ns={mtime_ns}:size={}", metadata.len())
}

pub(super) fn fixture_path(file_name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("worker_documents")
        .join(file_name)
        .canonicalize()
        .expect("committed Office fixture should exist")
}

pub(super) fn rich_fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/office/fixtures")
        .join(name)
}

fn rich_request(
    path: &std::path::Path,
    backend: ParserBackend,
) -> ParseRequest<OfficeParseProfile> {
    let extension = format!(".{}", path.extension().unwrap().to_str().unwrap());
    request_for_route(path, &extension, backend, source_version(path), 1_000_000)
}

fn units(
    result: &daily_parsers::contract::ParseResult,
) -> impl Iterator<Item = (String, &[daily_parsers::contract::SourceRange])> {
    result
        .evidence_audit
        .as_ref()
        .unwrap()
        .content_units
        .as_ref()
        .unwrap()
        .iter()
        .map(|u| {
            (
                result
                    .content
                    .chars()
                    .skip(u.text_start as usize)
                    .take((u.text_end - u.text_start) as usize)
                    .collect(),
                u.ranges.as_slice(),
            )
        })
}

#[test]
fn daily_workbooks_keep_late_sheet_totals_and_right_column_provenance() {
    for (name, backend) in [
        ("settlement.xlsx", ParserBackend::RustXlsxBoundedV2),
        ("settlement.xls", ParserBackend::RustXlsCalamineV3),
    ] {
        let path = rich_fixture(name);
        for columns in [2, 12] {
            let mut request = rich_request(&path, backend);
            request.parser_limits.excel_max_rows = 6;
            request.parser_limits.excel_max_columns = columns;
            let parsed = parse(&request).unwrap();
            parsed.validate().unwrap();
            assert!(
                parsed.truncated && parsed.content.contains("[6] 结算汇总"),
                "{name}: {}",
                parsed.content
            );
            let (line, ranges) = units(&parsed)
                .find(|(t, _)| t.contains("W508") && t.contains("498765.43"))
                .unwrap();
            assert!(line.contains("实付额（元）") && line.contains("合计"));
            assert!(!line.contains("512345.67"));
            assert!(ranges.iter().any(|r| r.label.as_deref() == Some("结算汇总")
                && r.start == 508
                && r.end == 509
                && r.column_start == Some(23)
                && r.column_end == Some(24)));
            assert!(ranges.iter().all(|r| r.column_start != Some(22)));
            assert!(ranges
                .iter()
                .any(|r| r.start < 3 && r.column_start == Some(23)));
            if columns == 12 {
                assert!(units(&parsed).any(|(line, _)| line.contains("V508")
                    && line.contains("512345.67")
                    && !line.contains("498765.43")));
            }
            if name.ends_with(".xls") {
                assert!(parsed.content.contains("单位：元"));
                assert!(units(&parsed).any(|(line, _)| line.contains("W407")
                    && line.contains("932.75")
                    && line.contains("待核差异")));
            }
        }
    }
}

#[test]
fn daily_docx_keeps_identical_values_with_their_own_caption_and_units() {
    use daily_parsers::contract::SourceRangeUnit::*;
    let path = rich_fixture("scopes.docx");
    let parsed = parse(&rich_request(&path, ParserBackend::RustOfficeOxideV2)).unwrap();
    parsed.validate().unwrap();
    for (ordinal, title, other) in [
        (1, "表1 预算（万元）", "实付"),
        (2, "表2 实付（元）", "预算"),
    ] {
        let (text, ranges) = units(&parsed)
            .find(|(t, _)| t.contains(title) && t.contains("本期") && t.contains("12"))
            .unwrap();
        assert!(!text.contains(other));
        assert!(ranges
            .iter()
            .any(|r| r.unit == Paragraph && r.start == ordinal && r.end == ordinal + 1));
        assert!(ranges.iter().any(|r| r.unit == TableRow
            && r.start == 2
            && r.label == Some(format!("Table {ordinal}"))));
    }
}

#[test]
fn daily_pptx_keeps_group_notes_and_series_with_one_cached_point() {
    let path = rich_fixture("delivery.pptx");
    let mut request = rich_request(&path, ParserBackend::RustOfficeOxideV2);
    for notes in [true, false] {
        request.parser_limits.pptx_include_notes = notes;
        let parsed = parse(&request).unwrap();
        parsed.validate().unwrap();
        assert!(parsed.content.contains("交付台账"));
        assert_eq!(parsed.content.contains("批准前复核数量"), notes);
        let (line, ranges) = units(&parsed).find(|(t, _)| t.contains("7319")).unwrap();
        assert!(line.contains("barChart") && line.contains("实收") && line.contains("第三期"));
        assert_eq!(ranges.len(), 1);
        assert!(
            ranges[0].start == 1
                && ranges[0]
                    .label
                    .as_ref()
                    .unwrap()
                    .contains("/ppt/charts/chart1.xml")
        );
    }
}

// Rewrite only members of a temporary copy. The committed source documents remain immutable.
fn edit_member(
    source: &std::path::Path,
    dest: &std::path::Path,
    name: &str,
    edit: impl Fn(&str) -> String,
) {
    let mut input = zip::ZipArchive::new(File::open(source).unwrap()).unwrap();
    let mut output = zip::ZipWriter::new(File::create(dest).unwrap());
    for i in 0..input.len() {
        let mut member = input.by_index(i).unwrap();
        if member.name() != name {
            output.raw_copy_file(member).unwrap();
        } else {
            let mut xml = String::new();
            member.read_to_string(&mut xml).unwrap();
            output
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            output.write_all(edit(&xml).as_bytes()).unwrap();
        }
    }
    output.finish().unwrap();
}

#[test]
fn pptx_uses_point_indices_and_only_the_slide_internal_chart_relationship() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("points.pptx");
    edit_member(
        &rich_fixture("delivery.pptx"),
        &file,
        "ppt/charts/chart1.xml",
        |xml| xml.replace("<c:pt idx=\"0\"><c:v>7319", "<c:pt idx=\"9\"><c:v>7319"),
    );
    let grouped = dir.path().join("grouped.pptx");
    edit_member(&file, &grouped, "ppt/slides/slide1.xml", |xml| {
        let start = xml.find("<p:graphicFrame>").unwrap();
        let end =
            start + xml[start..].find("</p:graphicFrame>").unwrap() + "</p:graphicFrame>".len();
        let mut updated = xml.to_owned();
        updated.replace_range(start..end, "");
        let group_end = updated.find("</p:grpSp>").unwrap();
        updated.insert_str(group_end, &xml[start..end]);
        updated
    });
    let parsed = parse(&rich_request(&grouped, ParserBackend::RustOfficeOxideV2)).unwrap();
    parsed.validate().unwrap();
    let (line, _) = units(&parsed).find(|(s, _)| s.contains("7319")).unwrap();
    assert!(line.contains("点 9") && !line.contains("第三期"));
    assert!(units(&parsed)
        .any(|(s, _)| s.contains("第三期") && s.contains("点 0") && !s.contains("7319")));
    let external = dir.path().join("external.pptx");
    edit_member(
        &grouped,
        &external,
        "ppt/slides/_rels/slide1.xml.rels",
        |xml| {
            xml.replace(
                "Target=\"../charts/chart1.xml\"",
                "TargetMode=\"External\" Target=\"../charts/chart1.xml\"",
            )
        },
    );
    let parsed = parse(&rich_request(&external, ParserBackend::RustOfficeOxideV2)).unwrap();
    assert!(parsed.truncated && parsed.content.contains("交付台账"));
    assert!(!parsed.content.contains("7319"));
}

#[test]
fn intrinsically_captioned_docx_rows_do_not_inherit_a_nearby_paragraph() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("native.docx");
    edit_member(
        &rich_fixture("scopes.docx"),
        &file,
        "word/document.xml",
        |xml| {
            xml.replacen(
                "<w:tblPr>",
                "<w:tblPr><w:tblCaption w:val=\"期末库存（件）\"/>",
                1,
            )
        },
    );
    let parsed = parse(&rich_request(&file, ParserBackend::RustOfficeOxideV2)).unwrap();
    parsed.validate().unwrap();
    let (text, ranges) = units(&parsed)
        .find(|(s, _)| s.contains("本期") && s.contains("期末库存（件）"))
        .unwrap();
    assert!(!text.contains("预算") && ranges.len() == 1);
    assert!(ranges[0].label.as_deref() == Some("Table 1"));
}

#[test]
fn implicit_worksheet_positions_follow_the_previous_physical_cell_and_row() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("implicit.xlsx");
    edit_member(
        &rich_fixture("settlement.xlsx"),
        &file,
        "xl/worksheets/sheet6.xml",
        |_| {
            r#"<worksheet><sheetData>
          <row r="1"><c r="U1" t="str"><v>项目</v></c><c r="W1" t="str"><v>实际金额</v></c></row>
          <row><c r="U42" t="str"><v>结项</v></c><c/><c><v>3127</v></c></row>
          <row/><row><c t="str"><v>尾部</v></c></row>
        </sheetData></worksheet>"#
                .to_string()
        },
    );
    let mut req = rich_request(&file, ParserBackend::RustXlsxBoundedV2);
    req.parser_limits.excel_max_sheets = 6;
    let parsed = parse(&req).unwrap();
    parsed.validate().unwrap();
    let (line, ranges) = units(&parsed)
        .find(|(s, _)| s.contains("W42=3127"))
        .unwrap();
    assert!(line.contains("U42=结项") && !line.contains("V42"));
    assert!(ranges
        .iter()
        .any(|r| r.start == 42 && r.column_start == Some(23) && r.column_end == Some(24)));
    assert!(units(&parsed).any(|(s, _)| s.contains("A44=尾部")));
}

#[test]
fn inflated_sheet_budget_keeps_complete_rows_and_marks_the_unread_tail() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("bounded.xlsx");
    edit_member(
        &rich_fixture("settlement.xlsx"),
        &file,
        "xl/worksheets/sheet6.xml",
        |_| {
            format!("<worksheet><sheetData><row r=\"1\"><c t=\"str\"><v>项目</v></c></row><row r=\"2\"><c t=\"str\"><v>已读明细</v></c></row><!--{}--><row r=\"508\"><c t=\"str\"><v>未读合计</v></c></row></sheetData></worksheet>", " ".repeat(9*1024*1024))
        },
    );
    let mut req = rich_request(&file, ParserBackend::RustXlsxBoundedV2);
    req.max_file_size_bytes = 16 * 1024 * 1024;
    req.parser_limits.excel_max_sheets = 6;
    let parsed = parse(&req).unwrap();
    parsed.validate().unwrap();
    assert!(
        parsed.truncated
            && parsed.content.contains("已读明细")
            && !parsed.content.contains("未读合计")
    );
    assert!(units(&parsed).all(|(_, rs)| rs.iter().all(|r| r.start != 508)));
}

#[test]
fn chapter_caption_between_two_tables_stays_an_independent_paragraph() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("chapters.docx");
    edit_member(
        &rich_fixture("scopes.docx"),
        &file,
        "word/document.xml",
        |xml| {
            xml.replace("表1 预算", "表1-1 预算")
                .replace("表2 实付", "表1-2 实付")
        },
    );
    let parsed = parse(&rich_request(&file, ParserBackend::RustOfficeOxideV2)).unwrap();
    parsed.validate().unwrap();
    assert!(parsed.truncated);
    assert!(units(&parsed).any(|(t, _)| t.contains("表1-1 预算") && t.contains("本期")));
    assert!(units(&parsed).any(|(t, r)| t.contains("表1-2 实付")
        && r.iter()
            .all(|s| s.unit == daily_parsers::contract::SourceRangeUnit::Paragraph)));
    assert!(units(&parsed).all(|(t, r)| !t.contains("表1-2")
        || !r
            .iter()
            .any(|s| s.unit == daily_parsers::contract::SourceRangeUnit::TableRow)));
}

#[test]
fn cached_chart_percentages_keep_raw_format_and_supported_display_separate() {
    let dir = tempdir().unwrap();
    for (i, (code, value, point_format, display)) in [
        ("0%", "0.25", None, Some("25%")),
        ("0.0%", "-1.25E-2", None, Some("-1.3%")),
        ("General", "0.25", Some("0%"), Some("25%")),
        ("0.00%;[Red](0.00%)", "0.25", None, None),
        ("General", "0.25", None, None),
    ]
    .into_iter()
    .enumerate()
    {
        let file = dir.path().join(format!("format{i}.pptx"));
        edit_member(
            &rich_fixture("delivery.pptx"),
            &file,
            "ppt/charts/chart1.xml",
            |xml| {
                let attr = point_format
                    .map(|f| format!(" formatCode=\"{f}\""))
                    .unwrap_or_default();
                xml.replace(
                    "<c:formatCode>General</c:formatCode>",
                    &format!("<c:formatCode>{code}</c:formatCode>"),
                )
                .replace(
                    "<c:pt idx=\"0\"><c:v>7319",
                    &format!("<c:pt idx=\"0\"{attr}><c:v>{value}"),
                )
            },
        );
        let parsed = parse(&rich_request(&file, ParserBackend::RustOfficeOxideV2)).unwrap();
        parsed.validate().unwrap();
        let (line, _) = units(&parsed)
            .find(|(s, _)| s.contains("第三期") && s.contains("缓存值"))
            .unwrap();
        assert!(
            line.contains(&format!("缓存值 {value}"))
                && line.contains(&format!("缓存格式码 {code}"))
                && line.contains(&format!("格式码 {}", point_format.unwrap_or(code))),
            "{line}"
        );
        if let Some(display) = display {
            assert!(line.contains(&format!("显示值 {display}")), "{line}");
        } else {
            assert!(!line.contains("显示值"), "{line}");
        }
    }
}

#[test]
fn wide_totals_keep_complete_fields_and_one_header_under_a_small_output_budget() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("wide.xlsx");
    edit_member(
        &rich_fixture("settlement.xlsx"),
        &file,
        "xl/worksheets/sheet6.xml",
        |_| {
            let mut xml = String::from("<worksheet><sheetData>");
            for n in 1..=21 {
                xml.push_str(&format!(
                    "<row r=\"{n}\"><c r=\"A{n}\" t=\"str\"><v>{}</v></c>",
                    if n == 1 { "项目" } else { "合计" }
                ));
                for col in b'B'..=b'W' {
                    let col = char::from(col);
                    let text = if n == 1 {
                        format!("{col}金额")
                    } else {
                        n.to_string()
                    };
                    xml.push_str(&format!("<c r=\"{col}{n}\" t=\"str\"><v>{text}</v></c>"));
                }
                xml.push_str("</row>");
            }
            xml.push_str("</sheetData></worksheet>");
            xml
        },
    );
    let mut req = rich_request(&file, ParserBackend::RustXlsxBoundedV2);
    req.parser_limits.excel_max_sheets = 1;
    req.parser_limits.excel_max_rows = 24;
    req.parser_limits.excel_max_columns = 24;
    req.parser_limits.document_excerpt_max_chars = 640;
    let parsed = parse(&req).unwrap();
    parsed.validate().unwrap();
    assert!(parsed.truncated && parsed.content.chars().count() <= 640);
    assert_eq!(parsed.content.matches("W1=W金额").count(), 1);
    assert!(units(&parsed).any(|(s, r)| s.contains("B2 B金额 = 2") && r[0].column_start == Some(2)));
}

#[test]
fn positioned_quantities_and_amounts_keep_separate_unit_scopes() {
    let dir = tempdir().unwrap();
    for (i, (note, amount_attached)) in [("金额单位：万元", true), ("单位：万元", false)]
        .into_iter()
        .enumerate()
    {
        let file = dir.path().join(format!("units{i}.xlsx"));
        edit_member(
            &rich_fixture("settlement.xlsx"),
            &file,
            "xl/worksheets/sheet6.xml",
            |_| {
                format!(
                    r#"<worksheet><sheetData>
         <row r="1"><c r="A1" t="str"><v>{note}</v></c></row>
         <row r="2"><c r="A2" t="str"><v>项目</v></c><c r="V2" t="str"><v>数量（台）</v></c><c r="W2" t="str"><v>实付金额</v></c></row>
         <row r="3"><c r="A3" t="str"><v>合计</v></c><c r="V3"><v>8</v></c><c r="W3"><v>25</v></c></row>
        </sheetData></worksheet>"#
                )
            },
        );
        let parsed = parse(&rich_request(&file, ParserBackend::RustXlsxBoundedV2)).unwrap();
        parsed.validate().unwrap();
        assert!(parsed.content.contains(note));
        let (q, qr) = units(&parsed)
            .find(|(t, _)| t.contains("V3 数量（台） = 8"))
            .unwrap();
        assert!(!q.contains("万元") && qr.iter().all(|r| r.start != 1));
        let (a, ar) = units(&parsed)
            .find(|(t, _)| t.contains("W3 实付金额 = 25"))
            .unwrap();
        assert_eq!(a.contains(note), amount_attached);
        assert_eq!(
            ar.iter().any(|r| r.start == 1 && r.column_start == Some(1)),
            amount_attached
        );
    }
}

#[test]
fn ordinary_numbered_paragraph_is_not_promoted_to_a_table_caption() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("narrative.docx");
    edit_member(
        &rich_fixture("scopes.docx"),
        &file,
        "word/document.xml",
        |xml| {
            xml.replacen("<w:pStyle w:val=\"Caption\"/>", "", 1)
                .replace("表1 预算（万元）", "表1 列出了本期预测")
        },
    );
    let parsed = parse(&rich_request(&file, ParserBackend::RustOfficeOxideV2)).unwrap();
    assert!(parsed.content.contains("表1 列出了本期预测"));
    assert!(units(&parsed).all(|(s, _)| !(s.contains("本期预测") && s.contains("| 本期 | 12 |"))));
}

pub(super) fn write_minimal_xlsx(path: &std::path::Path) {
    let file = File::create(path).expect("xlsx fixture should be creatable");
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, body) in [
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
 xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
 <sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
 <Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
 <sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>XLSX strict worker</t></is></c></row></sheetData>
</worksheet>"#,
        ),
    ] {
        zip.start_file(name, options)
            .expect("zip entry should start");
        zip.write_all(body.as_bytes())
            .expect("zip entry should be written");
    }
    zip.finish().expect("xlsx fixture should finish");
}
