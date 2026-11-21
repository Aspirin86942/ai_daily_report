use crate::parts::{attr_value, is_local_name, unescape_general_ref, unescape_text};
use quick_xml::events::Event;
use std::collections::HashMap;
use std::io::Cursor;
use std::time::Instant;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
pub(super) struct SheetRef {
    pub(super) name: String,
    pub(super) rel_id: String,
}

#[derive(Debug, Clone)]
pub(super) enum PreviewCell {
    Text(String),
    SharedString(usize),
    Formula(String, Box<PreviewCell>),
}

struct CellBuilder {
    capture: bool,
    column_index: usize,
    cell_type: String,
    text: String,
    formula: String,
    in_value: bool,
    in_text: bool,
    in_formula: bool,
}

pub(super) fn parse_workbook_sheets(
    xml: &str,
    max_sheets: usize,
) -> Result<(Vec<SheetRef>, bool), String> {
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

pub(super) fn parse_workbook_relationships(
    xml: &str,
    daily: bool,
) -> Result<HashMap<String, String>, String> {
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
                    let worksheet = !daily
                        || (!attr_value(&e, b"TargetMode")?
                            .is_some_and(|m| m.eq_ignore_ascii_case("External"))
                            && !target.contains([':', '\\'])
                            && !target.starts_with("//")
                            && attr_value(&e, b"Type")?
                                .is_none_or(|kind| kind.rsplit('/').next() == Some("worksheet")));
                    if worksheet {
                        relationships.insert(id, normalize_sheet_target(&target));
                    }
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

// quick-xml may yield the last text token before noticing that its enclosing row/string
// is unfinished. Only an actual sampling boundary may discard that incomplete token.
fn worksheet_event<'a>(
    reader: &mut quick_xml::Reader<impl std::io::BufRead>,
    buf: &'a mut Vec<u8>,
    partial_at: Option<u64>,
    depth: &mut usize,
) -> Result<Event<'a>, String> {
    let result = reader.read_event_into(buf);
    // For an unterminated tag quick-xml reports its opening position even after
    // consuming the entire bounded input. Check the actual BufRead boundary too.
    let at_cut = partial_at.is_some_and(|n| {
        reader.buffer_position() >= n
            || std::io::BufRead::fill_buf(reader.get_mut()).is_ok_and(|rest| rest.is_empty())
    });
    match result {
        Err(
            quick_xml::Error::Syntax(_)
            | quick_xml::Error::IllFormed(quick_xml::errors::IllFormedError::UnclosedReference),
        ) if at_cut => Ok(Event::Eof),
        Err(e) => Err(e.to_string()),
        Ok(Event::Text(e))
            if at_cut
                && std::str::from_utf8(e.as_ref()).is_err_and(|err| err.error_len().is_none()) =>
        {
            Ok(Event::Eof)
        }
        Ok(Event::Eof) if partial_at.is_none() && *depth != 0 => {
            Err("unexpected end of worksheet XML".into())
        }
        Ok(event) => {
            match &event {
                Event::Start(_) => *depth += 1,
                Event::End(_) => *depth = depth.saturating_sub(1),
                _ => {}
            }
            Ok(event)
        }
    }
}

