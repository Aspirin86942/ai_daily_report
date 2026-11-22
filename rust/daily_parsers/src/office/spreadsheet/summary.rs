use super::XlsxRow;
use crate::contract::{ContentUnitKind, SourceRange};
use std::collections::HashMap;

#[cfg(test)]
mod tests;

const SUMMARY_CHARS: usize = 480;
const SCOPE: &str = "统计仅指本次读取范围。";

fn identity_label(label: &str) -> bool {
    matches!(label.trim(), "姓名" | "Name")
}

fn update_labels<'a>(labels: &mut Vec<&'a str>, row: &'a XlsxRow) {
    labels.resize(labels.len().max(row.cells.len()), "");
    for (label, cell) in labels.iter_mut().zip(&row.cells) {
        if !cell.is_empty() {
            *label = cell;
        }
    }
}

fn column_label(labels: &[&str], column: usize) -> String {
    match labels.get(column) {
        Some(&label) if !label.is_empty() => label.to_owned(),
        _ => format!("列{}", column + 1),
    }
}

fn derived_row(text: String, ranges: Vec<SourceRange>) -> XlsxRow {
    // Only totals explicitly present in the workbook receive Total priority.
    XlsxRow {
        text,
        kind: ContentUnitKind::TableRow,
        ranges,
        separator: None,
        cells: vec![],
    }
}

pub(super) fn xlsx_unidentified_numeric_rows(rows: &[XlsxRow]) -> Vec<XlsxRow> {
    let mut labels = Vec::new();
    let mut identity = None;
    let mut result = Vec::new();
    for row in rows {
        if row.kind == ContentUnitKind::TableHeader {
            update_labels(&mut labels, row);
            identity = identity.or_else(|| row.cells.iter().position(|cell| identity_label(cell)));
            continue;
        }
        let Some(identity_column) = identity else {
            continue;
        };
        if row.kind != ContentUnitKind::TableRow
            || row
                .cells
                .get(identity_column)
                .is_some_and(|value| !value.is_empty())
            || row
                .cells
                .iter()
                .filter(|value| value.parse::<f64>().is_ok())
                .nth(1)
                .is_none()
        {
            continue;
        }
        let number = row.ranges.first().map_or(0, |range| range.start);
        let prefix = format!("原表数值行（第 {number} 行，未标姓名）：");
        let mut text = prefix.clone();
        for (column, value) in row.cells.iter().enumerate() {
            if value.parse::<f64>().is_err() {
                continue;
            }
            let label = column_label(&labels, column);
            let field = format!("{label} = {value}；");
            if text.chars().count() + field.chars().count() > SUMMARY_CHARS {
                if text != prefix {
                    result.push(derived_row(text, row.ranges.clone()));
                }
                text = prefix.clone();
            }
            if text.chars().count() + field.chars().count() <= SUMMARY_CHARS {
                text.push_str(&field);
            }
        }
        if text != prefix {
            result.push(derived_row(text, row.ranges.clone()));
        }
    }
    result
}

pub(super) fn xlsx_band_rows(rows: &[XlsxRow]) -> Vec<XlsxRow> {
    if !rows.iter().any(|row| {
        row.cells.first().is_some_and(|cell| cell == "所属公司")
            && row.cells.get(1).is_some_and(|cell| cell == "送审合同类型")
    }) {
        return vec![];
    }
    let mut result = Vec::new();
    for pair in rows.windows(2) {
        let bands = &pair[0];
        let data = &pair[1];
        if !bands
            .ranges
            .first()
            .zip(data.ranges.first())
            .is_some_and(|(left, right)| left.end == right.start && left.label == right.label)
        {
            continue;
        }
        if data.cells.len() < 3 || data.cells[0].is_empty() || data.cells[1].is_empty() {
            continue;
        }
        let intervals: Vec<_> = bands
            .cells
            .iter()
            .enumerate()
            .skip(2)
            .filter(|(_, value)| value.trim_start().starts_with("金额 [") && value.contains(')'))
            .map(|(column, value)| (column, value.as_str()))
            .collect();
        if intervals.is_empty() {
            continue;
        }
        let mut text = format!(
            "所属公司 = {}；送审合同类型 = {}；金额档 = {}；仅对应原表这两行。",
            data.cells[0],
            data.cells[1],
            intervals
                .iter()
                .map(|(_, label)| *label)
                .collect::<Vec<_>>()
                .join("、")
        );
        if text.chars().count() > SUMMARY_CHARS {
            continue;
        }
        if let [(column, _)] = intervals.as_slice() {
            if let Some(flow) = data.cells.get(*column).filter(|s| !s.is_empty()) {
                let detail = format!("审批流 = {flow}；");
                if text.chars().count() + detail.chars().count() <= SUMMARY_CHARS {
                    text.push_str(&detail);
                }
            }
        }
        result.push(derived_row(
            text,
            bands.ranges.iter().chain(&data.ranges).cloned().collect(),
        ));
    }
    result
}

