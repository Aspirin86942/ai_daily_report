use super::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn log_excerpt_keeps_the_recent_tail_lines() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("app.log");
    let lines: Vec<String> = (0..500)
        .map(|i| format!("2026-08-11 {i:04} 日志行"))
        .collect();
    fs::write(&path, lines.join("\n")).expect("log fixture should be written");
    let profile = TextParseProfile {
        csv_encodings: crate::settings::default_csv_encodings(),
        backend: "light_text_v2".to_string(),
        read_head_bytes: 1_048_576,
        read_tail_bytes: 1_048_576,
        max_chars: 1_000,
        excerpt_max_chars: 1_000,
    };
    let parsed =
        parse_light_text(&path, ".log", &profile, 16 * 1024 * 1024).expect("log should parse");

    assert!(parsed.truncated);
    assert!(parsed.content.contains("Log tail preview"));
    assert!(parsed.content.contains("excerpt_source: tail"));
    assert!(parsed.content.ends_with("2026-08-11 0499 日志行"));
    assert!(parsed.content.chars().count() <= 1_000);
}

#[test]
fn bounded_text_excerpt_keeps_both_ends_of_a_regular_file() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("notes.md");
    let lines: Vec<String> = (0..500)
        .map(|i| format!("2026-08-11 {i:04} 日志行"))
        .collect();
    fs::write(&path, lines.join("\n")).expect("md fixture should be written");
    let profile = TextParseProfile {
        csv_encodings: crate::settings::default_csv_encodings(),
        backend: "light_text_v2".to_string(),
        read_head_bytes: 1_048_576,
        read_tail_bytes: 1_048_576,
        max_chars: 1_000,
        excerpt_max_chars: 1_000,
    };
    let parsed =
        parse_light_text(&path, ".md", &profile, 16 * 1024 * 1024).expect("md should parse");

    assert!(parsed.truncated);
    assert!(parsed.content.contains("2026-08-11 0000 日志行"));
    assert!(parsed.content.contains("2026-08-11 0499 日志行"));
    assert!(parsed.content.chars().count() <= 1_000);
}

#[test]
fn bounded_read_uses_the_same_open_handle_as_its_metadata() {
    let directory = tempdir().expect("temporary root should exist");
    let path = directory.path().join("candidate.txt");
    let archived = directory.path().join("opened.txt");
    fs::write(&path, "opened content").expect("original fixture should be written");
    let mut file = File::open(&path).expect("original fixture should open");
    let metadata = file.metadata().expect("opened metadata should be readable");
    fs::rename(&path, &archived).expect("opened fixture should be renamed");
    fs::write(&path, "replacement content that is much larger")
        .expect("replacement fixture should be written");

    let profile = TextParseProfile {
        backend: "light_text_v2".into(),
        csv_encodings: crate::settings::default_csv_encodings(),
        read_head_bytes: 1024,
        read_tail_bytes: 1024,
        max_chars: 1024,
        excerpt_max_chars: 1024,
    };
    let excerpts = windows::read_excerpts(&mut file, metadata.len(), ".txt", &profile, None)
        .expect("the original opened handle should remain readable");
    let excerpt = &excerpts[0];

    assert_eq!(excerpt.text, "opened content");
    assert!(!excerpt.truncated);
}

#[test]
fn truncated_csv_validates_only_complete_logical_records() {
    let first = "a,b\r\n1,\"two\nlines\"\r\n";
    let rows = parse_csv(first, 10, false, None).unwrap();
    for tail in ["\"x\"oops", "a\rb", "12345678901"] {
        let truncated = format!("{first}{tail}");
        assert_eq!(parse_csv(&truncated, 10, true, None).unwrap(), rows);
        assert!(parse_csv(&truncated, 10, false, None).is_err());
        assert!(parse_csv(&format!("{truncated}\n"), 10, true, None).is_err());
    }
    let quoted = format!("{first}2,\"not yet closed\n");
    assert_eq!(parse_csv(&quoted, 10, true, None).unwrap(), rows);
    assert!(parse_csv(&quoted, 10, false, None).is_err());
}

#[test]
fn expired_light_text_deadline_precedes_opening_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.txt");
    let profile = TextParseProfile {
        backend: "light_text_v2".into(),
        max_chars: 100,
        read_head_bytes: 1024,
        read_tail_bytes: 1024,
        excerpt_max_chars: 100,
        csv_encodings: crate::settings::default_csv_encodings(),
    };
    assert_eq!(
        parse_light_text_until(&missing, ".txt", &profile, 1024, Instant::now()).unwrap_err(),
        LightTextError::TimedOut
    );
}

#[test]
fn light_text_decodes_gb18030_csv_with_original_record_positions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gb18030.csv");
    let source = "项目,状态\r\n日报,通过\r\n";
    let (bytes, _, malformed) = encoding_rs::GB18030.encode(source);
    assert!(!malformed);
    std::fs::write(&path, &bytes).unwrap();
    let profile = TextParseProfile {
        backend: "light_text_v2".into(),
        max_chars: 1000,
        read_head_bytes: 1024,
        read_tail_bytes: 1024,
        excerpt_max_chars: 1000,
        csv_encodings: vec!["utf-8".into(), "gb18030".into()],
    };
    let parsed = parse_light_text(&path, ".csv", &profile, 1024).unwrap();
    assert!(parsed.warnings.is_empty());
    assert!(parsed.content.contains("日报 | 通过"));
    assert_eq!(parsed.evidence_audit.encoding.as_deref(), Some("gb18030"));
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
}
