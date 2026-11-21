use super::XlsxRow;
use ai_daily_worker_contract::{ContentUnitKind, SourceRange};

#[cfg(test)]
mod tests;

pub(super) fn xlsx_unidentified_numeric_rows(rows: &[XlsxRow]) -> Vec<XlsxRow> {
    let Some((header_index, identity_column)) = rows.iter().enumerate().find_map(|(index, row)| {
        (row.kind == ContentUnitKind::TableHeader)
            .then(|| {
                row.cells
                    .iter()
                    .position(|cell| matches!(cell.trim(), "姓名" | "Name"))
            })
            .flatten()
            .map(|column| (index, column))
    }) else {
        return vec![];
    };
    let mut result = Vec::new();
    for (offset, row) in rows[header_index + 1..].iter().enumerate() {
        if row.kind != ContentUnitKind::TableRow
            || row
                .cells
                .get(identity_column)
                .is_some_and(|value| !value.is_empty())
            || row
                .cells
                .iter()
                .filter(|value| value.parse::<f64>().is_ok())
                .count()
                < 2
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
            let label = rows[..header_index + 1 + offset]
                .iter()
                .rev()
                .filter(|candidate| candidate.kind == ContentUnitKind::TableHeader)
                .find_map(|candidate| {
                    candidate
                        .cells
                        .get(column)
                        .filter(|label| !label.is_empty())
                })
                .cloned()
                .unwrap_or_else(|| format!("列{}", column + 1));
            let field = format!("{label} = {value}；");
            if text.chars().count() + field.chars().count() > 480 {
                if text != prefix {
                    result.push(XlsxRow {
                        text,
                        kind: ContentUnitKind::TableRow,
                        ranges: row.ranges.clone(),
                        separator: None,
                        cells: vec![],
                    });
                }
                text = prefix.clone();
            }
            if text.chars().count() + field.chars().count() <= 480 {
                text.push_str(&field);
            }
        }
        if text != prefix {
            result.push(XlsxRow {
                text,
                kind: ContentUnitKind::TableRow,
                ranges: row.ranges.clone(),
                separator: None,
                cells: vec![],
            });
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
            .skip(2)
            .filter(|value| value.trim_start().starts_with("金额 [") && value.contains(')'))
            .cloned()
            .collect();
        if intervals.is_empty() {
            continue;
        }
        let mut text = format!(
            "所属公司 = {}；送审合同类型 = {}；金额档 = {}；仅对应原表这两行。",
            data.cells[0],
            data.cells[1],
            intervals.join("、")
        );
        if text.chars().count() > 480 {
            continue;
        }
        if intervals.len() == 1 {
            let flow = bands
                .cells
                .iter()
                .enumerate()
                .skip(2)
                .find_map(|(column, value)| {
                    (value == &intervals[0])
                        .then(|| data.cells.get(column))
                        .flatten()
                        .filter(|value| !value.is_empty())
                });
            if let Some(flow) = flow {
                let detail = format!("审批流 = {flow}；");
                if text.chars().count() + detail.chars().count() <= 480 {
                    text.push_str(&detail);
                }
            }
        }
        result.push(XlsxRow {
            text,
            kind: ContentUnitKind::TableRow,
            ranges: bands.ranges.iter().chain(&data.ranges).cloned().collect(),
            separator: None,
            cells: vec![],
        });
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
    let last_header = &rows[header_index];
    let columns = rows[..=header_index]
        .iter()
        .filter(|row| row.kind == ContentUnitKind::TableHeader)
        .map(|row| row.cells.len())
        .max()
        .unwrap_or(0);
    let header = XlsxRow {
        text: last_header.text.clone(),
        kind: ContentUnitKind::TableHeader,
        ranges: last_header.ranges.clone(),
        separator: None,
        cells: (0..columns)
            .map(|column| {
                rows[..=header_index]
                    .iter()
                    .rev()
                    .filter(|row| row.kind == ContentUnitKind::TableHeader)
                    .find_map(|row| row.cells.get(column).filter(|cell| !cell.is_empty()))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect(),
    };
    let header = &header;
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
    if let Some(column) = header
        .cells
        .iter()
        .position(|label| matches!(label.trim(), "姓名" | "Name"))
    {
        let mut groups: Vec<(&str, Vec<&XlsxRow>)> = Vec::new();
        for row in &data {
            let Some(identity) = row.cells.get(column).filter(|value| !value.is_empty()) else {
                continue;
            };
            if let Some((_, members)) = groups.iter_mut().find(|(key, _)| *key == identity) {
                members.push(row);
            } else {
                groups.push((identity, vec![row]));
            }
        }
        if groups.len() > 1 {
            for (_, members) in groups {
                if members.len() >= 2 {
                    summaries.extend(xlsx_amount_series(header, &members, column));
                    summaries.push(xlsx_rows_summary(header, &members));
                }
            }
        } else {
            summaries.extend(xlsx_amount_series(header, &data, column));
        }
    }
    summaries.push(xlsx_rows_summary(header, &data));
    summaries
}

fn xlsx_amount_series(header: &XlsxRow, data: &[&XlsxRow], identity_column: usize) -> Vec<XlsxRow> {
    let period_column = header
        .cells
        .iter()
        .position(|label| matches!(label.trim(), "月份" | "交易日期" | "Date" | "Month"));
    let Some(period_column) = period_column else {
        return vec![];
    };
    let identity = &data[0].cells[identity_column];
    if identity.is_empty()
        || data
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
        let Some(column) = header.cells.iter().position(|value| value.trim() == label) else {
            continue;
        };
        let mut values: Vec<(&str, Vec<&XlsxRow>)> = Vec::new();
        for row in data {
            let Some(value) = row.cells.get(column).filter(|value| !value.is_empty()) else {
                continue;
            };
            if let Some((_, members)) = values.iter_mut().find(|(prior, _)| *prior == value) {
                members.push(row);
            } else {
                values.push((value, vec![row]));
            }
        }
        if values.len() < 2 {
            continue;
        }
        let prefix = format!(
            "读取窗口统计：{} = {identity}；",
            header.cells[identity_column]
        );
        let suffix = "统计仅指本次读取范围。";
        let mut text = prefix.clone();
        let mut used: Vec<&XlsxRow> = Vec::new();
        for (value, members) in values {
            let periods = members
                .iter()
                .filter_map(|row| row.cells.get(period_column))
                .cloned()
                .collect::<Vec<_>>()
                .join("、");
            let field = format!(
                "{label} = {value}（对应{} = {periods}）；",
                header.cells[period_column]
            );
            if text.chars().count() + field.chars().count() + suffix.chars().count() > 480
                && !used.is_empty()
            {
                used.sort_by_key(|row| row.ranges.first().map(|range| range.start));
                result.push(summary_row(text + suffix, &used));
                text = prefix.clone();
                used.clear();
            }
            if text.chars().count() + field.chars().count() + suffix.chars().count() <= 480 {
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

fn xlsx_rows_summary(header: &XlsxRow, data: &[&XlsxRow]) -> XlsxRow {
    let mut text = format!("读取窗口统计：本表头对应的已读数据行 {} 条。", data.len());
    let mut common = Vec::new();
    let mut differing = Vec::new();
    for (column, value) in data[0].cells.iter().enumerate() {
        if data.iter().all(|row| row.cells.get(column) == Some(value)) {
            if value.is_empty() {
                continue;
            }
            let label = header
                .cells
                .get(column)
                .filter(|label| !label.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("列{}", column + 1));
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
            .cloned()
            .collect::<Vec<_>>()
            .join("、");
        let label = header
            .cells
            .get(column)
            .filter(|label| !label.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("列{}", column + 1));
        let field = format!("逐行 {label} = {values}；");
        if text.chars().count() + field.chars().count() <= 480 {
            text.push_str(&field);
        }
    }
    if !common.is_empty() {
        text.push_str("这些行的共同值：");
        for field in common {
            if text.chars().count() + field.chars().count() <= 480 {
                text.push_str(&field);
            }
        }
    }
    text.push_str("统计仅指本次读取范围。");
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
    XlsxRow {
        text,
        // A derived window summary describes the cited rows; only a total
        // explicitly present in the workbook receives the Total priority.
        kind: ContentUnitKind::TableRow,
        ranges,
        separator: None,
        cells: vec![],
    }
}
