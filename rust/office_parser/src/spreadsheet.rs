use super::*;
use crate::parts::PartBudget;
use std::cmp::Reverse;

const DIRECTORY_LIMIT: usize = 1024;
const PROBE_LIMIT: usize = 32;
const PART_LIMIT: usize = 8 * 1024 * 1024;

pub(super) fn column_name(mut column: usize) -> String {
    let mut letters = Vec::new();
    while column > 0 {
        column -= 1;
        letters.push(char::from(b'A' + (column % 26) as u8));
        column /= 26;
    }
    letters.into_iter().rev().collect()
}

pub(super) fn cell_ranges(
    name: &str,
    row: u64,
    columns: &[usize],
    cells: &[String],
) -> Vec<SourceRange> {
    let mut result: Vec<SourceRange> = Vec::new();
    for (&column, cell) in columns.iter().zip(cells) {
        if cell.is_empty() {
            continue;
        }
        let start = column as u64;
        if let Some(last) = result.last_mut().filter(|r| r.column_end == Some(start)) {
            last.column_end = Some(start + 1);
        } else {
            let mut range = SourceRange::new(
                SourceRangeUnit::SheetRow,
                row,
                row + 1,
                Some(name.to_owned()),
            );
            range.column_start = Some(start);
            range.column_end = Some(start + 1);
            result.push(range);
        }
    }
    result
}

pub(super) fn positioned_values(
    sheet: &PreviewSheet,
    number: u64,
    cells: &[String],
    kind: ContentUnitKind,
    header: Option<&XlsxRow>,
    unit: Option<&XlsxRow>,
    unit_columns: &[Option<usize>],
) -> Vec<XlsxRow> {
    let mut rows = Vec::new();
    let key = cells.first().filter(|v| {
        total(v)
            || exception(v)
            || (sheet.columns[0] == 1 && !v.is_empty() && v.parse::<f64>().is_err())
    });
    for (index, value) in cells
        .iter()
        .enumerate()
        .filter(|(i, v)| (*i != 0 || key.is_none()) && !v.is_empty())
    {
        let col = sheet.columns[index];
        let letter = column_name(col);
        let mut ranges = cell_ranges(&sheet.name, number, &[col], std::slice::from_ref(value));
        let mut field = String::new();
        if let Some(h) = header {
            if let Some(name) = h.cells.get(index).filter(|s| !s.is_empty()) {
                field = format!(" {name}");
                let row = h.ranges[0].start;
                ranges.extend(cell_ranges(
                    &sheet.name,
                    row,
                    &[col],
                    std::slice::from_ref(name),
                ));
            }
        }
        let mut text = format!("{letter}{number}{field} = {value}");
        if let Some(key) = key {
            text.push_str(&format!(
                "（{}{number} {key}）",
                column_name(sheet.columns[0])
            ));
            ranges.extend(cell_ranges(
                &sheet.name,
                number,
                &sheet.columns[..1],
                std::slice::from_ref(key),
            ));
        }
        if let (Some(units), Some(&Some(column))) = (unit, unit_columns.get(index)) {
            let label = &units.cells[column];
            text.push_str(&format!("（{label}）"));
            ranges.extend(cell_ranges(
                &sheet.name,
                units.ranges[0].start,
                &sheet.columns[column..=column],
                std::slice::from_ref(label),
            ));
        }
        rows.push(XlsxRow {
            text,
            kind,
            ranges,
            separator: None,
            cells: vec![value.clone()],
        });
    }
    rows
}

