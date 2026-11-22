use super::*;
use crate::contract::SourceRangeUnit;
use crate::office::spreadsheet::is_header_row;

#[test]
fn amount_series_keeps_each_value_with_its_own_person_months_and_rows() {
    let row = |number, cells: &[&str], kind| XlsxRow {
        text: cells.join(" | "),
        kind,
        ranges: vec![SourceRange::new(
            SourceRangeUnit::SheetRow,
            number,
            number + 1,
            Some("Data".into()),
        )],
        separator: None,
        cells: cells.iter().map(|cell| (*cell).into()).collect(),
    };
    let rows = vec![
        row(
            1,
            &["Name", "Month", "Amount"],
            ContentUnitKind::TableHeader,
        ),
        row(2, &["Alpha", "2024-01", "2360"], ContentUnitKind::TableRow),
        row(3, &["Alpha", "2024-02", "2360"], ContentUnitKind::TableRow),
        row(4, &["Alpha", "2024-03", "30000"], ContentUnitKind::TableRow),
        row(5, &["Alpha", "2024-04", "30000"], ContentUnitKind::TableRow),
        row(6, &["Beta", "2024-01", "90000"], ContentUnitKind::TableRow),
        row(7, &["Beta", "2024-02", "90000"], ContentUnitKind::TableRow),
    ];
    let summaries = xlsx_window_summaries(&rows);
    let first = summaries
        .iter()
        .find(|row| row.text.contains("Name = Alpha；Amount = 2360"))
        .unwrap();
    assert!(first
        .text
        .contains("Amount = 2360（对应Month = 2024-01、2024-02）"));
    assert!(first
        .text
        .contains("Amount = 30000（对应Month = 2024-03、2024-04）"));
    assert!(!first.text.contains("90000"));
    assert_eq!((first.ranges[0].start, first.ranges[0].end), (2, 6));

    // A blank trailing identity cell can be absent from a sparse row.
    let sparse = [
        row(
            1,
            &["Month", "Amount", "Name"],
            ContentUnitKind::TableHeader,
        ),
        row(2, &["2024-01", "2360"], ContentUnitKind::TableRow),
        row(3, &["2024-02", "30000", "Alpha"], ContentUnitKind::TableRow),
    ];
    let summaries = xlsx_window_summaries(&sparse);
    assert_eq!(summaries.len(), 1);
    assert!(!summaries[0].text.contains("Name = Alpha"));
}

#[test]
fn multirow_headers_keep_identity_period_and_salary_fields_together() {
    let row = |number, cells: &[&str], kind| XlsxRow {
        text: cells.join(" | "),
        kind,
        ranges: vec![SourceRange::new(
            SourceRangeUnit::SheetRow,
            number,
            number + 1,
            Some("Payroll".into()),
        )],
        separator: None,
        cells: cells.iter().map(|cell| (*cell).into()).collect(),
    };
    let continuation = ["", "", "出勤系数（天数）", "应付工资"];
    assert!(is_header_row(
        &continuation.iter().map(|s| (*s).into()).collect::<Vec<_>>()
    ));
    let rows = vec![
        row(1, &["Name", "Month", "", ""], ContentUnitKind::TableHeader),
        row(2, &continuation, ContentUnitKind::TableHeader),
        row(
            3,
            &["Alpha", "2024-01", "1", "100"],
            ContentUnitKind::TableRow,
        ),
        row(
            4,
            &["Alpha", "2024-02", "1", "200"],
            ContentUnitKind::TableRow,
        ),
        row(
            5,
            &["Beta", "2024-01", "1", "999"],
            ContentUnitKind::TableRow,
        ),
        row(6, &["", "", "1", "300"], ContentUnitKind::TableRow),
    ];
    let summaries = xlsx_window_summaries(&rows);
    let series = summaries
        .iter()
        .find(|row| row.text.contains("Name = Alpha；应付工资"))
        .expect("the continuation header must retain the identity and period columns");
    assert!(series
        .text
        .contains("应付工资 = 100（对应Month = 2024-01）"));
    assert!(series
        .text
        .contains("应付工资 = 200（对应Month = 2024-02）"));
    assert!(!series.text.contains("999") && !series.text.contains("300"));
    let projected = xlsx_unidentified_numeric_rows(&rows);
    assert_eq!(projected.len(), 1);
    assert!(projected[0].text.contains("应付工资 = 300"));
}

