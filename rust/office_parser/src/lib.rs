use ai_daily_scanner_contract::{WorkerKind, WorkerVersionResponse};
use quick_xml::events::{BytesRef, BytesStart, BytesText, Event};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::PathBuf;
use zip::read::ZipArchive;

pub const RUST_OFFICE_BACKEND: &str = "rust_office_oxide_v1";
pub const RUST_XLSX_BOUNDED_BACKEND: &str = "rust_xlsx_bounded_v1";
pub const WORKER_CONTRACT_VERSION: &str = "ai_daily_worker_v1";

pub fn worker_version_response() -> WorkerVersionResponse {
    WorkerVersionResponse {
        contract: "ai_daily_worker".to_string(),
        protocol_version: 1,
        worker_kind: WorkerKind::Office,
        worker_contract_version: WORKER_CONTRACT_VERSION.to_string(),
        worker_version: env!("CARGO_PKG_VERSION").to_string(),
        worker_build: option_env!("AI_DAILY_OFFICE_WORKER_BUILD")
            .unwrap_or("dev-office-worker")
            .to_string(),
        supported_backends: vec![
            RUST_OFFICE_BACKEND.to_string(),
            RUST_XLSX_BOUNDED_BACKEND.to_string(),
        ],
        supported_extensions: vec![
            ".docx".to_string(),
            ".pptx".to_string(),
            ".xlsx".to_string(),
        ],
    }
}

#[derive(Debug, Deserialize)]
pub struct OfficeParseRequest {
    pub file_path: PathBuf,
    pub file_type: String,
    pub limits: BTreeMap<String, serde_json::Value>,
    pub parser_backend: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileContextOut {
    pub file_path: String,
    pub file_type: String,
    pub content: String,
    pub error: Option<String>,
    pub parser_backend: String,
    pub truncated: bool,
}

pub fn normalize_file_type(file_type: &str) -> String {
    file_type.trim().to_ascii_lowercase()
}

pub fn is_supported_office_type(file_type: &str) -> bool {
    matches!(
        normalize_file_type(file_type).as_str(),
        ".docx" | ".xlsx" | ".pptx" | ".doc" | ".xls" | ".ppt"
    )
}

pub fn positive_limit(
    limits: &BTreeMap<String, serde_json::Value>,
    key: &str,
    default_value: usize,
) -> usize {
    limits
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_value)
}

pub fn truncate_content(content: &str, max_chars: usize) -> (String, bool) {
    let max_chars = max_chars.max(1);
    let mut output = String::new();
    let mut truncated = false;
    for (index, character) in content.chars().enumerate() {
        if index >= max_chars {
            truncated = true;
            break;
        }
        output.push(character);
    }
    (output, truncated)
}

pub fn unsupported_context(request: &OfficeParseRequest) -> FileContextOut {
    let file_type = normalize_file_type(&request.file_type);
    FileContextOut {
        file_path: request.file_path.to_string_lossy().to_string(),
        file_type: file_type.clone(),
        content: String::new(),
        error: Some(format!("RUST_OFFICE_UNSUPPORTED_EXTENSION: {file_type}")),
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
        truncated: false,
    }
}

