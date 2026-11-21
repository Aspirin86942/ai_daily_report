use super::summary::{xlsx_band_rows, xlsx_unidentified_numeric_rows, xlsx_window_summaries};
use super::{
    applicable_unit, cell_ranges, column_name, exception, is_header_row, positioned_values, total,
    unit, PreviewSheet, XlsxBudget, XlsxRow,
};
use crate::{escape_markdown_cell, OfficeContent};
use ai_daily_worker_contract::{AuditTextBuilder, ContentUnitKind};
use std::collections::BTreeSet;
use std::time::Instant;

struct SheetSelection {
    costs: Vec<usize>,
    // (row index, cumulative cost) for headers and unit notes, in source order.
    structure: Vec<(usize, usize)>,
    covered: usize,
    heading_cost: usize,
    selected: BTreeSet<usize>,
}

impl SheetSelection {
    fn new(heading: &str, rows: &[XlsxRow]) -> Self {
        let costs: Vec<_> = rows.iter().map(XlsxRow::chars).collect();
        let mut structure = Vec::new();
        let mut sum = 0;
        for (i, row) in rows.iter().enumerate() {
            if matches!(
                row.kind,
                ContentUnitKind::TableHeader | ContentUnitKind::Unit | ContentUnitKind::Heading
            ) {
                sum += costs[i];
                structure.push((i, sum));
            }
        }
        Self {
            costs,
            structure,
            covered: 0,
            heading_cost: heading.chars().count() + 1,
            selected: BTreeSet::new(),
        }
    }

    fn admit(&mut self, row: usize, remaining: &mut usize) {
        let n = self.structure.partition_point(|(index, _)| *index < row);
        let prefix_cost = |n: usize| n.checked_sub(1).map_or(0, |i| self.structure[i].1);
        let cost = self.costs[row]
            + prefix_cost(n).saturating_sub(prefix_cost(self.covered))
            + if self.selected.is_empty() {
                self.heading_cost
            } else {
                0
            };
        if cost <= *remaining {
            if n > self.covered {
                self.selected.extend(
                    self.structure[self.covered..n]
                        .iter()
                        .map(|(index, _)| *index),
                );
                self.covered = n;
            }
            self.selected.insert(row);
            *remaining -= cost;
        }
    }
}

