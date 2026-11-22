use daily_parsers::contract::SourceRangeUnit;
use daily_parsers::settings::TextParseProfile;
use daily_parsers::text::{parse_light_text, LightTextWarning};
use std::fs;

fn profile() -> TextParseProfile {
    TextParseProfile {
        csv_encodings: vec!["utf-8".into(), "utf-8-sig".into(), "gb18030".into()],
        backend: "light_text_v2".into(),
        read_head_bytes: 4096,
        read_tail_bytes: 4096,
        max_chars: 4096,
        excerpt_max_chars: 4096,
    }
}

#[test]
fn exact_json_preview_keeps_original_number_lexemes_and_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numbers.json");
    let integer = "8".repeat(350);
    let object = format!(r#"{{"阶段":"待确认","zero":-0,"scientific":1.2E+500,"long":{integer}}}"#);
    let array = format!(r#"["资料",-0,1.2e+500,{integer}]"#);
    let cases = [
        ("90071992547409931234567890", "top_level_type: int"),
        (integer.as_str(), "top_level_type: int"),
        ("-0", "top_level_type: int"),
        ("-0.0", "top_level_type: float"),
        ("1.2E+500", "top_level_type: float"),
        ("3.50e-010", "top_level_type: float"),
        ("-Infinity", "top_level_type: float"),
        ("\"NaN\"", "top_level_type: str"),
        (
            object.as_str(),
            "top_level_keys: long, scientific, zero, 阶段",
        ),
        (array.as_str(), "top_level_type: list\ntop_level_items: 4"),
    ];
    for (input, detail) in cases {
        fs::write(&path, input).unwrap();
        let parsed = parse_light_text(&path, ".json", &profile(), 4096).unwrap();
        let title = if input.starts_with('{') {
            "JSON object preview"
        } else {
            "JSON preview"
        };
        let expected =
            format!("{title}\nexcerpt_source: head\ntruncated: false\n{detail}\n\n{input}");
        assert_eq!(parsed.content.as_bytes(), expected.as_bytes(), "{input}");
        assert!(parsed.warnings.is_empty(), "{input}");
        assert!(!parsed.truncated);
        let ranges = parsed.evidence_audit.read_ranges.unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            (ranges[0].unit, ranges[0].start, ranges[0].end),
            (SourceRangeUnit::Byte, 0, input.len() as u64)
        );
    }
}

#[test]
fn malformed_json_with_large_numbers_keeps_the_original_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid.json");
    for input in ["01", "1e+", r#"{"n":1e500,"broken":"\ud800"}"#] {
        fs::write(&path, input).unwrap();
        let parsed = parse_light_text(&path, ".json", &profile(), 4096).unwrap();
        let expected = format!(
            "JSON preview\nexcerpt_source: head\ntruncated: false\nwarning: JSON_PREVIEW_FALLBACK\n\n{input}"
        );
        assert_eq!(parsed.content.as_bytes(), expected.as_bytes(), "{input}");
        assert_eq!(parsed.warnings, [LightTextWarning::JsonPreviewFallback]);
    }
}

#[test]
fn utf8_and_gb18030_csv_keep_exact_preview_bytes_and_source_offsets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("encoded.csv");
    let utf8 = "项目,状态\r\n日报,通过\r\n".as_bytes();
    let gb18030 =
        b"\xcf\xee\xc4\xbf,\xd7\xb4\xcc\xac\r\n\xc8\xd5\xb1\xa8,\xcd\xa8\xb9\xfd\r\n".as_slice();
    let expected = "CSV preview\nexcerpt_source: head\ntruncated: false\nheader: 项目 | 状态\nrow 1: 日报 | 通过";
    for (encoding, bytes) in [("utf-8", utf8), ("gb18030", gb18030)] {
        fs::write(&path, bytes).unwrap();
        let parsed = parse_light_text(&path, ".csv", &profile(), 4096).unwrap();
        assert_eq!(parsed.content.as_bytes(), expected.as_bytes());
        assert_eq!(parsed.evidence_audit.encoding.as_deref(), Some(encoding));
        assert!(parsed
            .evidence_audit
            .read_ranges
            .unwrap()
            .iter()
            .any(|range| {
                range.unit == SourceRangeUnit::Byte
                    && range.start == 0
                    && range.end == bytes.len() as u64
            }));
        assert!(!parsed.truncated);
    }
}