pub fn parse_office_file(request: &OfficeParseRequest) -> FileContextOut {
    let file_type = normalize_file_type(&request.file_type);
    if !is_supported_office_type(&file_type) {
        return unsupported_context(request);
    }

    let max_chars = positive_limit(&request.limits, "document_excerpt_max_chars", 6000);

    if file_type == ".xlsx" {
        return parse_bounded_xlsx(request, &file_type, max_chars);
    }

    match office_oxide::Document::open(&request.file_path) {
        Ok(document) => {
            let markdown = document.to_markdown();
            let content = if markdown.trim().is_empty() {
                "No Office text extracted".to_string()
            } else {
                markdown
            };
            let (content, truncated) = truncate_content(&content, max_chars);
            FileContextOut {
                file_path: request.file_path.to_string_lossy().to_string(),
                file_type,
                content,
                error: None,
                parser_backend: RUST_OFFICE_BACKEND.to_string(),
                truncated,
            }
        }
        Err(error) => FileContextOut {
            file_path: request.file_path.to_string_lossy().to_string(),
            file_type,
            content: String::new(),
            error: Some(format!("RUST_OFFICE_PARSE_FAILED: {error}")),
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
            truncated: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn write_test_xlsx(parts: &[(&str, &str)]) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bounded-xlsx-{unique}.xlsx"));
        let file = File::create(&path).expect("test xlsx should be creatable");
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

    #[test]
    fn supported_office_type_is_case_insensitive() {
        assert!(is_supported_office_type(".DOCX"));
        assert!(is_supported_office_type(".xlsx"));
        assert!(is_supported_office_type(".PPT"));
        assert!(!is_supported_office_type(".pdf"));
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
}

#[derive(Debug, Clone)]
struct XlsxBudget {
    max_sheets: usize,
    max_rows: usize,
    max_columns: usize,
    max_chars: usize,
}

#[derive(Debug, Clone)]
struct SheetRef {
    name: String,
    rel_id: String,
}

#[derive(Debug, Clone)]
enum PreviewCell {
    Text(String),
    SharedString(usize),
}

#[derive(Debug, Clone)]
struct PreviewSheet {
    name: String,
    rows: Vec<Vec<PreviewCell>>,
    shared_string_indexes: BTreeSet<usize>,
    truncated: bool,
}

struct CellBuilder {
    capture: bool,
    column_index: usize,
    cell_type: String,
    text: String,
    in_value: bool,
    in_text: bool,
}

fn parse_bounded_xlsx(
    request: &OfficeParseRequest,
    file_type: &str,
    max_chars: usize,
) -> FileContextOut {
    match parse_bounded_xlsx_inner(request, max_chars) {
        Ok((content, truncated)) => FileContextOut {
            file_path: request.file_path.to_string_lossy().to_string(),
            file_type: file_type.to_string(),
            content,
            error: None,
            parser_backend: RUST_XLSX_BOUNDED_BACKEND.to_string(),
            truncated,
        },
        Err(error) => FileContextOut {
            file_path: request.file_path.to_string_lossy().to_string(),
            file_type: file_type.to_string(),
            content: String::new(),
            error: Some(format!("RUST_XLSX_BOUNDED_PARSE_FAILED: {error}")),
            parser_backend: RUST_XLSX_BOUNDED_BACKEND.to_string(),
            truncated: false,
        },
    }
}

fn parse_bounded_xlsx_inner(
    request: &OfficeParseRequest,
    max_chars: usize,
) -> Result<(String, bool), String> {
    let budget = XlsxBudget {
        max_sheets: positive_limit(&request.limits, "excel_max_sheets", 2),
        max_rows: positive_limit(&request.limits, "excel_max_rows", 10),
        max_columns: positive_limit(&request.limits, "excel_max_columns", 12),
        max_chars: max_chars.max(1),
    };

    let file = File::open(&request.file_path).map_err(|error| format!("I/O error: {error}"))?;
    let mut archive = ZipArchive::new(file).map_err(|error| format!("ZIP error: {error}"))?;
    let workbook_xml = read_zip_text(&mut archive, "xl/workbook.xml")?;
    let (sheets, sheets_truncated) = parse_workbook_sheets(&workbook_xml, budget.max_sheets)?;
    let rels_xml = read_zip_text(&mut archive, "xl/_rels/workbook.xml.rels")?;
    let relationships = parse_workbook_relationships(&rels_xml)?;

    let mut preview_sheets = Vec::new();
    let mut needed_shared_strings = BTreeSet::new();
    let mut truncated = sheets_truncated;
    for sheet in sheets {
        let Some(path) = resolve_sheet_path(&sheet, &relationships) else {
            truncated = true;
            continue;
        };
        let preview = parse_sheet_bounded(&mut archive, &path, &sheet.name, &budget)?;
        needed_shared_strings.extend(preview.shared_string_indexes.iter().copied());
        truncated |= preview.truncated;
        preview_sheets.push(preview);
    }

    let shared_strings = parse_needed_shared_strings(&mut archive, &needed_shared_strings)?;
    let (content, char_truncated) = render_xlsx_markdown(&preview_sheets, &shared_strings, &budget);
    Ok((content, truncated || char_truncated))
}

fn read_zip_text<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, String> {
    let mut entry = archive
        .by_name(name)
        .map_err(|error| format!("ZIP entry {name} error: {error}"))?;
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .map_err(|error| format!("ZIP entry {name} read error: {error}"))?;
    Ok(text)
}

fn parse_workbook_sheets(xml: &str, max_sheets: usize) -> Result<(Vec<SheetRef>, bool), String> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(xml.as_bytes()));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut sheets = Vec::new();
    let mut truncated = false;

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|error| format!("workbook XML error: {error}"))?
        {
            Event::Start(e) | Event::Empty(e) if is_local_name(e.name().as_ref(), b"sheet") => {
                let name = attr_value(&e, b"name")?.unwrap_or_else(|| "Sheet".to_string());
                let rel_id = attr_value(&e, b"id")?.unwrap_or_default();
                if !rel_id.is_empty() {
                    if sheets.len() >= max_sheets {
                        truncated = true;
                        break;
                    }
                    sheets.push(SheetRef { name, rel_id });
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok((sheets, truncated))
}

fn parse_workbook_relationships(xml: &str) -> Result<HashMap<String, String>, String> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(xml.as_bytes()));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut relationships = HashMap::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|error| format!("relationships XML error: {error}"))?
        {
            Event::Start(e) | Event::Empty(e)
                if is_local_name(e.name().as_ref(), b"Relationship") =>
            {
                if let (Some(id), Some(target)) =
                    (attr_value(&e, b"Id")?, attr_value(&e, b"Target")?)
                {
                    relationships.insert(id, normalize_sheet_target(&target));
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(relationships)
}

fn normalize_sheet_target(target: &str) -> String {
    let target = target.trim_start_matches('/');
    if target.starts_with("xl/") {
        target.to_string()
    } else {
        format!("xl/{target}")
    }
}

fn resolve_sheet_path(sheet: &SheetRef, relationships: &HashMap<String, String>) -> Option<String> {
    relationships.get(&sheet.rel_id).cloned()
}

fn parse_sheet_bounded<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    path: &str,
    sheet_name: &str,
    budget: &XlsxBudget,
) -> Result<PreviewSheet, String> {
    let entry = archive
        .by_name(path)
        .map_err(|error| format!("worksheet {path} error: {error}"))?;
    let mut reader = quick_xml::Reader::from_reader(BufReader::new(entry));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut rows: Vec<Vec<PreviewCell>> = Vec::new();
    let mut current_row: Option<Vec<Option<PreviewCell>>> = None;
    let mut current_cell: Option<CellBuilder> = None;
    let mut shared_string_indexes = BTreeSet::new();
    let mut truncated = false;

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|error| format!("worksheet {path} XML error: {error}"))?
        {
            Event::Start(e) if is_local_name(e.name().as_ref(), b"row") => {
                if rows.len() >= budget.max_rows {
                    truncated = true;
                    break;
                }
                current_row = Some(vec![None; budget.max_columns]);
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"row") => {
                if let Some(row) = current_row.take() {
                    if let Some(trimmed) = trim_preview_row(row) {
                        rows.push(trimmed);
                    }
                }
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"c") => {
                let column_index = attr_value(&e, b"r")?
                    .and_then(|cell_ref| column_index_from_cell_ref(&cell_ref))
                    .unwrap_or_else(|| {
                        current_row
                            .as_ref()
                            .map(|row| row.iter().filter(|cell| cell.is_some()).count() + 1)
                            .unwrap_or(1)
                    });
                let capture = column_index <= budget.max_columns;
                if !capture {
                    truncated = true;
                }
                current_cell = Some(CellBuilder {
                    capture,
                    column_index,
                    cell_type: attr_value(&e, b"t")?.unwrap_or_default(),
                    text: String::new(),
                    in_value: false,
                    in_text: false,
                });
            }
            Event::Empty(e) if is_local_name(e.name().as_ref(), b"c") => {
                if let Some(row) = current_row.as_mut() {
                    let column_index = attr_value(&e, b"r")?
                        .and_then(|cell_ref| column_index_from_cell_ref(&cell_ref))
                        .unwrap_or_else(|| row.iter().filter(|cell| cell.is_some()).count() + 1);
                    if column_index > budget.max_columns {
                        truncated = true;
                    }
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"c") => {
                if let (Some(cell), Some(row)) = (current_cell.take(), current_row.as_mut()) {
                    if cell.capture {
                        if let Some((column_index, preview_cell)) =
                            finish_cell(cell, &mut shared_string_indexes)
                        {
                            let target = column_index.min(row.len()).max(1) - 1;
                            row[target] = Some(preview_cell);
                        }
                    }
                }
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"v") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_value = true;
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"v") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_value = false;
                }
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"t") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_text = true;
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"t") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_text = false;
                }
            }
            Event::Text(e) => {
                if let Some(cell) = current_cell.as_mut() {
                    if cell.capture && (cell.in_value || cell.in_text) {
                        cell.text.push_str(&unescape_text(&e)?);
                    }
                }
            }
            Event::GeneralRef(e) => {
                if let Some(cell) = current_cell.as_mut() {
                    if cell.capture && (cell.in_value || cell.in_text) {
                        cell.text.push_str(&unescape_general_ref(&e)?);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(PreviewSheet {
        name: sheet_name.to_string(),
        rows,
        shared_string_indexes,
        truncated,
    })
}

fn finish_cell(
    cell: CellBuilder,
    shared_string_indexes: &mut BTreeSet<usize>,
) -> Option<(usize, PreviewCell)> {
    let text = cell.text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    if cell.cell_type == "s" {
        if let Ok(index) = text.parse::<usize>() {
            shared_string_indexes.insert(index);
            return Some((cell.column_index, PreviewCell::SharedString(index)));
        }
    }
    Some((cell.column_index, PreviewCell::Text(text)))
}

fn trim_preview_row(row: Vec<Option<PreviewCell>>) -> Option<Vec<PreviewCell>> {
    let mut cells: Vec<PreviewCell> = row
        .into_iter()
        .map(|cell| cell.unwrap_or_else(|| PreviewCell::Text(String::new())))
        .collect();
    while cells
        .last()
        .is_some_and(|cell| matches!(cell, PreviewCell::Text(value) if value.is_empty()))
    {
        cells.pop();
    }
    if cells.is_empty() {
        None
    } else {
        Some(cells)
    }
}

fn parse_needed_shared_strings<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    needed: &BTreeSet<usize>,
) -> Result<HashMap<usize, String>, String> {
    let Some(max_needed) = needed.iter().next_back().copied() else {
        return Ok(HashMap::new());
    };
    let Ok(entry) = archive.by_name("xl/sharedStrings.xml") else {
        return Ok(HashMap::new());
    };
    let mut reader = quick_xml::Reader::from_reader(BufReader::new(entry));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut result = HashMap::new();
    let mut current_index = 0usize;
    let mut in_si = false;
    let mut in_text = false;
    let mut current_text = String::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|error| format!("sharedStrings XML error: {error}"))?
        {
            Event::Start(e) if is_local_name(e.name().as_ref(), b"si") => {
                in_si = true;
                current_text.clear();
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"si") => {
                if needed.contains(&current_index) {
                    result.insert(current_index, current_text.clone());
                }
                if current_index >= max_needed {
                    break;
                }
                current_index += 1;
                in_si = false;
                current_text.clear();
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"t") && in_si => {
                in_text = true;
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"t") => {
                in_text = false;
            }
            Event::Text(e) if in_si && in_text => {
                current_text.push_str(&unescape_text(&e)?);
            }
            Event::GeneralRef(e) if in_si && in_text => {
                current_text.push_str(&unescape_general_ref(&e)?);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(result)
}

fn render_xlsx_markdown(
    sheets: &[PreviewSheet],
    shared_strings: &HashMap<usize, String>,
    budget: &XlsxBudget,
) -> (String, bool) {
    let mut builder = LimitedTextBuilder::new(budget.max_chars);
    let mut wrote_sheet = false;
    if !builder.push("# XLSX preview") {
        return (builder.finish(), true);
    }

    for sheet in sheets {
        if sheet.rows.is_empty() {
            continue;
        }
        wrote_sheet = true;
        if !builder.push(&format!(
            "\n\n## Sheet: {}\n\n",
            escape_markdown_cell(&sheet.name)
        )) {
            return (builder.finish(), true);
        }
        for line in sheet_to_markdown_lines(sheet, shared_strings) {
            if !builder.push(&line) || !builder.push("\n") {
                return (builder.finish(), true);
            }
        }
    }

    if !wrote_sheet {
        let mut empty_builder = LimitedTextBuilder::new(budget.max_chars);
        let truncated = !empty_builder.push("No worksheet text extracted");
        return (empty_builder.finish(), truncated);
    }
    let truncated = builder.truncated;
    (builder.finish().trim_end().to_string(), truncated)
}

fn sheet_to_markdown_lines(
    sheet: &PreviewSheet,
    shared_strings: &HashMap<usize, String>,
) -> Vec<String> {
    let col_count = sheet.rows.iter().map(Vec::len).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for (row_index, row) in sheet.rows.iter().enumerate() {
        if row_index == 1 {
            lines.push(format!("| {} |", vec!["---"; col_count].join(" | ")));
        }
        let cells: Vec<String> = (0..col_count)
            .map(|index| {
                row.get(index)
                    .map(|cell| cell_to_text(cell, shared_strings))
                    .unwrap_or_default()
            })
            .map(|cell| escape_markdown_cell(&cell))
            .collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    if sheet.rows.len() == 1 {
        lines.push(format!("| {} |", vec!["---"; col_count].join(" | ")));
    }
    lines
}

fn cell_to_text(cell: &PreviewCell, shared_strings: &HashMap<usize, String>) -> String {
    match cell {
        PreviewCell::Text(value) => value.clone(),
        PreviewCell::SharedString(index) => shared_strings
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("#SHARED_STRING[{index}]")),
    }
}

fn escape_markdown_cell(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .replace('|', "\\|")
        .trim()
        .to_string()
}

struct LimitedTextBuilder {
    max_chars: usize,
    current_chars: usize,
    output: String,
    truncated: bool,
}

impl LimitedTextBuilder {
    fn new(max_chars: usize) -> Self {
        Self {
            max_chars: max_chars.max(1),
            current_chars: 0,
            output: String::new(),
            truncated: false,
        }
    }

    fn push(&mut self, text: &str) -> bool {
        let remaining = self.max_chars.saturating_sub(self.current_chars);
        if remaining == 0 {
            self.truncated = true;
            return false;
        }
        let text_chars = text.chars().count();
        if text_chars <= remaining {
            self.output.push_str(text);
            self.current_chars += text_chars;
            return true;
        }
        self.output.extend(text.chars().take(remaining));
        self.current_chars = self.max_chars;
        self.truncated = true;
        false
    }

    fn finish(self) -> String {
        self.output
    }
}

fn column_index_from_cell_ref(cell_ref: &str) -> Option<usize> {
    let mut value = 0usize;
    let mut saw_letter = false;
    for byte in cell_ref.bytes() {
        let upper = byte.to_ascii_uppercase();
        if upper.is_ascii_uppercase() {
            saw_letter = true;
            value = value * 26 + usize::from(upper - b'A' + 1);
        } else if saw_letter {
            break;
        }
    }
    saw_letter.then_some(value)
}

fn attr_value(e: &BytesStart<'_>, local_name: &[u8]) -> Result<Option<String>, String> {
    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|error| format!("XML attribute error: {error}"))?;
        if is_local_name(attr.key.as_ref(), local_name) {
            return attr
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, e.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|error| format!("XML attribute decode error: {error}"));
        }
    }
    Ok(None)
}

fn is_local_name(name: &[u8], expected: &[u8]) -> bool {
    let local = name
        .iter()
        .rposition(|byte| *byte == b':')
        .map(|index| &name[index + 1..])
        .unwrap_or(name);
    local == expected
}

fn unescape_text(e: &BytesText<'_>) -> Result<String, String> {
    let decoded = e
        .decode()
        .map_err(|error| format!("XML text decode error: {error}"))?;
    quick_xml::escape::unescape(&decoded)
        .map(|value| value.into_owned())
        .map_err(|error| format!("XML text unescape error: {error}"))
}

fn unescape_general_ref(e: &BytesRef<'_>) -> Result<String, String> {
    let decoded = e
        .decode()
        .map_err(|error| format!("XML reference decode error: {error}"))?;
    let wrapped = format!("&{decoded};");
    quick_xml::escape::unescape(&wrapped)
        .map(|value| value.into_owned())
        .map_err(|error| format!("XML reference unescape error: {error}"))
}
