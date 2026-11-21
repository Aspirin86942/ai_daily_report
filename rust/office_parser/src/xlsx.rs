mod xml;

use crate::parts::{resolve_zip_member, PartBudget};
use crate::spreadsheet::{
    is_header_row, probe_order, render_selection, SheetSample, XlsxBudget, DIRECTORY_LIMIT,
};
use crate::{escape_markdown_cell, OfficeContent, OfficeInput, OfficeResult};
use ai_daily_worker_contract::{ContentUnitKind, SourceRange, SourceRangeUnit};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::time::Instant;
use xml::{
    parse_workbook_relationships, parse_workbook_sheets, read_string_table, vertical_sum_formula,
    visit_sheet_rows, PreviewCell,
};
use zip::read::ZipArchive;

const PART_LIMIT: usize = 8 * 1024 * 1024;

pub(super) fn parse(
    request: &OfficeInput<'_>,
    parts: PartBudget,
    budget: &XlsxBudget,
) -> OfficeResult {
    let failure = |message: String, retryable| {
        (
            format!("RUST_XLSX_BOUNDED_PARSE_FAILED: {message}"),
            retryable,
        )
    };
    let file = File::open(request.path).map_err(|e| failure(format!("I/O error: {e}"), true))?;
    let mut zip = ZipArchive::new(file).map_err(|e| failure(format!("ZIP error: {e}"), false))?;
    let parsed = if request.archival {
        archive_xlsx_rows(request, &mut zip, budget)
    } else {
        daily_xlsx(parts, &mut zip, budget)
    };
    parsed.map_err(|message| failure(message, true))
}

fn daily_xlsx(
    mut parts: PartBudget,
    zip: &mut ZipArchive<File>,
    budget: &XlsxBudget,
) -> Result<OfficeContent, String> {
    parts.reserve_render_time();
    let (book, book_partial) = parts.read(zip, "xl/workbook.xml", 2 * 1024 * 1024)?;
    let (rels, rels_partial) = parts.read(zip, "xl/_rels/workbook.xml.rels", 2 * 1024 * 1024)?;
    if book_partial || rels_partial {
        return Ok(render_selection(&[], vec![], budget, "xlsx", true));
    }
    let (refs, mut truncated) = parse_workbook_sheets(
        std::str::from_utf8(&book).map_err(|e| e.to_string())?,
        DIRECTORY_LIMIT,
    )?;
    let relationships =
        parse_workbook_relationships(std::str::from_utf8(&rels).map_err(|e| e.to_string())?, true)?;
    let names: Vec<_> = refs.iter().map(|s| s.name.clone()).collect();
    let (strings, strings_partial) = if zip
        .file_names()
        .any(|n| n.eq_ignore_ascii_case("xl/sharedStrings.xml"))
    {
        let (xml, partial) = parts.read(zip, "xl/sharedStrings.xml", PART_LIMIT)?;
        read_string_table(
            xml.as_slice(),
            131_072,
            Some(parts.deadline),
            partial.then_some(xml.len() as u64),
        )?
    } else {
        (HashMap::new(), false)
    };
    truncated |= strings_partial;
    let mut samples = Vec::new();
    let order = probe_order(&names, budget.max_sheets);
    let count = order.len();
    for (probed, index) in order.into_iter().enumerate() {
        if Instant::now() >= parts.deadline || parts.remaining == 0 {
            truncated = true;
            break;
        }
        let sheet = &refs[index];
        let Some(path) = relationships.get(&sheet.rel_id) else {
            truncated = true;
            continue;
        };
        let allowance = PART_LIMIT.min(parts.remaining / (count - probed));
        let (xml, partial) = parts.read(zip, path, allowance)?;
        let mut sample = SheetSample::new(&sheet.name, budget.max_rows);
        let mut value_bytes = PART_LIMIT;
        let limited = visit_sheet_rows(
            xml.as_slice(),
            1_048_576,
            16_384,
            false,
            Some(parts.deadline),
            partial.then_some(xml.len() as u64),
            |n, row| {
                let mut values = Vec::new();
                let mut formula = false;
                let mut sum = false;
                for (column, cell) in &row {
                    let cached = match cell {
                        PreviewCell::Formula(f, cached) => {
                            formula = true;
                            sum |= vertical_sum_formula(f, n);
                            cached.as_ref()
                        }
                        value => value,
                    };
                    let value = match cached {
                        PreviewCell::Text(text) => text,
                        PreviewCell::SharedString(id) => match strings.get(id) {
                            Some(text) => text,
                            None if strings_partial => {
                                sample.truncated = true;
                                // A missing label could change the meaning of another value in this row.
                                return Ok(true);
                            }
                            None => return Err("referenced shared string is missing".into()),
                        },
                        PreviewCell::Formula(..) => unreachable!(),
                    };
                    if !value.is_empty() {
                        if value.len() > value_bytes {
                            return Ok(false);
                        }
                        value_bytes -= value.len();
                        values.push((*column, value.clone()));
                    }
                }
                sample.observe(n, values, formula, sum);
                Ok(true)
            },
        )?;
        sample.truncated |= limited;
        samples.push((index, sample.finish(budget.max_columns)));
    }
    Ok(render_selection(&names, samples, budget, "xlsx", truncated))
}