#[test]
fn unnamed_numeric_rows_keep_header_fields_without_becoming_totals() {
    let row = |number, cells: &[&str], kind| XlsxRow {
        text: cells.join(" | "),
        kind,
        ranges: vec![SourceRange::new(
            SourceRangeUnit::SheetRow,
            number,
            number + 1,
            Some("Payroll".into()),
        )],
        separator: None,
        cells: cells.iter().map(|cell| (*cell).into()).collect(),
    };
    let rows = vec![
        row(1, &["", "应付工资", ""], ContentUnitKind::TableHeader),
        row(
            2,
            &["姓名", "", "代扣代缴合计"],
            ContentUnitKind::TableHeader,
        ),
        row(3, &["Alpha", "10", "2"], ContentUnitKind::TableRow),
        row(4, &["", "13", "5"], ContentUnitKind::TableRow),
        row(5, &["", "0", "0"], ContentUnitKind::Placeholder),
        row(6, &["合计", "13", "5"], ContentUnitKind::Total),
    ];
    let projected = xlsx_unidentified_numeric_rows(&rows);
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].kind, ContentUnitKind::TableRow);
    assert!(projected[0]
        .text
        .contains("应付工资 = 13；代扣代缴合计 = 5"));
    assert!(projected[0].text.contains("第 4 行，未标姓名"));
    assert!(!projected[0].text.contains("Alpha") && !projected[0].text.contains("元"));
    assert_eq!(
        (projected[0].ranges[0].start, projected[0].ranges[0].end),
        (4, 5)
    );
}

#[test]
fn amount_bands_keep_their_company_type_and_two_original_rows() {
    let row = |number, cells: &[&str]| XlsxRow {
        text: cells.join(" | "),
        kind: ContentUnitKind::TableRow,
        ranges: vec![SourceRange::new(
            SourceRangeUnit::SheetRow,
            number,
            number + 1,
            Some("Approvals".into()),
        )],
        separator: None,
        cells: cells.iter().map(|cell| (*cell).into()).collect(),
    };
    let rows = vec![
        row(1, &["所属公司", "送审合同类型", "审批流"]),
        row(2, &["", "", "金额 [0, 50000) 元", "金额 [50000, +∞) 元"]),
        row(3, &["Alpha", "标准文本", "Approval A", "Approval B"]),
        row(4, &["", "", "金额 [0, 5000) 元", "金额 [5000, +∞) 元"]),
        row(5, &["Beta", "非标准文本", "Approval C", "Approval D"]),
    ];
    let projected = xlsx_band_rows(&rows);
    assert_eq!(projected.len(), 2);
    assert!(projected[0].text.contains("Alpha；送审合同类型 = 标准文本"));
    assert!(
        projected[0].text.contains("[0, 50000) 元") && !projected[0].text.contains("[0, 5000) 元")
    );
    assert_eq!(
        projected[0]
            .ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect::<Vec<_>>(),
        [(2, 3), (3, 4)]
    );
    let mut nonadjacent = rows;
    nonadjacent[2].ranges[0].start = 9;
    nonadjacent[2].ranges[0].end = 10;
    assert_eq!(xlsx_band_rows(&nonadjacent).len(), 1);
    let single_band = [
        row(1, &["所属公司", "送审合同类型", "审批流"]),
        row(2, &["", "", "金额 [0, +∞) 元"]),
        row(3, &["Alpha", "—（不分类型）", "Director → Seal（协同）"]),
    ];
    let projected = xlsx_band_rows(&single_band);
    assert_eq!(projected.len(), 1);
    assert!(projected[0]
        .text
        .contains("审批流 = Director → Seal（协同）"));
    assert!(projected[0].text.contains("金额 [0, +∞) 元"));
    assert!(projected[0].text.chars().count() <= 480);
}