pub(super) fn visit_sheet_rows(
    input: impl std::io::BufRead,
    row_limit: usize,
    col_limit: usize,
    archival: bool,
    deadline: Option<Instant>,
    partial_at: Option<u64>,
    mut sink: impl FnMut(u64, Vec<(usize, PreviewCell)>) -> Result<bool, String>,
) -> Result<bool, String> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut depth = 0;
    let mut read_count = 0;
    let mut current_row_number = 0;
    let mut row_anchor = None;
    let mut last_column = 0;
    let mut current_row = None;
    let mut current_cell: Option<CellBuilder> = None;
    let mut truncated = false;
    loop {
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Ok(true);
        }
        let event = worksheet_event(&mut reader, &mut buf, partial_at, &mut depth)
            .map_err(|e| format!("worksheet XML error: {e}"))?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e)
                if is_local_name(e.name().as_ref(), b"row") && !(archival && empty) =>
            {
                if read_count >= row_limit {
                    truncated = true;
                    break;
                }
                row_anchor = attr_value(&e, b"r")?
                    .map(|v| {
                        v.parse::<u64>()
                            .ok()
                            .filter(|n| (1..=1_048_576).contains(n))
                            .ok_or("invalid worksheet row number")
                    })
                    .transpose()?;
                current_row_number = row_anchor.unwrap_or(current_row_number + 1);
                read_count += 1;
                last_column = 0;
                current_row = (!empty).then(Vec::new);
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"row") => {
                if let Some(row) = current_row.take() {
                    if !row.is_empty() && !sink(current_row_number, row)? {
                        return Ok(true);
                    }
                }
            }
            Event::Start(e) | Event::Empty(e)
                if is_local_name(e.name().as_ref(), b"c") && current_row.is_some() =>
            {
                let column_index = if let Some(address) = attr_value(&e, b"r")? {
                    let (column, row) =
                        cell_position(&address).ok_or("invalid worksheet cell reference")?;
                    if !archival {
                        if row_anchor.is_some_and(|n| n != row) {
                            return Err("worksheet row disagrees with cell reference".into());
                        }
                        current_row_number = row;
                        row_anchor = Some(row);
                    }
                    column
                } else {
                    last_column + 1
                };
                last_column = column_index;
                let capture = column_index <= col_limit;
                truncated |= !capture;
                if !empty {
                    current_cell = Some(CellBuilder {
                        capture,
                        column_index,
                        cell_type: attr_value(&e, b"t")?.unwrap_or_default(),
                        text: String::new(),
                        formula: String::new(),
                        in_value: false,
                        in_text: false,
                        in_formula: false,
                    });
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"c") => {
                if let (Some(mut cell), Some(row)) = (current_cell.take(), current_row.as_mut()) {
                    if cell.capture {
                        let formula = std::mem::take(&mut cell.formula);
                        let col = cell.column_index;
                        let value = finish_cell(cell);
                        if !formula.is_empty() {
                            row.push((
                                col,
                                PreviewCell::Formula(
                                    formula,
                                    Box::new(
                                        value.unwrap_or_else(|| PreviewCell::Text(String::new())),
                                    ),
                                ),
                            ));
                        } else if let Some(value) = value {
                            row.push((col, value));
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
            Event::Start(e) if is_local_name(e.name().as_ref(), b"f") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_formula = true;
                    if archival {
                        for key in [b"t".as_slice(), b"si", b"ref"] {
                            if let Some(value) = attr_value(&e, key)? {
                                cell.formula.push_str(&format!(
                                    "[{}={value}]",
                                    String::from_utf8_lossy(key)
                                ));
                            }
                        }
                    }
                }
            }
            Event::Empty(e) if is_local_name(e.name().as_ref(), b"f") && archival => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.formula.push_str("[formula]");
                    for key in [b"t".as_slice(), b"si", b"ref"] {
                        if let Some(value) = attr_value(&e, key)? {
                            cell.formula
                                .push_str(&format!("[{}={value}]", String::from_utf8_lossy(key)));
                        }
                    }
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"f") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_formula = false;
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
                    } else if cell.capture && cell.in_formula {
                        cell.formula.push_str(&unescape_text(&e)?);
                    }
                }
            }
            Event::GeneralRef(e) => {
                if let Some(cell) = current_cell.as_mut() {
                    if cell.capture && (cell.in_value || cell.in_text) {
                        cell.text.push_str(&unescape_general_ref(&e)?);
                    } else if cell.capture && cell.in_formula {
                        cell.formula.push_str(&unescape_general_ref(&e)?);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(truncated || partial_at.is_some())
}

pub(super) fn vertical_sum_formula(formula: &str, row: u64) -> bool {
    let normalized = formula.replace([' ', '$'], "").to_ascii_uppercase();
    let start = normalized
        .find("SUM(")
        .map(|position| position + 4)
        .or_else(|| normalized.find("SUBTOTAL(").map(|position| position + 9));
    let Some(start) = start else {
        return false;
    };
    let Some(end) = normalized[start..].find(')') else {
        return false;
    };
    let range = normalized[start..start + end]
        .rsplit(',')
        .next()
        .unwrap_or("");
    let Some((from, to)) = range.split_once(':') else {
        return false;
    };
    fn reference(value: &str) -> Option<(&str, u64)> {
        let column_len = value.bytes().take_while(u8::is_ascii_alphabetic).count();
        let (column, number) = value.split_at(column_len);
        (!column.is_empty())
            .then(|| number.parse::<u64>().ok().map(|n| (column, n)))
            .flatten()
    }
    match (reference(from), reference(to)) {
        (Some((first_col, first)), Some((last_col, last))) => {
            first_col == last_col && first > 0 && first < last && last < row
        }
        _ => false,
    }
}

fn finish_cell(cell: CellBuilder) -> Option<PreviewCell> {
    let text = cell.text.trim();
    if text.is_empty() {
        return None;
    }
    if cell.cell_type == "s" {
        if let Ok(index) = text.parse() {
            return Some(PreviewCell::SharedString(index));
        }
    }
    Some(PreviewCell::Text(text.to_owned()))
}
pub(super) fn read_string_table(
    input: impl std::io::BufRead,
    limit: usize,
    deadline: Option<Instant>,
    partial_at: Option<u64>,
) -> Result<(HashMap<usize, String>, bool), String> {
    let mut reader = quick_xml::Reader::from_reader(input);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut result = HashMap::new();
    let mut depth = 0;
    let mut current_index = 0usize;
    let mut in_si = false;
    let mut in_text = false;
    let mut current_text = String::new();
    loop {
        if deadline.is_some_and(|end| Instant::now() >= end) || current_index >= limit {
            return Ok((result, true));
        }
        match worksheet_event(&mut reader, &mut buf, partial_at, &mut depth)
            .map_err(|e| format!("sharedStrings XML error: {e}"))?
        {
            Event::Empty(e) if is_local_name(e.name().as_ref(), b"si") => {
                result.insert(current_index, String::new());
                current_index += 1;
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"si") => {
                in_si = true;
                current_text.clear();
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"si") => {
                result.insert(current_index, std::mem::take(&mut current_text));
                current_index += 1;
                in_si = false;
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
    Ok((result, partial_at.is_some()))
}

fn cell_position(address: &str) -> Option<(usize, u64)> {
    let width = address.bytes().take_while(u8::is_ascii_alphabetic).count();
    if !(1..=3).contains(&width) {
        return None;
    }
    let (letters, digits) = address.split_at(width);
    let mut column = 0;
    for c in letters.bytes() {
        column = column * 26 + usize::from(c.to_ascii_uppercase() - b'A' + 1);
    }
    let row = digits.parse::<u64>().ok()?;
    (column <= 16_384 && (1..=1_048_576).contains(&row)).then_some((column, row))
}
