use super::*;
use calamine::{open_workbook, Reader, Xls};

/// Inspect container identities before selecting exactly one content parser.
pub fn detect_office_format(path: &PathBuf) -> Result<(&'static str, ParserBackend), String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut signature = [0; 8];
    file.read_exact(&mut signature).map_err(|e| e.to_string())?;
    file.rewind().map_err(|e| e.to_string())?;
    if signature.starts_with(b"PK") {
        let zip = ZipArchive::new(file).map_err(|e| e.to_string())?;
        let mut formats = Vec::new();
        for (entry, kind, backend) in [
            (
                "word/document.xml",
                ".docx",
                ParserBackend::RustOfficeOxideV2,
            ),
            (
                "ppt/presentation.xml",
                ".pptx",
                ParserBackend::RustOfficeOxideV2,
            ),
            ("xl/workbook.xml", ".xlsx", ParserBackend::RustXlsxBoundedV2),
        ] {
            if zip.index_for_name(entry).is_some() {
                formats.push((kind, backend));
            }
        }
        if formats.len() == 1 {
            return Ok(formats[0]);
        }
    } else if signature == [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1] {
        let cfb = office_oxide::cfb::CfbReader::new(file).map_err(|e| e.to_string())?;
        let mut formats = Vec::new();
        if cfb.has_stream("WordDocument") {
            formats.push((".doc", ParserBackend::RustLegacyOfficeV3));
        }
        if cfb.has_stream("PowerPoint Document") || cfb.has_stream("PP97_DUALSTORAGE") {
            formats.push((".ppt", ParserBackend::RustLegacyOfficeV3));
        }
        if cfb.has_stream("Workbook") || cfb.has_stream("Book") {
            formats.push((".xls", ParserBackend::RustXlsCalamineV3));
        }
        if formats.len() == 1 {
            return Ok(formats[0]);
        }
    }
    Err("Office container does not identify one supported format".into())
}

pub fn parse_xls(request: &OfficeParseRequest) -> FileContextOut {
    let result = (|| -> Result<_, calamine::XlsError> {
        let mut book: Xls<_> = open_workbook(&request.file_path)?;
        let max_sheets = positive_limit(&request.limits, "excel_max_sheets", 5);
        // Preserve XLS's existing header plus data-row budget.
        let max_rows = positive_limit(&request.limits, "excel_max_rows", 50).saturating_add(1);
        let max_columns = positive_limit(&request.limits, "excel_max_columns", 12);
        let max_chars = positive_limit(&request.limits, "document_excerpt_max_chars", 6000);
        let names = book.sheet_names().to_vec();
        let mut bounded = names.len() > max_sheets;
        let mut builder = content_builder(request, max_chars, "xls");
        for name in names.iter().take(max_sheets) {
            let formulas = if request.limits.contains_key("archive_output") {
                Some(book.worksheet_formula(name)?)
            } else {
                None
            };
            let range = book.worksheet_range(name)?;
            let (start_row, start_col) = range.start().unwrap_or((0, 0));
            bounded |= range.height() > max_rows || range.width() > max_columns;
            builder.push(
                &format!("## {}", escape_markdown_cell(name)),
                ContentUnitKind::Heading,
                vec![],
            );
            for (index, row) in range.rows().take(max_rows).enumerate() {
                let cells = row
                    .iter()
                    .take(max_columns)
                    .enumerate()
                    .map(|(column, cell)| {
                        let text = match cell {
                            calamine::Data::DateTime(value) => value
                                .as_datetime()
                                .map(|date| date.format("%Y-%m-%d %H:%M:%S").to_string())
                                .unwrap_or_else(|| cell.to_string()),
                            _ => cell.to_string(),
                        };
                        let text = if let Some(formula) = formulas
                            .as_ref()
                            .and_then(|f| {
                                f.get_value((start_row + index as u32, start_col + column as u32))
                            })
                            .filter(|f| !f.is_empty())
                        {
                            format!("公式：={formula}；缓存值：{text}")
                        } else {
                            text
                        };
                        escape_markdown_cell(&text)
                    })
                    .collect::<Vec<_>>();
                if cells.is_empty() {
                    continue;
                }
                let mut source = SourceRange::new(
                    SourceRangeUnit::SheetRow,
                    start_row as u64 + index as u64 + 1,
                    start_row as u64 + index as u64 + 2,
                    Some(name.chars().take(512).collect()),
                );
                source.column_start = Some(start_col as u64 + 1);
                source.column_end = Some(start_col as u64 + cells.len() as u64 + 1);
                builder.read(source.clone());
                if cells.iter().all(|cell| cell.trim().is_empty()) {
                    continue;
                }
                let line = format!("| {} |", cells.join(" | "));
                let kind = if line.contains("单位") {
                    ContentUnitKind::Unit
                } else if line.contains("合计") || line.contains("小计") {
                    ContentUnitKind::Total
                } else if index == 0 && range.height() > 1 {
                    ContentUnitKind::TableHeader
                } else {
                    ContentUnitKind::TableRow
                };
                if !builder.push(&line, kind, vec![source.clone()]) {
                    bounded = true;
                }
                if kind == ContentUnitKind::TableHeader {
                    builder.push(
                        &format!("| {} |", vec!["---"; cells.len()].join(" | ")),
                        kind,
                        vec![source],
                    );
                }
            }
        }
        let (content, audit, truncated) = builder.finish();
        Ok((content, audit, truncated || bounded))
    })();
    let (content, evidence_audit, truncated, error) = match result {
        Ok((text, audit, bounded)) => (text, Some(audit), bounded, None),
        Err(_) => (
            String::new(),
            None,
            false,
            Some("RUST_XLS_PARSE_FAILED".into()),
        ),
    };
    FileContextOut {
        file_path: request.file_path.to_string_lossy().into_owned(),
        file_type: request.file_type.clone(),
        content,
        evidence_audit,
        error,
        parser_backend: ParserBackend::RustXlsCalamineV3.as_str().into(),
        truncated,
    }
}