pub(super) fn render_workbook(
    sheets: &[PreviewSheet],
    budget: &XlsxBudget,
    format: &str,
    directory: &str,
) -> OfficeContent {
    let mut builder = AuditTextBuilder::new(budget.max_chars as u64, format);
    let mut title = format!("# {} preview", format.to_ascii_uppercase());
    if !directory.is_empty() {
        title.push('\n');
        title.push_str(directory);
    }
    builder.push(&title, ContentUnitKind::Heading, vec![]);
    let wall_remaining = budget.deadline.saturating_duration_since(Instant::now());
    let select_until =
        budget.deadline - (wall_remaining / 4).min(std::time::Duration::from_millis(200));
    let prepare_until =
        select_until - (wall_remaining / 8).min(std::time::Duration::from_millis(100));
    let mut prepared = Vec::new();
    let mut selection_truncated = false;
    for sheet in sheets {
        if Instant::now() >= prepare_until {
            selection_truncated = true;
            break;
        }
        if sheet.rows.is_empty() {
            continue;
        }
        let heading = format!("## Sheet: {}", escape_markdown_cell(&sheet.name));
        let col_count = sheet.columns.len();
        let positioned = sheet.columns.last().is_some_and(|col| *col > 12)
            || !sheet.columns.iter().copied().eq(1..=col_count);
        let header_index = sheet.rows.iter().position(|row| is_header_row(row));
        let labels = &sheet.rows[header_index.unwrap_or(0)];
        let identity_columns: Vec<_> = labels
            .iter()
            .enumerate()
            .filter_map(|(index, label)| {
                matches!(label.trim(), "姓名" | "月份" | "Name" | "Date").then_some(index)
            })
            .collect();
        let mut rendered: Vec<XlsxRow> = Vec::new();
        let mut header_at: Option<usize> = None;
        let mut unit_at: Option<usize> = None;
        let mut data_since_unit = false;
        let mut unit_columns = Vec::new();
        for (index, row) in sheet.rows.iter().enumerate() {
            if Instant::now() >= prepare_until {
                selection_truncated = true;
                break;
            }
            let cells: Vec<_> = row.iter().map(|c| escape_markdown_cell(c)).collect();
            let number = sheet.row_numbers[index];
            let ranges = cell_ranges(&sheet.name, number, &sheet.columns, &cells);
            for range in &ranges {
                builder.read(range.clone());
            }
            let metadata = header_index.is_some_and(|header| index < header);
            let line = if positioned {
                cells
                    .iter()
                    .zip(&sheet.columns)
                    .filter(|(value, _)| !value.is_empty())
                    .map(|(value, col)| format!("{}{number}={value}", column_name(*col)))
                    .collect::<Vec<_>>()
                    .join(" | ")
            } else if metadata {
                cells
                    .iter()
                    .filter(|cell| !cell.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | ")
            } else {
                format!("| {} |", cells.join(" | "))
            };
            let unit_line = cells.iter().any(|c| unit(c));
            let formula_padding = sheet.formula_rows.contains(&sheet.row_numbers[index])
                && !identity_columns.is_empty()
                && identity_columns.iter().all(|column| {
                    cells
                        .get(*column)
                        .is_none_or(|value| value.is_empty() || value == "0")
                })
                && cells
                    .iter()
                    .filter(|value| !value.is_empty())
                    .all(|value| value.parse::<f64>().is_ok_and(|number| number == 0.0));
            let kind = if formula_padding {
                ContentUnitKind::Placeholder
            } else if unit_line {
                ContentUnitKind::Unit
            } else if is_header_row(&cells)
                || header_index.is_some_and(|header| index < header)
                || (sheet.rows.len() > 1
                    && index
                        == usize::from(
                            sheet
                                .rows
                                .first()
                                .is_some_and(|r| r.iter().any(|c| unit(c))),
                        ))
            {
                ContentUnitKind::TableHeader
            } else if cells.iter().any(|c| total(c))
                || sheet.total_rows.contains(&sheet.row_numbers[index])
            {
                ContentUnitKind::Total
            } else {
                ContentUnitKind::TableRow
            };
            if positioned {
                match kind {
                    ContentUnitKind::TableHeader => {
                        if data_since_unit {
                            unit_at = None;
                        }
                        header_at = Some(rendered.len());
                    }
                    ContentUnitKind::Unit => {
                        unit_at = Some(rendered.len());
                        data_since_unit = false;
                    }
                    ContentUnitKind::TableRow | ContentUnitKind::Total => data_since_unit = true,
                    _ => {}
                }
            }
            if positioned
                && (kind == ContentUnitKind::Total
                    || (kind == ContentUnitKind::TableRow && cells.iter().any(|s| exception(s))))
            {
                let values = positioned_values(
                    sheet,
                    number,
                    &cells,
                    kind,
                    header_at.map(|i| &rendered[i]),
                    unit_at.map(|i| &rendered[i]),
                    &unit_columns,
                );
                if !values.is_empty() {
                    rendered.extend(values);
                    continue;
                }
            }
            rendered.push(XlsxRow {
                text: line,
                kind,
                ranges,
                cells,
                separator: (kind == ContentUnitKind::TableHeader && !metadata && !positioned)
                    .then(|| format!("| {} |", vec!["---"; col_count].join(" | "))),
            });
            if positioned && matches!(kind, ContentUnitKind::TableHeader | ContentUnitKind::Unit) {
                unit_columns.clear();
                if let (Some(h), Some(u)) = (header_at, unit_at) {
                    unit_columns.extend(
                        rendered[h]
                            .cells
                            .iter()
                            .enumerate()
                            .map(|(i, name)| applicable_unit(&rendered[u], i, name)),
                    );
                }
            }
        }
        if positioned {
            prepared.push((heading, rendered));
            continue;
        }
        let mut summaries = xlsx_window_summaries(&rendered);
        let whole = summaries.pop();
        let mut projected = xlsx_unidentified_numeric_rows(&rendered);
        projected.extend(xlsx_band_rows(&rendered));
        projected.append(&mut summaries);
        let insert_at = rendered
            .iter()
            .rposition(|row| row.kind == ContentUnitKind::TableHeader)
            .map_or(0, |index| index + 1);
        rendered.splice(insert_at..insert_at, projected);
        if let Some(whole) = whole {
            rendered.push(whole);
        }
        rendered.retain(|row| row.ranges.len() <= 128);
        prepared.push((heading, rendered));
    }
    // All rows are already read within the physical limits. Share the rendering
    // budget across sheets before giving any sheet its remaining bulk rows.
    let mut remaining = budget.max_chars.saturating_sub(title.chars().count());
    let mut choices: Vec<_> = prepared
        .iter()
        .map(|(h, rows)| SheetSelection::new(h, rows))
        .collect();
    let raw_cost: usize = prepared
        .iter()
        .zip(&choices)
        .map(|((_, rows), choice)| {
            choice.heading_cost
                + rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| !r.cells.is_empty() && r.kind != ContentUnitKind::Placeholder)
                    .map(|(i, _)| choice.costs[i])
                    .sum::<usize>()
        })
        .sum();
    let all_raw_fit = raw_cost <= remaining;
    for ((_, rows), choice) in prepared.iter().zip(&mut choices) {
        if all_raw_fit {
            choice.selected.extend((0..rows.len()).filter(|&i| {
                !rows[i].cells.is_empty() && rows[i].kind != ContentUnitKind::Placeholder
            }));
            choice.covered = choice.structure.len();
        } else if let Some(first) = rows.iter().position(|r| r.useful() && !r.cells.is_empty()) {
            choice.admit(first, &mut remaining);
        }
    }
    if all_raw_fit {
        remaining -= raw_cost;
    }
    let mut orders = Vec::new();
    let mut cheapest = usize::MAX;
    for ((_, rows), choice) in prepared.iter().zip(&choices) {
        let mut order = Vec::new();
        if remaining > 0 && Instant::now() < select_until {
            for (i, row) in rows.iter().enumerate() {
                if row.useful() && !choice.selected.contains(&i) && choice.costs[i] <= remaining {
                    cheapest = cheapest.min(choice.costs[i]);
                    order.push(i);
                }
            }
            order.sort_by_cached_key(|&i| {
                (
                    rows[i].kind != ContentUnitKind::Total,
                    !rows[i].cells.iter().any(|s| exception(s)),
                )
            });
        }
        orders.push(order.into_iter());
    }
    while remaining >= cheapest && Instant::now() < select_until {
        let mut progress = false;
        for (choice, order) in choices.iter_mut().zip(&mut orders) {
            if let Some(row) = order.next() {
                choice.admit(row, &mut remaining);
                progress = true;
                if remaining < cheapest {
                    break;
                }
            }
        }
        if !progress {
            break;
        }
    }
    for ((heading, rows), choice) in prepared.iter().zip(choices) {
        selection_truncated |= rows
            .iter()
            .enumerate()
            .any(|(i, row)| !row.cells.is_empty() && !choice.selected.contains(&i));
        if choice.selected.is_empty() {
            continue;
        }
        builder.push(heading, ContentUnitKind::Heading, vec![]);
        for row in &choice.selected {
            if Instant::now() >= budget.deadline {
                selection_truncated = true;
                break;
            }
            let row = &rows[*row];
            builder.push(&row.text, row.kind, row.ranges.clone());
            if let Some(separator) = &row.separator {
                builder.push(separator, ContentUnitKind::TableHeader, row.ranges.clone());
            }
        }
    }
    let (content, audit, truncated) = builder.finish();
    (content, audit, truncated || selection_truncated)
}
