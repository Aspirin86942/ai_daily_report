use crate::office::parts::{
    attr_value, is_local_name, unescape_general_ref, unescape_text, MAX_XML_DEPTH,
};
use quick_xml::errors::SyntaxError;
use quick_xml::events::{BytesStart, Event};
use std::collections::HashMap;
use std::time::Instant;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
pub(super) struct SheetRef {
    pub(super) name: String,
    pub(super) rel_id: String,
}

pub(super) struct PreviewCell {
    pub(super) value: CellValue,
    pub(super) formula: String,
}

pub(super) enum CellValue {
    Text(String),
    SharedString(usize),
}

struct CellBuilder {
    capture: bool,
    start: u64,
    column_index: usize,
    shared: bool,
    text: String,
    formula: String,
    field: CellField,
}

enum CellField {
    Ignore,
    Text,
    Formula,
}

const MAX_TEXT_BYTES: usize = 128 * 1024;
const MAX_CELL_XML_BYTES: u64 = 1024 * 1024;
const MAX_ROW_XML_BYTES: u64 = 4 * 1024 * 1024;

/// Sample cheap XML events together. Large tokens and completed rows still
/// check the wall clock before decoding or emitting evidence.
struct XmlDeadline {
    end: Option<Instant>,
    events_left: u8,
    next_byte_check: u64,
}

impl XmlDeadline {
    fn new(end: Option<Instant>) -> Self {
        Self {
            end,
            events_left: 0,
            next_byte_check: 0,
        }
    }

    fn expired(&mut self, position: u64, boundary: bool) -> bool {
        let Some(end) = self.end else { return false };
        if self.events_left != 0 && position < self.next_byte_check && !boundary {
            self.events_left -= 1;
            return false;
        }
        self.events_left = u8::MAX;
        self.next_byte_check = position.saturating_add(16 * 1024);
        Instant::now() >= end
    }
}

pub(super) fn parse_workbook_sheets(
    xml: &str,
    max_sheets: usize,
    deadline: Instant,
) -> Result<(Vec<SheetRef>, bool), String> {
    let mut sheets = Vec::new();
    let finished = visit_directory(xml, b"sheet", deadline, |e| {
        let name = attr_value(&e, b"name")?.unwrap_or_else(|| "Sheet".to_string());
        let rel_id = attr_value(&e, b"id")?.unwrap_or_default();
        if !rel_id.is_empty() {
            if sheets.len() >= max_sheets {
                return Ok(false);
            }
            sheets.push(SheetRef { name, rel_id });
        }
        Ok(true)
    })?;
    Ok((sheets, !finished))
}

pub(super) fn parse_workbook_relationships(
    xml: &str,
    deadline: Instant,
) -> Result<HashMap<String, String>, String> {
    let mut relationships = HashMap::new();
    let finished = visit_directory(xml, b"Relationship", deadline, |e| {
        if let (Some(id), Some(target)) = (attr_value(&e, b"Id")?, attr_value(&e, b"Target")?) {
            let worksheet = !attr_value(&e, b"TargetMode")?
                .is_some_and(|m| m.eq_ignore_ascii_case("External"))
                && !target.contains([':', '\\'])
                && !target.starts_with("//")
                && attr_value(&e, b"Type")?
                    .is_none_or(|kind| kind.rsplit('/').next() == Some("worksheet"));
            if worksheet {
                relationships.insert(id, normalize_sheet_target(&target));
            }
        }
        Ok(true)
    })?;
    if !finished {
        return Err("XLSX relationship parsing reached its deadline".into());
    }
    Ok(relationships)
}

fn visit_directory(
    xml: &str,
    tag: &[u8],
    deadline: Instant,
    mut visit: impl FnMut(BytesStart<'_>) -> Result<bool, String>,
) -> Result<bool, String> {
    let mut xml = WorksheetXml::new(xml.as_bytes(), Some(deadline), None);
    xml.reader.config_mut().trim_text(true);
    while let Some(event) = xml.next(|_| false)? {
        match event {
            Event::Start(e) | Event::Empty(e) if is_local_name(e.name().as_ref(), tag) => {
                if !visit(e)? {
                    return Ok(false);
                }
            }
            Event::Eof => return Ok(true),
            _ => {}
        }
    }
    Ok(false)
}

fn normalize_sheet_target(target: &str) -> String {
    let target = target.trim_start_matches('/');
    if target.starts_with("xl/") {
        target.to_string()
    } else {
        format!("xl/{target}")
    }
}

struct WorksheetXml<'a> {
    reader: quick_xml::Reader<&'a [u8]>,
    clock: XmlDeadline,
    partial_at: Option<u64>,
    depth: usize,
}

