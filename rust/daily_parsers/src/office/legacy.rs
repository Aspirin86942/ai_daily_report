use super::parts::{self, CheckedReader};
use super::spreadsheet::{self, XlsxBudget};
use super::{check_time, OfficeContent};
use calamine::{Reader, Xls};
use office_oxide::cfb::{CfbReader, EntryType};
use std::io::{Cursor, Read, Seek};
use std::time::Instant;

const WORKBOOK_NAMES: [&str; 4] = ["Workbook", "Book", "WORKBOOK", "BOOK"];
const CATALOG_READ_LIMIT: usize = 24 * 1024 * 1024;
const STREAM_BYTES: usize = 32 * 1024 * 1024;

pub(super) enum CompoundPayload {
    Workbook(Vec<u8>),
    Document(&'static str, Vec<u8>),
    Disabled,
}

pub(super) fn read_compound_file(
    mut reader: CheckedReader<impl Read + Seek>,
    deadline: Instant,
    legacy_enabled: bool,
) -> Result<CompoundPayload, String> {
    let mut header = [0; 60];
    reader.read_exact(&mut header).map_err(|e| e.to_string())?;
    if !matches!(
        (word_at(&header, 26), word_at(&header, 30)),
        (3, 9) | (4, 12)
    ) || word_at(&header, 32) != 6
        || dword(&header, 56) != 4096
    {
        return Err("invalid compound sector size".into());
    }
    reader.rewind().map_err(|e| e.to_string())?;
    // The reader charges real catalog I/O, including FAT chains and the mini
    // stream that CfbReader loads eagerly. Keep the unused total budget for
    // the chosen streams. Unrelated regular streams are never opened.
    let budget = reader.read_budget();
    let total = budget.get();
    let catalog = total.min(CATALOG_READ_LIMIT);
    budget.set(catalog);
    let mut cfb = CfbReader::new(reader).map_err(|e| e.to_string())?;
    budget.set(total - (catalog - budget.get()));
    let word = cfb.find_entry("WordDocument");
    let slides = ["PowerPoint Document", "PP97_DUALSTORAGE"]
        .into_iter()
        .find_map(|name| cfb.find_entry(name).map(|index| (name, index)));
    let workbook = WORKBOOK_NAMES.into_iter().find_map(|name| {
        cfb.entries()
            .iter()
            .position(|entry| entry.entry_type == EntryType::Stream && entry.name == name)
            .map(|index| (name, index))
    });
    let mut remaining = STREAM_BYTES;
    match (word, slides, workbook) {
        (None, None, Some((name, index))) => {
            let workbook = read_stream(&mut cfb, index, &mut remaining)?;
            validate_workbook(&workbook, deadline)?;
            // Calamine 0.37 requires a root mini stream for CFB v4.
            let file = super::compound::pack(vec![(name, workbook), ("Contents", vec![0])])?;
            Ok(CompoundPayload::Workbook(file))
        }
        (Some(index), None, None) if legacy_enabled => {
            let word = read_stream(&mut cfb, index, &mut remaining)?;
            let table_names = if word.get(0x0b).is_some_and(|flags| flags & 2 != 0) {
                ["1Table", "0Table"]
            } else {
                ["0Table", "1Table"]
            };
            let table = table_names
                .into_iter()
                .find_map(|name| cfb.find_entry(name).map(|index| (name, index)));
            let table = table
                .map(|(name, index)| {
                    read_stream(&mut cfb, index, &mut remaining).map(|bytes| (name, bytes))
                })
                .transpose()?;
            validate_doc(
                &word,
                deadline,
                table.as_ref().map(|(_, bytes)| bytes.as_slice()),
            )?;
            let streams = std::iter::once(("WordDocument", word))
                .chain(table)
                .collect();
            Ok(CompoundPayload::Document(
                ".doc",
                super::compound::pack(streams)?,
            ))
        }
        (None, Some((name, index)), None) if legacy_enabled => {
            let slides = read_stream(&mut cfb, index, &mut remaining)?;
            validate_ppt(&slides, deadline)?;
            Ok(CompoundPayload::Document(
                ".ppt",
                super::compound::pack(vec![(name, slides)])?,
            ))
        }
        (Some(_), None, None) | (None, Some(_), None) => Ok(CompoundPayload::Disabled),
        _ => Err("compound file must contain exactly one Office format".into()),
    }
}

fn read_stream<R: Read + Seek>(
    cfb: &mut CfbReader<R>,
    index: usize,
    remaining: &mut usize,
) -> Result<Vec<u8>, String> {
    let size = usize::try_from(cfb.entries()[index].stream_size)
        .map_err(|_| "compound stream size limit")?;
    *remaining = remaining
        .checked_sub(size)
        .ok_or("compound text stream budget")?;
    let data = cfb.read_stream_by_index(index).map_err(|e| e.to_string())?;
    if data.len() != size {
        return Err("incomplete compound stream".into());
    }
    Ok(data)
}

fn validate_doc(word: &[u8], deadline: Instant, table: Option<&[u8]>) -> Result<(), String> {
    // The pinned DOC decoder uses these FIB fields to find the CLX piece table.
    // Ordered character offsets bound the decoded text even for overlapping byte spans.
    let max_chars = word.get(0x4c..0x50).map_or(0, |v| dword(v, 0));
    if max_chars > 2_000_000 {
        return Err("DOC character limit".into());
    }
    if word.len() < 0x1aa {
        return Ok(());
    }
    let start = dword(word, 0x1a2) as usize;
    let size = dword(word, 0x1a6) as usize;
    if size == 0 {
        return Ok(());
    }
    let table = table.ok_or("missing DOC table")?;
    let mut clx = table
        .get(start..start + size)
        .ok_or("invalid DOC CLX boundary")?;
    while clx.first() == Some(&1) {
        check_time(deadline).map_err(|e| e.to_string())?;
        let header = clx.get(..3).ok_or("incomplete DOC property record")?;
        clx = clx
            .get(3 + usize::from(word_at(header, 1))..)
            .ok_or("incomplete DOC property data")?;
    }
    if clx.len() < 5 || clx[0] != 2 {
        return Err("invalid DOC piece table".into());
    }
    let size = dword(clx, 1) as usize;
    let clx = clx
        .get(5..5 + size)
        .filter(|v| v.len() >= 4)
        .ok_or("invalid DOC piece table size")?;
    let pieces = (clx.len() - 4) / 12;
    let offsets = clx[..4 * (pieces + 1)].chunks_exact(4).map(|b| dword(b, 0));
    if pieces > 65_536 || !offsets.is_sorted() {
        return Err("invalid DOC piece table positions".into());
    }
    Ok(())
}

fn validate_ppt(bytes: &[u8], deadline: Instant) -> Result<(), String> {
    let mut ends = vec![bytes.len()];
    let mut offset = 0;
    for _ in 0..120_000 {
        check_time(deadline).map_err(|e| e.to_string())?;
        while ends.last() == Some(&offset) {
            ends.pop();
        }
        let Some(&end) = ends.last() else {
            return Ok(());
        };
        let header = bytes
            .get(offset..end)
            .and_then(|rest| rest.get(..8))
            .ok_or("invalid PPT record header")?;
        let record_end = offset + 8 + dword(header, 4) as usize;
        if record_end > end {
            return Err("invalid PPT record length".into());
        }
        if header[0] & 0x0f == 0x0f {
            if ends.len() >= 48 {
                return Err("PPT record nesting limit".into());
            }
            offset += 8;
            ends.push(record_end);
        } else {
            offset = record_end;
        }
    }
    Err("PPT record count limit".into())
}

const MAX_DENSE_CELLS: usize = 240_000;
const MAX_STRING_COPIES: usize = 12 * 1024 * 1024;

pub(super) fn validate_workbook(workbook: &[u8], deadline: Instant) -> Result<(), String> {
    let mut remaining = workbook;
    let mut starts = Vec::new();
    let mut string_costs = Vec::new();
    let mut codepage = 1200;
    let mut name_records = 0;
    let mut records_left = 120_000;
    loop {
        let (kind, body) = biff_record(&mut remaining, &mut records_left, deadline)?;
        if kind == 0x000a {
            break;
        }
        if kind == 0x0042 && body.len() >= 2 {
            codepage = word_at(body, 0);
        }
        if kind == 0x00fc {
            let mut parts = vec![body.get(8..).ok_or("incomplete XLS shared string table")?];
            while remaining.len() > 4 && word_at(remaining, 0) == 0x003c {
                if parts.len() == 8192 {
                    return Err("XLS shared string continuation limit".into());
                }
                parts.push(biff_record(&mut remaining, &mut records_left, deadline)?.1);
            }
            string_costs = shared_string_costs(parts, codepage, deadline)?;
        }
        if kind == 0x0085 {
            if body.len() < 6 || starts.len() >= 1024 {
                return Err("invalid XLS sheet directory".into());
            }
            starts.push(dword(body, 0) as usize);
        }
        if kind == 0x0018 {
            name_records += 1;
            if name_records > 1024 || body.len() > 1024 {
                return Err("XLS defined name limit".into());
            }
        }
    }
    let global_end = workbook.len() - remaining.len();
    starts.sort_unstable();
    if starts.first().is_some_and(|&i| i < global_end)
        || starts.last().is_some_and(|&i| i >= workbook.len())
        || starts.windows(2).any(|v| v[0] == v[1])
    {
        return Err("invalid XLS sheet positions".into());
    }
    starts.push(workbook.len());
    let mut dense = 0usize;
    let mut string_copies = 0usize;
    let mut formula_bytes = 0usize;
    for bounds in starts.windows(2) {
        let mut sheet = &workbook[bounds[0]..bounds[1]];
        let mut first = (usize::MAX, usize::MAX);
        let mut last = (0, 0);
        let mut depth = 0usize;
        let mut seen_bof = false;
        let mut last_formula = (0, 0);
        loop {
            let (kind, body) = biff_record(&mut sheet, &mut records_left, deadline)?;
            if kind == 0x0809 {
                depth += usize::from(seen_bof);
                seen_bof = true;
                continue;
            }
            if kind == 0x000a {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                continue;
            }
            if depth != 0 {
                continue;
            }
            if kind == 0x0200 {
                validate_dimensions(body)?;
            }
            if !matches!(
                kind,
                0x0203 | 0x0204 | 0x00d6 | 0x0205 | 0x0207 | 0x027e | 0x00fd | 0x00bd | 0x0006
            ) {
                continue;
            }
            let (row, col) = if kind == 0x0207 {
                last_formula
            } else {
                if body.len() < 4 {
                    return Err("invalid XLS cell record".into());
                }
                (usize::from(word_at(body, 0)), usize::from(word_at(body, 2)))
            };
            let last_col = if kind == 0x00bd {
                if body.len() < 12 || (body.len() - 6) % 6 != 0 {
                    return Err("invalid XLS multi-cell record".into());
                }
                col + (body.len() - 6) / 6 - 1
            } else {
                col
            };
            if last_col > 255 {
                return Err("invalid XLS column".into());
            }
            first = (first.0.min(row), first.1.min(col));
            last = (last.0.max(row), last.1.max(last_col));
            if kind == 0x00fd {
                if body.len() < 10 {
                    return Err("invalid XLS shared string cell".into());
                }
                // An out-of-range index produces no cell in Calamine.
                if let Some(cost) = string_costs.get(dword(body, 6) as usize) {
                    string_copies += cost;
                    if string_copies > MAX_STRING_COPIES {
                        return Err("XLS shared string memory limit".into());
                    }
                }
            }
            if kind == 0x0006 {
                last_formula = (row, col);
                formula_bytes += body.len();
                if body.len() > 1024 || formula_bytes > 192 * 1024 {
                    return Err("XLS formula work limit".into());
                }
            }
        }
        if first.0 != usize::MAX {
            dense += (last.0 - first.0 + 1) * (last.1 - first.1 + 1);
            if dense > MAX_DENSE_CELLS {
                return Err("XLS dense range limit".into());
            }
        }
    }
    Ok(())
}

// Count the decoded allocation per SST entry before Calamine clones cell text.
// A continuation starts with an encoding flag only when it splits text.
fn shared_string_costs(
    parts: Vec<&[u8]>,
    codepage: u16,
    deadline: Instant,
) -> Result<Vec<usize>, String> {
    let invalid = "incomplete XLS shared string";
    let mut parts = parts.into_iter();
    let mut bytes = parts.next().unwrap_or_default();
    let mut costs = Vec::new();
    let mut total = 0;
    loop {
        if bytes.is_empty() {
            let Some(next) = parts.next() else { break };
            bytes = next;
        }
        check_time(deadline).map_err(|e| e.to_string())?;
        if costs.len() == MAX_DENSE_CELLS {
            return Err("XLS shared string count limit".into());
        }
        let flags = *bytes.get(2).ok_or(invalid)?;
        let mut chars_left = usize::from(word_at(bytes, 0));
        bytes = &bytes[3..];
        let mut extra = 0;
        if flags & 8 != 0 {
            let (size, rest) = bytes.split_at_checked(2).ok_or(invalid)?;
            extra = 4 * usize::from(word_at(size, 0));
            bytes = rest;
        }
        if flags & 4 != 0 {
            let (size, rest) = bytes.split_at_checked(4).ok_or(invalid)?;
            extra += dword(size, 0) as usize;
            bytes = rest;
        }
        let mut width = usize::from(flags & 1) + 1;
        let mut cost = 0;
        while chars_left != 0 {
            let chars = chars_left.min(bytes.len() / width);
            let (text, rest) = bytes.split_at(chars * width);
            cost += match (codepage, width) {
                (1200, 1) => chars + text.iter().filter(|&&byte| !byte.is_ascii()).count(),
                (1200, 2) if !text.starts_with(b"\xef\xbb\xbf") => chars * 3,
                _ => chars * 6,
            };
            bytes = rest;
            chars_left -= chars;
            if chars_left != 0 {
                let (&flags, next) = parts.next().and_then(<[u8]>::split_first).ok_or(invalid)?;
                width = usize::from(flags & 1) + 1;
                bytes = next;
            }
        }
        while extra > bytes.len() {
            extra -= bytes.len();
            bytes = parts.next().ok_or(invalid)?;
        }
        bytes = &bytes[extra..];
        total += cost;
        if total > MAX_STRING_COPIES {
            return Err("XLS decoded string table memory limit".into());
        }
        costs.push(cost);
    }
    Ok(costs)
}

fn validate_dimensions(body: &[u8]) -> Result<(), String> {
    let (first, end, col) = match body.len() {
        10 => (u32::from(word_at(body, 0)), u32::from(word_at(body, 2)), 4),
        14 | 16 => (dword(body, 0), dword(body, 4), 8),
        _ => return Err("invalid XLS dimension record".into()),
    };
    let col_first = word_at(body, col);
    let col_end = word_at(body, col + 2);
    if first > 65_535
        || end > 65_536
        || col_first > 255
        || col_end > 256
        || (end != 0 && end <= first)
        || (col_end != 0 && col_end <= col_first)
        || end.saturating_sub(first) as usize * usize::from(col_end.saturating_sub(col_first))
            > MAX_DENSE_CELLS
    {
        return Err("XLS worksheet dimension limit".into());
    }
    Ok(())
}

fn biff_record<'a>(
    remaining: &mut &'a [u8],
    records_left: &mut usize,
    deadline: Instant,
) -> Result<(u16, &'a [u8]), String> {
    *records_left = records_left.checked_sub(1).ok_or("XLS record limit")?;
    if records_left.is_multiple_of(128) {
        check_time(deadline).map_err(|e| e.to_string())?;
    }
    let header = remaining.get(..4).ok_or("incomplete XLS record header")?;
    let kind = word_at(header, 0);
    let end = 4 + usize::from(word_at(header, 2));
    let body = remaining.get(4..end).ok_or("incomplete XLS record body")?;
    *remaining = &remaining[end..];
    Ok((kind, body))
}

