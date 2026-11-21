use super::*;

#[test]
fn byte_windows_keep_only_closed_rows_and_shared_strings() {
    let first = "<worksheet><sheetData><row r=\"1\"><c t=\"str\"><v>完整</v></c></row>";
    let second = "<row r=\"2\"><c t=\"str\"><v>采购&amp;交付</v></c></row>";
    let xml = format!("{first}{second}</sheetData></worksheet>");
    for n in first.len()..xml.len() {
        let mut numbers = Vec::new();
        assert!(visit_sheet_rows(
            &xml.as_bytes()[..n],
            10,
            10,
            false,
            None,
            Some(n as u64),
            |number, _| {
                numbers.push(number);
                Ok(true)
            }
        )
        .unwrap_or_else(|e| panic!("row cut {n}: {e}")));
        assert_eq!(
            numbers,
            if n >= first.len() + second.len() {
                vec![1, 2]
            } else {
                vec![1]
            }
        );
    }
    let first = "<sst><si><t>完整</t></si>";
    let second = "<si><r><t>采购&amp;交付</t></r></si>";
    let xml = format!("{first}{second}</sst>");
    for n in first.len()..xml.len() {
        let (strings, limited) = read_string_table(&xml.as_bytes()[..n], 10, None, Some(n as u64))
            .unwrap_or_else(|e| panic!("string cut {n}: {e}"));
        assert!(limited);
        assert_eq!(strings[&0], "完整");
        assert_eq!(
            strings.get(&1).map(String::as_str),
            (n >= first.len() + second.len()).then_some("采购&交付")
        );
    }
    assert!(read_string_table(b"<sst><si><t>x".as_slice(), 10, None, None).is_err());
    let invalid = b"<sst><si><t>\xff</t></si></sst>";
    assert!(read_string_table(invalid.as_slice(), 10, None, None).is_err());
    let bad_end = b"<sst><si></bad>";
    assert!(read_string_table(bad_end.as_slice(), 10, None, Some(bad_end.len() as u64)).is_err());
    assert!(visit_sheet_rows(
        b"<worksheet><row><c>".as_slice(),
        10,
        10,
        false,
        None,
        None,
        |_, _| Ok(true)
    )
    .is_err());
}

#[test]
fn expired_worksheet_deadline_emits_no_unread_rows() {
    let xml = b"<worksheet><sheetData><row><c><v>7</v></c></row></sheetData></worksheet>";
    let limited = visit_sheet_rows(
        xml.as_slice(),
        10,
        10,
        false,
        Some(Instant::now()),
        None,
        |_, _| panic!("expired worksheet scan emitted a row"),
    )
    .unwrap();
    assert!(limited);
}

#[test]
fn wrapped_vertical_sum_is_a_total_but_horizontal_sum_is_not() {
    assert!(vertical_sum_formula("ROUND(SUM($B$2:$B$8),2)", 9));
    assert!(vertical_sum_formula("SUBTOTAL(109,B2:B8)", 9));
    assert!(!vertical_sum_formula("ROUND(SUM(B9:H9),2)", 9));
    assert!(!vertical_sum_formula("SUM(B2:B9)", 9));
}