impl<'a> WorksheetXml<'a> {
    fn new(input: &'a [u8], deadline: Option<Instant>, partial_at: Option<u64>) -> Self {
        Self {
            reader: quick_xml::Reader::from_reader(input),
            clock: XmlDeadline::new(deadline),
            partial_at,
            depth: 0,
        }
    }

    // An unfinished token or trailing character can be discarded only at a sampling boundary.
    fn next(
        &mut self,
        checkpoint: impl FnOnce(&Event<'_>) -> bool,
    ) -> Result<Option<Event<'a>>, String> {
        let result = self.reader.read_event();
        // For an unterminated tag quick-xml reports its opening position even after
        // consuming the entire bounded input. Check the actual BufRead boundary too.
        let at_cut = self.partial_at.is_some_and(|n| {
            self.reader.buffer_position() >= n || self.reader.get_ref().is_empty()
        });
        let event = match result {
            // The slice reader keeps its position at the opening tag for an
            // incomplete token. An Unclosed error still proves it reached EOF.
            Err(quick_xml::Error::Syntax(syntax))
                if self.partial_at.is_some()
                    && !matches!(
                        syntax,
                        SyntaxError::InvalidBangMarkup | SyntaxError::UnclosedDoctype
                    ) =>
            {
                Event::Eof
            }
            Err(
                quick_xml::Error::Syntax(_)
                | quick_xml::Error::IllFormed(quick_xml::errors::IllFormedError::UnclosedReference),
            ) if at_cut => Event::Eof,
            Err(e) => return Err(e.to_string()),
            Ok(Event::Text(e))
                if at_cut
                    && std::str::from_utf8(e.as_ref())
                        .is_err_and(|err| err.error_len().is_none()) =>
            {
                Event::Eof
            }
            Ok(Event::Eof) if self.partial_at.is_none() && self.depth != 0 => {
                return Err("unexpected end of worksheet XML".into());
            }
            Ok(event) => {
                match &event {
                    Event::Start(e) | Event::Empty(e) if e.len() > 512 * 1024 => {
                        return Err("spreadsheet XML element limit".into());
                    }
                    Event::Start(_) => {
                        self.depth += 1;
                        if self.depth > MAX_XML_DEPTH {
                            return Err("spreadsheet XML nesting limit".into());
                        }
                    }
                    Event::DocType(_) => {
                        return Err("spreadsheet XML document type is unsupported".into())
                    }
                    Event::End(_) => self.depth = self.depth.saturating_sub(1),
                    _ => {}
                }
                event
            }
        };
        let timed_out = self
            .clock
            .expired(self.reader.buffer_position(), checkpoint(&event));
        Ok((!timed_out).then_some(event))
    }
}

pub(super) fn visit_sheet_rows(
    input: &[u8],
    row_limit: usize,
    col_limit: usize,
    deadline: Option<Instant>,
    partial_at: Option<u64>,
    mut sink: impl FnMut(u64, Vec<(usize, PreviewCell)>) -> Result<bool, String>,
) -> Result<bool, String> {
    let mut xml = WorksheetXml::new(input, deadline, partial_at);
    let mut read_count = 0;
    let mut current_row_number = 0;
    let mut row_anchor = None;
    let mut last_column = 0;
    let mut current_row = None;
    let mut row_start = 0;
    let mut skip_row = false;
    let mut current_cell: Option<CellBuilder> = None;
    let mut truncated = false;
    if xml.clock.expired(0, true) {
        return Ok(true);
    }
    loop {
        let start = xml.reader.buffer_position();
        let Some(event) = xml
            .next(|event| {
                matches!(event, Event::Eof)
                    || matches!(event, Event::End(e) if is_local_name(e.name().as_ref(), b"row"))
            })
            .map_err(|e| format!("worksheet XML error: {e}"))?
        else {
            return Ok(true);
        };
        let end = xml.reader.buffer_position();
        if current_cell
            .as_ref()
            .is_some_and(|cell| end - cell.start > MAX_CELL_XML_BYTES)
            || (current_row.is_some() && end - row_start > MAX_ROW_XML_BYTES)
        {
            skip_row = true;
        }
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e) if is_local_name(e.name().as_ref(), b"row") => {
                if read_count >= row_limit {
                    truncated = true;
                    break;
                }
                if current_row.is_some() {
                    return Err("nested worksheet row".into());
                }
                row_start = start;
                skip_row = false;
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
                    truncated |= skip_row;
                    if !skip_row && !row.is_empty() && !sink(current_row_number, row)? {
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
                    if row_anchor.is_some_and(|n| n != row) {
                        return Err("worksheet row disagrees with cell reference".into());
                    }
                    current_row_number = row;
                    row_anchor = Some(row);
                    column
                } else {
                    last_column + 1
                };
                last_column = column_index;
                truncated |= column_index > col_limit;
                let capture = !skip_row && column_index <= col_limit;
                if current_cell.is_some() {
                    return Err("nested worksheet cell".into());
                }
                if !empty {
                    current_cell = Some(CellBuilder {
                        capture,
                        start,
                        column_index,
                        shared: attr_value(&e, b"t")?.as_deref() == Some("s"),
                        text: String::new(),
                        formula: String::new(),
                        field: CellField::Ignore,
                    });
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"c") => {
                if let (Some(cell), Some(row)) = (current_cell.take(), current_row.as_mut()) {
                    if cell.capture && !skip_row {
                        let column = cell.column_index;
                        if let Some(cell) = finish_cell(cell) {
                            row.push((column, cell));
                        }
                    }
                }
            }
            Event::Start(tag) => {
                if let Some(cell) = current_cell.as_mut() {
                    match tag.local_name().as_ref() {
                        b"v" | b"t" => cell.field = CellField::Text,
                        b"f" => cell.field = CellField::Formula,
                        _ => {}
                    }
                }
            }
            Event::End(tag) if matches!(tag.local_name().as_ref(), b"v" | b"t" | b"f") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.field = CellField::Ignore;
                }
            }
            event @ (Event::Text(_) | Event::GeneralRef(_)) if !skip_row => {
                if let Some(cell) = current_cell.as_mut().filter(|cell| cell.capture) {
                    let out = match cell.field {
                        CellField::Ignore => continue,
                        CellField::Text => &mut cell.text,
                        CellField::Formula => &mut cell.formula,
                    };
                    skip_row = !append_text(out, event)?;
                }
            }
            Event::Eof => break,
            _ => {}
        }
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

