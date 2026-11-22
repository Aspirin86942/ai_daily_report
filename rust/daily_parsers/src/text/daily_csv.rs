use super::{
    audited_text, check_time, column_label, format_csv_row, is_attention_fact, matches_label,
    parse_csv, preview_metadata, AuditTextBuilder, ContentUnitKind, LightTextError,
    LightTextWarning, RawExcerpt, Selection, SourceRange, SourceRangeUnit, TextOutcome,
    LEGACY_CSV_FIELD_SIZE_LIMIT,
};
use std::time::Instant;

struct Record {
    text: String,
    cost: u64,
    kind: ContentUnitKind,
    header: Option<usize>,
    units: std::ops::Range<usize>,
    priority: u8,
}

fn is_unit_row(label: &str, cells: &[String]) -> bool {
    let (key, value) = label.split_once([':', '：']).unwrap_or((label, ""));
    let units = [
        "元",
        "万元",
        "亿元",
        "人民币",
        "美元",
        "港元",
        "欧元",
        "千元",
        "件",
        "台",
        "吨",
        "人",
        "小时",
        "天",
        "%",
        "kg",
        "usd",
        "cny",
        "rmb",
    ];
    if !matches_label(
        key.trim(),
        &["单位", "金额单位", "计量单位", "币种", "unit", "units"],
    ) {
        return false;
    }
    let mut fields = cells
        .iter()
        .map(|cell| cell.trim())
        .filter(|cell| !cell.is_empty())
        .skip(1);
    match (value.trim(), fields.next()) {
        ("", Some(unit)) => fields.next().is_none() && matches_label(unit, &units),
        ("", None) => false,
        (_, next) => next.is_none(),
    }
}

fn is_total_row(label: &str) -> bool {
    let label = label.trim_end_matches([':', '：']).trim_end();
    ["合计", "总计", "小计", "总额"]
        .iter()
        .any(|word| label.ends_with(word))
        || matches_label(label, &["total", "subtotal", "grand total"])
}

fn section_header(row: &[String], columns: &[String]) -> bool {
    // A later header needs corroborating column labels. Data may legitimately
    // repeat a single label (for example "单位") alongside an amount.
    const LABELS: &[&str] = &[
        "项目",
        "项目名称",
        "名称",
        "人员",
        "姓名",
        "部门",
        "单位",
        "日期",
        "数量",
        "金额",
        "工时",
        "单价",
        "余额",
        "差额",
        "负责人",
        "状态",
        "备注",
        "序号",
        "item",
        "name",
        "amount",
        "quantity",
        "hours",
        "date",
        "unit",
    ];
    let mut matches = 0;
    for (i, cell) in row.iter().enumerate() {
        let name = column_label(cell);
        if name.is_empty() {
            continue;
        }
        if cell.trim().parse::<f64>().is_ok() {
            return false;
        }
        if columns
            .get(i)
            .is_some_and(|old| name.eq_ignore_ascii_case(column_label(old)))
            || matches_label(name, LABELS)
        {
            matches += 1;
        } else {
            return false;
        }
    }
    matches >= 2
}

fn csv_range(start: usize, end: usize) -> SourceRange {
    SourceRange::new(
        SourceRangeUnit::CsvRecord,
        start as u64 + 1,
        end as u64 + 1,
        None,
    )
}