fn word_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn dword(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
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

pub(super) fn parse_xls(
    bytes: Vec<u8>,
    mut scan: parts::PartBudget,
    budget: &XlsxBudget,
) -> Result<OfficeContent, calamine::XlsError> {
    let reader = CheckedReader::new(Cursor::new(bytes), scan.deadline);
    let mut book = Xls::new(reader)?;
    let names = book.sheet_names();
    let XlsxBudget {
        max_sheets,
        max_rows,
        max_columns,
        ..
    } = *budget;
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
        sheets.push((
            index,
            sheet.finish(max_columns, spreadsheet::MAX_SAMPLE_CELLS / count),
        ));
    }
    Ok(spreadsheet::render_selection(
        &names,
        sheets,
        budget,
        "xls",
        Instant::now() >= scan.deadline,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn record(output: &mut Vec<u8>, kind: u16, body: &[u8]) {
        output.extend_from_slice(&kind.to_le_bytes());
        output.extend_from_slice(&(body.len() as u16).to_le_bytes());
        output.extend_from_slice(body);
    }

    #[test]
    fn xls_rejects_extreme_dimensions_and_sparse_dense_expansion() {
        let mut header = Vec::new();
        record(&mut header, 0x0085, &[14, 0, 0, 0, 0, 0]);
        record(&mut header, 0x000a, &[]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut declared = header.clone();
        let mut dimensions = [0; 14];
        dimensions[4..8].copy_from_slice(&65_536u32.to_le_bytes());
        dimensions[10..12].copy_from_slice(&256u16.to_le_bytes());
        record(&mut declared, 0x0200, &dimensions);
        record(&mut declared, 0x000a, &[]);
        assert!(validate_workbook(&declared, deadline)
            .unwrap_err()
            .contains("dimension"));

        let mut sparse = header;
        record(&mut sparse, 0x0203, &[0; 14]);
        let mut far_cell = [0; 14];
        far_cell[..4].copy_from_slice(&[0xff, 0xff, 0xff, 0x00]);
        record(&mut sparse, 0x0203, &far_cell);
        record(&mut sparse, 0x000a, &[]);
        assert!(validate_workbook(&sparse, deadline)
            .unwrap_err()
            .contains("dense range"));
    }
}