fn append_text(target: &mut String, event: Event<'_>) -> Result<bool, String> {
    let text = match event {
        Event::Text(ref text) if text.len() > MAX_TEXT_BYTES => return Ok(false),
        Event::Text(text) => unescape_text(&text)?,
        Event::GeneralRef(reference) if reference.len() > 32 => {
            return Err("invalid XML reference".into())
        }
        Event::GeneralRef(reference) => unescape_general_ref(&reference)?,
        _ => unreachable!("text event"),
    };
    if target.len() + text.len() > MAX_TEXT_BYTES {
        return Ok(false);
    }
    target.push_str(&text);
    Ok(true)
}

fn finish_cell(cell: CellBuilder) -> Option<PreviewCell> {
    let text = cell.text.trim();
    if text.is_empty() && cell.formula.is_empty() {
        return None;
    }
    let shared = if cell.shared { text.parse().ok() } else { None };
    let value = shared.map_or_else(|| CellValue::Text(text.to_owned()), CellValue::SharedString);
    Some(PreviewCell {
        value,
        formula: cell.formula,
    })
}

pub(super) fn read_string_table(
    input: &[u8],
    limit: usize,
    deadline: Option<Instant>,
    partial_at: Option<u64>,
) -> Result<(HashMap<usize, String>, bool), String> {
    let mut xml = WorksheetXml::new(input, deadline, partial_at);
    let mut result = HashMap::new();
    let mut current_index = 0usize;
    let mut in_si = false;
    let mut in_text = false;
    let mut current_text = String::new();
    let mut string_start = 0;
    let mut skip_string = false;
    let mut limited = false;
    if xml.clock.expired(0, true) {
        return Ok((result, true));
    }
    loop {
        if current_index >= limit {
            return Ok((result, true));
        }
        let start = xml.reader.buffer_position();
        let Some(event) = xml
            .next(|event| matches!(event, Event::Eof))
            .map_err(|e| format!("sharedStrings XML error: {e}"))?
        else {
            return Ok((result, true));
        };
        if in_si && xml.reader.buffer_position() - string_start > MAX_CELL_XML_BYTES {
            skip_string = true;
        }
        match event {
            Event::Empty(e) if is_local_name(e.name().as_ref(), b"si") => {
                result.insert(current_index, String::new());
                current_index += 1;
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"si") => {
                if in_si {
                    return Err("nested XLSX shared string".into());
                }
                in_si = true;
                string_start = start;
                skip_string = false;
                current_text.clear();
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"si") => {
                if skip_string {
                    limited = true;
                } else {
                    result.insert(current_index, std::mem::take(&mut current_text));
                }
                current_index += 1;
                in_si = false;
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"t") && in_si => {
                in_text = true;
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"t") => {
                in_text = false;
            }
            event @ (Event::Text(_) | Event::GeneralRef(_)) if in_si && in_text && !skip_string => {
                skip_string = !append_text(&mut current_text, event)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((result, limited || partial_at.is_some()))
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
