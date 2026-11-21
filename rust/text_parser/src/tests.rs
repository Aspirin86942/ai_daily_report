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
        csv_encodings: ai_daily_scanner_contract::default_csv_encodings(),
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
        csv_encodings: ai_daily_scanner_contract::default_csv_encodings(),
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

    let excerpt = read_head(&mut file, metadata.len(), 1024, None)
        .expect("the original opened handle should remain readable");

    assert_eq!(excerpt.text, "opened content");
    assert!(!excerpt.truncated);
}

#[test]
fn truncated_csv_validates_only_complete_logical_records() {
    let first = "a,b\r\n1,\"two\nlines\"\r\n";
    let rows = parse_csv(first, 10, false).unwrap();
    for tail in ["\"x\"oops", "a\rb", "12345678901"] {
        let truncated = format!("{first}{tail}");
        assert_eq!(parse_csv(&truncated, 10, true).unwrap(), rows);
        assert!(parse_csv(&truncated, 10, false).is_err());
        assert!(parse_csv(&format!("{truncated}\n"), 10, true).is_err());
    }
    let quoted = format!("{first}2,\"not yet closed\n");
    assert_eq!(parse_csv(&quoted, 10, true).unwrap(), rows);
    assert!(parse_csv(&quoted, 10, false).is_err());
}
