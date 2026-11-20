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

fn xls_text(cell: &calamine::Data) -> String {
    match cell {
        calamine::Data::DateTime(value) => value
            .as_datetime()
            .map(|date| date.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| cell.to_string()),
        _ => cell.to_string(),
    }
}

pub fn parse_xls(request: &OfficeParseRequest) -> FileContextOut {
    let result = (|| -> Result<_, calamine::XlsError> {
        let mut scan = parts::PartBudget::new(request);
        let mut book: Xls<_> = open_workbook(&request.file_path)?;
        let max_sheets = positive_limit(&request.limits, "excel_max_sheets", 5);
        // Preserve XLS's existing header plus data-row budget.
        let max_rows = positive_limit(&request.limits, "excel_max_rows", 50).saturating_add(1);
        let max_columns = positive_limit(&request.limits, "excel_max_columns", 12);
        let max_chars = positive_limit(&request.limits, "document_excerpt_max_chars", 6000);
        let names = book.sheet_names();
        if !request.limits.contains_key("archive_output") {
            let budget = XlsxBudget {
                deadline: scan.deadline,
                max_sheets,
                max_rows,
                max_columns,
                max_chars,
            };
            scan.reserve_render_time();
            let order = spreadsheet::probe_order(&names, max_sheets);
            let count = order.len();
            let mut sheets = Vec::new();
            for (probed, index) in order.into_iter().enumerate() {
                if Instant::now() >= scan.deadline || scan.remaining == 0 {
                    break;
                }
                let name = &names[index];
                // Calamine returns BIFF cached values, including cached Unicode formula text.
                // The pinned XLS API returns an owned dense range. Move its strings into
                // the row sample instead of making a second copy of their contents.
                let mut range = book.worksheet_range(name)?;
                let (first_row, first_col) = range.start().unwrap_or_default();
                let allowance = scan.remaining / (count - probed);
                let mut spent = 0usize;
                let mut sheet = spreadsheet::SheetSample::new(name, max_rows);
                for offset in 0..range.height() {
                    let row = &mut range[offset];
                    let mut cells = Vec::new();
                    spent = spent.saturating_add(row.len());
                    if spent > allowance || Instant::now() >= scan.deadline {
                        sheet.truncated = true;
                        break;
                    }
                    for (column, cell) in row.iter_mut().enumerate() {
                        let text = match cell {
                            calamine::Data::Empty => continue,
                            calamine::Data::String(s) => std::mem::take(s),
                            other => xls_text(other),
                        };
                        spent = spent.saturating_add(text.len());
                        if !text.is_empty() {
                            cells.push((first_col as usize + column + 1, text));
                        }
                    }
                    if spent > allowance || Instant::now() >= scan.deadline {
                        sheet.truncated = true;
                        break;
                    }
                    sheet.observe(
                        u64::from(first_row) + offset as u64 + 1,
                        cells,
                        false,
                        false,
                    );
                }
                scan.remaining = scan.remaining.saturating_sub(spent);
                sheets.push((index, sheet.finish(max_columns)));
            }
            let (text, bounded, audit) = spreadsheet::render_selection(
                &names,
                sheets,
                &budget,
                "xls",
                Instant::now() >= scan.deadline,
            );
            return Ok((text, audit, bounded));
        }
        let mut bounded = names.len() > max_sheets;
        let mut builder = content_builder(request, max_chars, "xls");
        for name in names.iter().take(max_sheets) {
            let formulas = book.worksheet_formula(name)?;
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
                        let text = xls_text(cell);
                        let text = if let Some(formula) = formulas
                            .get_value((start_row + index as u32, start_col + column as u32))
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