pub(super) fn xlsx_window_summaries(rows: &[XlsxRow]) -> Vec<XlsxRow> {
    let header_index = rows
        .iter()
        .rposition(|row| row.kind == ContentUnitKind::TableHeader);
    let Some(header_index) = header_index else {
        return vec![];
    };
    let mut header = Vec::new();
    for row in &rows[..=header_index] {
        if row.kind == ContentUnitKind::TableHeader {
            update_labels(&mut header, row);
        }
    }
    let data: Vec<_> = rows[header_index + 1..]
        .iter()
        .filter(|row| {
            row.kind == ContentUnitKind::TableRow
                && row.cells.iter().filter(|cell| !cell.is_empty()).count() >= 2
        })
        .collect();
    if data.len() < 2 {
        return vec![];
    }
    let mut summaries = Vec::new();
    // A repeated identity defines a bounded group. Its common amounts and dates
    // remain attributable to that identity instead of being mixed across people.
    if let Some(column) = header.iter().position(|label| identity_label(label)) {
        let groups = groups_in_source_order(&data, column);
        if groups.len() > 1 {
            for (_, members) in groups {
                if members.len() >= 2 {
                    summaries.extend(xlsx_amount_series(&header, &members, column));
                    summaries.push(xlsx_rows_summary(&header, &members));
                }
            }
        } else {
            summaries.extend(xlsx_amount_series(&header, &data, column));
        }
    }
    summaries.push(xlsx_rows_summary(&header, &data));
    summaries
}

fn groups_in_source_order<'a>(
    data: &[&'a XlsxRow],
    column: usize,
) -> Vec<(&'a str, Vec<&'a XlsxRow>)> {
    let mut groups: Vec<(&str, Vec<&XlsxRow>)> = Vec::new();
    let mut positions = HashMap::new();
    for &row in data {
        let Some(value) = row.cells.get(column).filter(|value| !value.is_empty()) else {
            continue;
        };
        let i = *positions.entry(value.as_str()).or_insert_with(|| {
            groups.push((value, Vec::new()));
            groups.len() - 1
        });
        groups[i].1.push(row);
    }
    groups
}

fn xlsx_amount_series(header: &[&str], data: &[&XlsxRow], identity_column: usize) -> Vec<XlsxRow> {
    let period_column = header
        .iter()
        .position(|label| matches!(label.trim(), "月份" | "交易日期" | "Date" | "Month"));
    let Some(period_column) = period_column else {
        return vec![];
    };
    let Some(identity) = data[0]
        .cells
        .get(identity_column)
        .filter(|id| !id.is_empty())
    else {
        return vec![];
    };
    if data
        .iter()
        .any(|row| row.cells.get(identity_column) != Some(identity))
    {
        return vec![];
    }
    let mut result = Vec::new();
    for label in [
        "应付工资",
        "实发工资",
        "代扣代缴合计",
        "基本工资",
        "交易金额",
        "Amount",
        "Total",
    ] {
        let Some(column) = header.iter().position(|value| value.trim() == label) else {
            continue;
        };
        let values = groups_in_source_order(data, column);
        if values.len() < 2 {
            continue;
        }
        let prefix = format!("读取窗口统计：{} = {identity}；", header[identity_column]);
        let suffix = SCOPE;
        let mut text = prefix.clone();
        let mut used: Vec<&XlsxRow> = Vec::new();
        for (value, members) in values {
            let periods = members
                .iter()
                .filter_map(|row| row.cells.get(period_column))
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("、");
            let field = format!(
                "{label} = {value}（对应{} = {periods}）；",
                header[period_column]
            );
            if text.chars().count() + field.chars().count() + suffix.chars().count() > SUMMARY_CHARS
                && !used.is_empty()
            {
                used.sort_by_key(|row| row.ranges.first().map(|range| range.start));
                result.push(summary_row(text + suffix, &used));
                text = prefix.clone();
                used.clear();
            }
            if text.chars().count() + field.chars().count() + suffix.chars().count()
                <= SUMMARY_CHARS
            {
                text.push_str(&field);
                used.extend(members);
            }
        }
        if !used.is_empty() {
            used.sort_by_key(|row| row.ranges.first().map(|range| range.start));
            result.push(summary_row(text + suffix, &used));
        }
    }
    result
}

fn xlsx_rows_summary(header: &[&str], data: &[&XlsxRow]) -> XlsxRow {
    let mut text = format!("读取窗口统计：本表头对应的已读数据行 {} 条。", data.len());
    let mut common = Vec::new();
    let mut differing = Vec::new();
    for (column, value) in data[0].cells.iter().enumerate() {
        if data.iter().all(|row| row.cells.get(column) == Some(value)) {
            if value.is_empty() {
                continue;
            }
            let label = column_label(header, column);
            common.push(format!("{label} = {value}；"));
        } else {
            differing.push(column);
        }
    }
    if differing.len() == 1 {
        let column = differing[0];
        let values = data
            .iter()
            .filter_map(|row| row.cells.get(column))
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("、");
        let label = column_label(header, column);
        let field = format!("逐行 {label} = {values}；");
        if text.chars().count() + field.chars().count() <= SUMMARY_CHARS {
            text.push_str(&field);
        }
    }
    if !common.is_empty() {
        text.push_str("这些行的共同值：");
        for field in common {
            if text.chars().count() + field.chars().count() <= SUMMARY_CHARS {
                text.push_str(&field);
            }
        }
    }
    text.push_str(SCOPE);
    summary_row(text, data)
}

fn summary_row(text: String, data: &[&XlsxRow]) -> XlsxRow {
    let mut ranges: Vec<SourceRange> = Vec::new();
    for row in data {
        for range in &row.ranges {
            if let Some(prior) = ranges.last_mut().filter(|prior| {
                prior.end == range.start
                    && prior.column_start == range.column_start
                    && prior.column_end == range.column_end
                    && prior.label == range.label
            }) {
                prior.end = range.end;
            } else {
                ranges.push(range.clone());
            }
        }
    }
    derived_row(text, ranges)
}