fn read_zip_text<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, String> {
    let name = resolve_zip_member(archive, name)?;
    let mut entry = archive
        .by_name(&name)
        .map_err(|error| format!("ZIP entry {name} error: {error}"))?;
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .map_err(|error| format!("ZIP entry {name} read error: {error}"))?;
    Ok(text)
}

fn archive_xlsx_rows<R: Read + Seek>(
    request: &OfficeInput<'_>,
    archive: &mut ZipArchive<R>,
    budget: &XlsxBudget,
) -> Result<OfficeContent, String> {
    let workbook = read_zip_text(archive, "xl/workbook.xml")?;
    let (sheets, mut truncated) = parse_workbook_sheets(&workbook, budget.max_sheets)?;
    let rels = read_zip_text(archive, "xl/_rels/workbook.xml.rels")?;
    let relationships = parse_workbook_relationships(&rels, false)?;
    let strings = if let Ok(name) = resolve_zip_member(archive, "xl/sharedStrings.xml") {
        let entry = archive.by_name(&name).map_err(|e| e.to_string())?;
        read_string_table(BufReader::new(entry), usize::MAX, None, None)?.0
    } else {
        HashMap::new()
    };
    let mut builder = request.builder();
    builder.push("# XLSX archive", ContentUnitKind::Heading, vec![]);
    for sheet in sheets {
        let Some(path) = relationships.get(&sheet.rel_id) else {
            truncated = true;
            continue;
        };
        if !builder.push(
            &format!("## Sheet: {}", escape_markdown_cell(&sheet.name)),
            ContentUnitKind::Heading,
            vec![],
        ) {
            truncated = true;
            break;
        }
        let mut first = true;
        let member = resolve_zip_member(archive, path)?;
        let entry = archive.by_name(&member).map_err(|e| e.to_string())?;
        let limited = visit_sheet_rows(
            BufReader::new(entry),
            budget.max_rows,
            budget.max_columns,
            true,
            None,
            None,
            |number, row| {
                let count = row.iter().map(|(col, _)| *col).max().unwrap_or(0);
                let mut cells = vec![String::new(); count];
                for (col, cell) in row {
                    cells[col - 1] = escape_markdown_cell(&cell_to_text(&cell, &strings));
                }
                if cells.iter().any(|value| value.contains("#SHARED_STRING[")) {
                    return Err("referenced shared string is missing".into());
                }
                let kind = if first || is_header_row(&cells) {
                    ContentUnitKind::TableHeader
                } else {
                    ContentUnitKind::TableRow
                };
                first = false;
                let mut range = SourceRange::new(
                    SourceRangeUnit::SheetRow,
                    number,
                    number + 1,
                    Some(sheet.name.clone()),
                );
                range.column_start = Some(1);
                range.column_end = Some(cells.len() as u64 + 1);
                Ok(builder.push(&format!("| {} |", cells.join(" | ")), kind, vec![range]))
            },
        )?;
        truncated |= limited;
        if limited {
            break;
        }
    }
    let (content, audit, bounded) = builder.finish();
    Ok((content, audit, truncated || bounded))
}

fn cell_to_text(cell: &PreviewCell, shared_strings: &HashMap<usize, String>) -> String {
    match cell {
        PreviewCell::Text(value) => value.clone(),
        PreviewCell::Formula(formula, cached) => format!(
            "公式：={formula}；缓存值：{}",
            cell_to_text(cached, shared_strings)
        ),
        PreviewCell::SharedString(index) => shared_strings
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("#SHARED_STRING[{index}]")),
    }
}
