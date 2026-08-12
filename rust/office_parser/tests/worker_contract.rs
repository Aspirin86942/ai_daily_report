use ai_daily_office_parser::parse_worker_request;
use ai_daily_worker_contract::{ParseRequest, ParserBackend, ParserLimits, WorkerLane};
use std::fs::{self, File};
use std::io::Write;
use std::time::UNIX_EPOCH;
use tempfile::tempdir;

#[test]
fn strict_worker_request_parses_bounded_xlsx() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("工作 表.xlsx");
    write_minimal_xlsx(&path);
    let request = request_for(&path, source_version(&path), 1_000_000);

    let response = parse_worker_request(&request).expect("xlsx should parse");

    assert_eq!(response.parser_backend, ParserBackend::RustXlsxBoundedV2);
    assert_eq!(response.worker_lane, WorkerLane::RustOfficeProcessV2);
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

        let response = parse_worker_request(&request).expect("office file should parse");

        assert_eq!(
            response.parser_backend,
            ParserBackend::RustOfficeOxideV2,
            "{extension}"
        );
        assert_eq!(
            response.worker_lane,
            WorkerLane::RustOfficeProcessV2,
            "{extension}"
        );
        assert!(response.content.contains(expected_text), "{extension}");
        assert_eq!(response.observed_source_version, source_version(&path));
    }
}

#[test]
fn corrupt_xlsx_is_a_non_retryable_structured_failure() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("corrupt.xlsx");
    fs::write(&path, b"not a zip").expect("corrupt fixture should be written");
    let request = request_for(&path, source_version(&path), 1_000_000);

    let error = parse_worker_request(&request).expect_err("corrupt xlsx must fail");
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

        let error = parse_worker_request(&request).expect_err("corrupt office must fail");
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

    let error = parse_worker_request(&request).expect_err("size guard must reject");
    assert_eq!(error.error_code, "FILE_TOO_LARGE");
}

#[test]
fn worker_rejects_a_stale_expected_source_version() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("changed.xlsx");
    write_minimal_xlsx(&path);
    let request = request_for(&path, "mtime_ns=1:size=2".to_string(), 1_000_000);

    let error = parse_worker_request(&request).expect_err("stale source must reject");
    assert_eq!(error.error_code, "SOURCE_VERSION_CHANGED");
    assert!(error.retryable);
}

fn request_for(
    path: &std::path::Path,
    expected_source_version: String,
    max_file_size_bytes: u64,
) -> ParseRequest {
    request_for_route(
        path,
        ".xlsx",
        ParserBackend::RustXlsxBoundedV2,
        expected_source_version,
        max_file_size_bytes,
    )
}

fn request_for_route(
    path: &std::path::Path,
    file_type: &str,
    backend: ParserBackend,
    expected_source_version: String,
    max_file_size_bytes: u64,
) -> ParseRequest {
    ParseRequest {
        file_path: path.to_string_lossy().to_string(),
        file_type: file_type.to_string(),
        backend,
        remaining_timeout_ms: 30_000,
        max_file_size_bytes,
        parser_limits: ParserLimits::Office {
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

fn source_version(path: &std::path::Path) -> String {
    let metadata = fs::metadata(path).expect("fixture metadata should exist");
    let mtime_ns = metadata
        .modified()
        .expect("fixture mtime should exist")
        .duration_since(UNIX_EPOCH)
        .expect("fixture mtime should be after epoch")
        .as_nanos();
    format!("mtime_ns={mtime_ns}:size={}", metadata.len())
}

fn fixture_path(file_name: &str) -> std::path::PathBuf {
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

fn write_minimal_xlsx(path: &std::path::Path) {
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
