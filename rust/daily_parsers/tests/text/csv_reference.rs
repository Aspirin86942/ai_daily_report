use daily_parsers::contract::{ContentUnitKind, SourceRangeUnit};
use daily_parsers::settings::{default_csv_encodings, TextParseProfile};
use daily_parsers::text::parse_light_text;
use std::fs;

#[test]
fn daily_csv_matches_rust_csv_values_record_positions_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.csv");
    let input = "项目,备注,金额（元）\r\nA,,1234.56\r\n\"B,B\",\"第一行\r\n第二行\",-8.20\r\nC,\"他说\"\"已核对\"\"\",0\r\nD,,\r\nE,下一步,9\r\n合计,,1235.36";
    fs::write(&path, input).unwrap();
    let profile = TextParseProfile {
        csv_encodings: default_csv_encodings(),
        backend: "light_text_v2".into(),
        read_head_bytes: 4096,
        read_tail_bytes: 4096,
        max_chars: 4096,
        excerpt_max_chars: 4096,
    };
    let parsed = parse_light_text(&path, ".csv", &profile, 4096).unwrap();
    parsed.evidence_audit.validate(&parsed.content).unwrap();
    assert!(!parsed.truncated);
    assert!(parsed.warnings.is_empty());
    let chars: Vec<_> = parsed.content.chars().collect();
    let units: Vec<_> = parsed
        .evidence_audit
        .content_units
        .unwrap()
        .into_iter()
        .filter(|unit| unit.kind != ContentUnitKind::Heading)
        .collect();
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(input.as_bytes());
    let reference: Vec<_> = reader.records().collect::<Result<_, _>>().unwrap();
    assert_eq!(units.len(), reference.len());
    for (unit, record) in units.iter().zip(reference) {
        let number = record.position().unwrap().record();
        let prefix = if number == 0 {
            "header: ".into()
        } else {
            format!("row {number}: ")
        };
        let expected = prefix + &record.iter().collect::<Vec<_>>().join(" | ");
        let actual: String = chars[unit.text_start as usize..unit.text_end as usize]
            .iter()
            .collect();
        assert_eq!(actual, expected.trim_end());
        assert_eq!(unit.ranges.len(), 1);
        let range = &unit.ranges[0];
        assert_eq!(
            (range.unit, range.start, range.end),
            (SourceRangeUnit::CsvRecord, number + 1, number + 2)
        );
    }
    assert_eq!(units.last().unwrap().kind, ContentUnitKind::Total);
}