pub(super) fn exception(value: &str) -> bool {
    if !value
        .bytes()
        .any(|c| c.is_ascii_alphabetic() || !c.is_ascii())
    {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    [
        "异常",
        "差异",
        "逾期",
        "超期",
        "超支",
        "未通过",
        "待核",
        "不符",
        "风险",
        "#REF!",
        "#DIV/0!",
        "#N/A",
        "#VALUE!",
    ]
    .iter()
    .any(|word| value.contains(word))
        || ["exception", "overdue", "variance", "failed"]
            .iter()
            .any(|word| lower.contains(word))
}

fn unit_scope(value: &str) -> Option<&str> {
    let value = value.trim();
    if let Some(note) = value.strip_prefix("Unit:") {
        return (!note.trim().is_empty()).then_some("");
    }
    let (scope, tail) = value.split_once("单位")?;
    let note = tail
        .strip_prefix(['：', ':'])
        .or_else(|| tail.strip_prefix(' '))?;
    (!note.trim().is_empty()).then_some(scope.trim())
}

pub(super) fn unit(value: &str) -> bool {
    unit_scope(value).is_some()
}

// A sheet-wide note can qualify a matching field. A column with its own unit is already explicit.
pub(super) fn applicable_unit(row: &XlsxRow, index: usize, header: &str) -> Option<usize> {
    let header = header.trim();
    if header.is_empty() || header.contains(['(', '（', '[', '【', '/', '%', '％']) {
        return None;
    }
    let aligned_units = row.cells.iter().filter(|s| unit(s)).nth(1).is_some();
    let matches = |column: usize, text: &str| {
        unit_scope(text).is_some_and(|scope| {
            if scope.is_empty() {
                return aligned_units && column == index;
            }
            header == scope
                || header.ends_with(scope)
                || (scope == "金额"
                    && ["预算额", "实付额", "实收额", "单价", "总价", "价款", "余额"]
                        .iter()
                        .any(|suffix| header.ends_with(suffix)))
        })
    };
    if row.cells.get(index).is_some_and(|v| matches(index, v)) {
        return Some(index);
    }
    let mut notes = row.cells.iter().enumerate().filter(|(i, v)| matches(*i, v));
    let note = notes.next()?;
    notes.next().is_none().then_some(note.0)
}

pub(super) fn total(value: &str) -> bool {
    ["合计", "小计", "总计", "总额"]
        .iter()
        .any(|word| value.contains(word))
        || value.trim().eq_ignore_ascii_case("total")
}

fn field_priority(value: &str) -> u8 {
    if exception(value) {
        4
    } else if total(value)
        || ["实付", "实收", "实际", "差额"]
            .iter()
            .any(|word| value.contains(word))
    {
        3
    } else if [
        "单位",
        "金额",
        "余额",
        "状态",
        "结果",
        "数量",
        "进度",
        "负责人",
        "项目",
        "姓名",
    ]
    .iter()
    .any(|word| value.contains(word))
        || ["amount", "status", "unit", "name"]
            .iter()
            .any(|word| value.eq_ignore_ascii_case(word))
    {
        2
    } else {
        0
    }
}

struct SampleRow {
    number: u64,
    cells: Vec<(usize, String)>,
    formula: bool,
    total: bool,
}

// Keep the first structural rows, then source-marked totals/exceptions, then ordinary rows.
// Ties between ordinary rows preserve the original prefix and stable window statistics.
pub(super) struct SheetSample {
    name: String,
    limit: usize,
    rows: BTreeMap<(u8, u64), SampleRow>,
    seen: usize,
    priority: u8,
    pub truncated: bool,
}

impl SheetSample {
    pub fn new(name: &str, limit: usize) -> Self {
        Self {
            name: name.chars().take(512).collect(),
            limit: limit.min(512),
            rows: BTreeMap::new(),
            seen: 0,
            priority: 0,
            truncated: false,
        }
    }

    pub fn observe(&mut self, number: u64, cells: Vec<(usize, String)>, formula: bool, sum: bool) {
        if cells.is_empty() {
            return;
        }
        let unit = cells.iter().any(|(_, v)| unit(v));
        let total = sum || cells.iter().any(|(_, v)| total(v));
        let special = cells.iter().any(|(_, v)| exception(v));
        let header = self.seen < 8 && is_header_row_values(cells.iter().map(|(_, v)| v.as_str()));
        let priority = if self.seen == 0 || header || unit {
            5
        } else if total {
            4
        } else if special {
            3
        } else {
            1
        };
        self.seen += 1;
        self.priority = self.priority.max(if special || total {
            4
        } else if header {
            2
        } else {
            1
        });
        let order = if priority == 4 || priority == 3 {
            number
        } else {
            u64::MAX - number
        };
        let key = (priority, order);
        if self.rows.len() == self.limit {
            self.truncated = true;
            if self
                .rows
                .first_key_value()
                .is_some_and(|(first, _)| *first >= key)
            {
                return;
            }
            self.rows.pop_first();
        }
        self.rows.insert(
            key,
            SampleRow {
                number,
                cells,
                formula,
                total: sum,
            },
        );
    }

    pub fn finish(self, max_columns: usize) -> PreviewSheet {
        let mut rows: Vec<_> = self.rows.into_values().collect();
        rows.sort_by_key(|row| row.number);
        let mut scores: BTreeMap<usize, u8> = BTreeMap::new();
        for row in &rows {
            for (col, value) in &row.cells {
                let score = field_priority(value);
                scores
                    .entry(*col)
                    .and_modify(|s| *s = (*s).max(score))
                    .or_insert(score);
            }
        }
        let first = scores.keys().next().copied();
        let mut columns: Vec<_> = scores.keys().copied().collect();
        columns.sort_by_key(|c| (Some(*c) != first, Reverse(scores[c]), *c));
        let truncated = self.truncated || columns.len() > max_columns.min(128);
        columns.truncate(max_columns.min(128));
        columns.sort_unstable();
        let mut sheet = PreviewSheet {
            priority: if rows.is_empty() {
                0
            } else {
                self.priority + field_priority(&self.name)
            },
            name: self.name,
            truncated,
            columns,
            rows: Vec::new(),
            row_numbers: Vec::new(),
            total_rows: BTreeSet::new(),
            formula_rows: BTreeSet::new(),
        };
        for row in rows {
            let mut values = vec![String::new(); sheet.columns.len()];
            for (col, value) in row.cells {
                if let Ok(index) = sheet.columns.binary_search(&col) {
                    values[index] = value;
                }
            }
            if values.iter().all(String::is_empty) {
                sheet.truncated = true;
                continue;
            }
            if row.total {
                sheet.total_rows.insert(row.number);
            }
            if row.formula {
                sheet.formula_rows.insert(row.number);
            }
            sheet.rows.push(values);
            sheet.row_numbers.push(row.number);
        }
        sheet
    }
}

pub(super) fn probe_order(names: &[String], max_sheets: usize) -> Vec<usize> {
    let n = names.len();
    let limit = max_sheets.clamp(PROBE_LIMIT, DIRECTORY_LIMIT);
    let mut indexes: Vec<_> = (0..n).collect();
    if n > limit {
        // Reserve half the survey for evenly spaced sheets even when names are uninformative.
        let spread: BTreeSet<_> = (0..limit / 2)
            .map(|i| i * (n - 1) / (limit / 2 - 1))
            .collect();
        indexes.sort_by_key(|i| (!spread.contains(i), Reverse(field_priority(&names[*i])), *i));
        indexes.truncate(limit);
    }
    indexes.sort_by_key(|i| {
        (
            Reverse(field_priority(&names[*i])),
            *i != 0,
            *i != n - 1,
            *i,
        )
    });
    indexes
}

pub(super) fn render_selection(
    names: &[String],
    mut sheets: Vec<(usize, PreviewSheet)>,
    budget: &XlsxBudget,
    format: &str,
    mut truncated: bool,
) -> (String, bool, ParsedContentAudit) {
    truncated |= sheets.len() < names.len() || sheets.len() > budget.max_sheets;
    // For equal evidence keep the first sheet, then the last, then workbook order.
    sheets.sort_by_key(|(i, s)| {
        (
            Reverse(s.priority),
            *i != 0,
            budget.max_sheets > 1 && *i + 1 != names.len(),
            *i,
        )
    });
    sheets.truncate(budget.max_sheets);
    sheets.sort_by_key(|(i, _)| *i);
    let sheets: Vec<_> = sheets.into_iter().map(|(_, s)| s).collect();
    truncated |= sheets.iter().any(|s| s.truncated);
    let mut directory = String::new();
    if names.len() > 1 {
        directory.push_str("工作表目录");
        let cap = (budget.max_chars / 8).min(2048);
        for (index, name) in names.iter().enumerate() {
            let entry = format!("  [{}] {}", index + 1, escape_markdown_cell(name));
            if directory.chars().count() + entry.chars().count() > cap {
                directory.push('…');
                break;
            }
            directory.push_str(&entry);
        }
    }
    let (text, limited, audit) = render_workbook(&sheets, budget, format, &directory);
    (text, limited || truncated, audit)
}

pub(super) fn daily_xlsx(
    mut parts: PartBudget,
    zip: &mut ZipArchive<File>,
    budget: &XlsxBudget,
) -> Result<(String, bool, ParsedContentAudit), String> {
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
        let Some(path) = resolve_sheet_path(sheet, &relationships) else {
            truncated = true;
            continue;
        };
        let allowance = PART_LIMIT.min(parts.remaining / (count - probed));
        let (xml, partial) = parts.read(zip, &path, allowance)?;
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
