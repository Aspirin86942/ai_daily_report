mod xml;

use crate::office::parts::PartBudget;
use crate::office::spreadsheet::{
    probe_order, render_selection, SheetSample, XlsxBudget, DIRECTORY_LIMIT, MAX_SAMPLE_CELLS,
};
use crate::office::OfficeContent;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::time::Instant;
use xml::{
    parse_workbook_relationships, parse_workbook_sheets, read_string_table, vertical_sum_formula,
    visit_sheet_rows, CellValue,
};
use zip::read::ZipArchive;

const PART_LIMIT: usize = 8 * 1024 * 1024;

pub(super) fn parse<R: Read + Seek>(
    mut parts: PartBudget,
    zip: &mut ZipArchive<R>,
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
        parts.deadline,
    )?;
    let relationships = parse_workbook_relationships(
        std::str::from_utf8(&rels).map_err(|e| e.to_string())?,
        parts.deadline,
    )?;
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
    let mut remaining_values = 16 * 1024 * 1024;
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
        let mut value_bytes = PART_LIMIT.min(remaining_values / (count - probed));
        let value_allowance = value_bytes;
        let limited = visit_sheet_rows(
            xml.as_slice(),
            1_048_576,
            16_384,
            Some(parts.deadline),
            partial.then_some(xml.len() as u64),
            |n, row| {
                let mut values = Vec::new();
                let mut row_bytes = 0;
                let mut formula = false;
                let mut sum = false;
                for (column, cell) in row {
                    if !cell.formula.is_empty() {
                        formula = true;
                        sum |= vertical_sum_formula(&cell.formula, n);
                    }
                    let value = match cell.value {
                        CellValue::Text(text) => Cow::Owned(text),
                        CellValue::SharedString(id) => match strings.get(&id) {
                            Some(text) => Cow::Borrowed(text.as_str()),
                            None if strings_partial => {
                                sample.truncated = true;
                                // A missing label could change the meaning of another value in this row.
                                return Ok(true);
                            }
                            None => return Err("referenced shared string is missing".into()),
                        },
                    };
                    if !value.is_empty() {
                        let cost = value.len() + size_of::<(usize, String)>();
                        row_bytes += cost;
                        if row_bytes > 2 * 1024 * 1024 {
                            sample.truncated = true;
                            return Ok(true);
                        }
                        if row_bytes > value_bytes {
                            return Ok(false);
                        }
                        values.push((column, value.into_owned()));
                    }
                }
                value_bytes -= row_bytes;
                sample.observe(n, values, formula, sum);
                Ok(true)
            },
        )?;
        remaining_values -= value_allowance - value_bytes;
        sample.truncated |= limited;
        samples.push((
            index,
            sample.finish(budget.max_columns, MAX_SAMPLE_CELLS / count),
        ));
    }
    Ok(render_selection(&names, samples, budget, "xlsx", truncated))
}