pub(super) fn audited_csv(
    raw: &RawExcerpt,
    max_chars: u64,
    deadline: Option<Instant>,
) -> Result<TextOutcome, LightTextError> {
    let input = raw.text.trim_start_matches('\u{feff}');
    let rows = match parse_csv(input, LEGACY_CSV_FIELD_SIZE_LIMIT, raw.truncated, deadline) {
        Ok(rows) => rows,
        Err(LightTextError::TimedOut) => return Err(LightTextError::TimedOut),
        Err(_) => {
            return audited_text(
                std::slice::from_ref(raw),
                max_chars,
                false,
                Some(LightTextWarning::CsvPreviewFallback),
                deadline,
            );
        }
    };

    let mut builder = AuditTextBuilder::new(max_chars, "csv");
    if let Some(range) = raw.byte_range() {
        builder.read(range);
    }
    // Keep the original per-record read evidence for small files. Consecutive records
    // form one exact half-open range when listing them would exhaust the audit limit.
    if rows.len() < 512 {
        for index in 0..rows.len() {
            builder.read(csv_range(index, index + 1));
        }
    } else {
        builder.read(csv_range(0, rows.len()));
    }

    let mut records = Vec::with_capacity(rows.len());
    let mut header = None;
    let mut columns = Vec::new();
    let mut units = 0..0;
    let mut after_fact = false;
    for (index, row) in rows.into_iter().enumerate() {
        check_time(deadline)?;
        let label = row
            .iter()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
            .unwrap_or("");
        let kind = if is_unit_row(label, &row) {
            if after_fact {
                units.start = index;
            }
            units.end = index + 1;
            after_fact = false;
            ContentUnitKind::Unit
        } else if header.is_none() || section_header(&row, &columns) {
            if after_fact {
                units = index..index;
            }
            header = Some(index);
            after_fact = false;
            ContentUnitKind::TableHeader
        } else if is_total_row(label) {
            ContentUnitKind::Total
        } else {
            ContentUnitKind::TableRow
        };
        after_fact |= matches!(kind, ContentUnitKind::TableRow | ContentUnitKind::Total);
        let prefix = if kind == ContentUnitKind::TableHeader {
            "header: ".into()
        } else {
            format!("row {index}: ")
        };
        let mut text = prefix + &format_csv_row(&row);
        if kind == ContentUnitKind::TableHeader {
            columns = row;
        }
        text.truncate(text.trim_end().len());
        let priority = if kind == ContentUnitKind::Total {
            0
        } else if is_attention_fact(&text) {
            1
        } else {
            3
        };
        records.push(Record {
            cost: text.chars().count() as u64 + 1,
            text,
            kind,
            header,
            units: units.clone(),
            priority,
        });
    }
    // CSV packs selected rows into the output unit limit after selection.
    let mut selection = Selection::new(records.len(), max_chars.saturating_add(1), usize::MAX);
    if !selection.keep(0..records.len(), |i| records[i].cost) {
        let mut order: Vec<_> = (0..records.len())
            .filter(|&i| {
                matches!(
                    records[i].kind,
                    ContentUnitKind::TableRow | ContentUnitKind::Total
                )
            })
            .collect();
        if let Some(&last) = order.last() {
            records[last].priority = records[last].priority.min(2);
        }
        order.sort_by_key(|&i| records[i].priority);
        for i in order {
            check_time(deadline)?;
            let needed = records[i]
                .header
                .into_iter()
                .chain(
                    records[i]
                        .units
                        .clone()
                        .filter(|&n| records[n].kind == ContentUnitKind::Unit),
                )
                .chain([i]);
            selection.keep(needed, |i| records[i].cost);
        }
    }

    let meta = preview_metadata("CSV preview", raw.source, raw.truncated);
    let include_meta = (meta.chars().count() as u64) < selection.remaining;
    if include_meta {
        builder.push(&meta, ContentUnitKind::Heading, vec![]);
    }
    let kept: Vec<_> = (0..records.len()).filter(|&i| selection.kept[i]).collect();
    let omitted = kept.len() != records.len();
    write_records(&mut builder, &records, &kept, usize::from(include_meta));
    let (content, mut audit, bounded) = builder.finish();
    audit.encoding = Some(raw.encoding.into());
    Ok((content, raw.truncated || omitted || bounded, None, audit))
}

fn write_records(
    builder: &mut AuditTextBuilder,
    records: &[Record],
    kept: &[usize],
    metadata: usize,
) {
    // Short CSVs retain one unit per record. Large ones may pack complete data records,
    // with their original row numbers and exact (possibly disjoint) source ranges.
    let structural = |i: usize| {
        matches!(
            records[i].kind,
            ContentUnitKind::TableHeader | ContentUnitKind::Unit
        )
    };
    let available = 512 - metadata;
    let last_fact = kept.iter().rfind(|&&i| !structural(i)).copied();
    let mut single: Vec<_> = kept
        .iter()
        .map(|&i| structural(i) || records[i].priority < 3 || Some(i) == last_fact)
        .collect();
    // Reserve a unit per important fact, plus at least one for each intervening
    // run of ordinary rows. If this itself exceeds the limit, only headers and
    // units must be singletons. Every data record still carries its own number.
    let mut pack = 1;
    if kept.len() > available {
        for protect_signals in [true, false] {
            if !protect_signals {
                single = kept.iter().map(|&i| structural(i)).collect();
            }
            let singles = single.iter().filter(|&&s| s).count();
            let runs = single.chunk_by(|a, b| a == b).filter(|run| !run[0]).count();
            let normal = kept.len() - singles;
            pack = normal
                .div_ceil(available.saturating_sub(singles + runs).max(1))
                .max(1);
            if singles + runs <= available {
                break;
            }
        }
    }
    let mut cursor = 0;
    while cursor < kept.len() {
        let first = kept[cursor];
        let mut end = cursor + 1;
        let mut ranges = vec![csv_range(first, first + 1)];
        if !single[cursor] {
            while end < kept.len() && end - cursor < pack && !single[end] {
                let next = kept[end];
                if next == kept[end - 1] + 1 {
                    ranges.last_mut().unwrap().end = next as u64 + 2;
                } else if ranges.len() < 128 {
                    ranges.push(csv_range(next, next + 1));
                } else {
                    break;
                }
                end += 1;
            }
        }
        let group = &kept[cursor..end];
        let text = group
            .iter()
            .map(|&i| records[i].text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let kind = if group
            .iter()
            .all(|&i| records[i].kind == records[first].kind)
        {
            records[first].kind
        } else {
            ContentUnitKind::TableRow
        };
        builder.push(&text, kind, ranges);
        cursor = end;
    }
}
