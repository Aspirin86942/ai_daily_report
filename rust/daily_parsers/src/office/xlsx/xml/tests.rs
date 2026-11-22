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
        Some(Instant::now()),
        None,
        |_, _| panic!("expired worksheet scan emitted a row"),
    )
    .unwrap();
    assert!(limited);
}

#[test]
fn xml_deadline_checks_long_tokens_rows_and_even_tiny_events() {
    use std::time::Duration;

    let mut clock = XmlDeadline::new(Some(Instant::now() + Duration::from_secs(60)));
    assert!(!clock.expired(0, true));
    clock.end = Some(Instant::now());
    for pos in 1..=255 {
        assert!(!clock.expired(pos, false));
    }
    assert!(clock.expired(256, false));
    assert!(clock.expired(256 + 16 * 1024, false));
    assert!(clock.expired(257 + 16 * 1024, true));
}

#[test]
fn timeout_before_a_long_cell_keeps_only_the_previous_complete_row() {
    let xml = format!(
        "<worksheet><row r='1'><c><v>7</v></c></row>\
        <row r='2'><c><v>{}</v></c></row></worksheet>",
        "x".repeat(80 * 1024)
    );
    let deadline = Instant::now() + std::time::Duration::from_secs(1);
    let mut emitted = Vec::new();
    let limited = visit_sheet_rows(xml.as_bytes(), 10, 10, Some(deadline), None, |n, _| {
        emitted.push(n);
        // Simulate time consumed handling an already completed row.
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        Ok(true)
    })
    .unwrap();
    assert!(limited);
    assert_eq!(emitted, [1]);
}

#[test]
fn wrapped_vertical_sum_is_a_total_but_horizontal_sum_is_not() {
    assert!(vertical_sum_formula("ROUND(SUM($B$2:$B$8),2)", 9));
    assert!(vertical_sum_formula("SUBTOTAL(109,B2:B8)", 9));
    assert!(!vertical_sum_formula("ROUND(SUM(B9:H9),2)", 9));
    assert!(!vertical_sum_formula("SUM(B2:B9)", 9));
}
