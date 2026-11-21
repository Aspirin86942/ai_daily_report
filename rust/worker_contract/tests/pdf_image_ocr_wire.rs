use ai_daily_worker_contract::{ParseRequest, ParserLimits};
use serde_json::{json, Value};

fn request(limits: Value) -> Value {
    json!({
        "file_path": r"D:\synthetic\image.pdf",
        "file_type": ".pdf",
        "backend": "rust_pdfium_v3",
        "remaining_timeout_ms": 15000,
        "max_file_size_bytes": 1048576,
        "expected_source_version": "mtime_ns=100:size=200",
        "parser_limits": limits
    })
}

#[test]
fn historical_pdf_limits_keep_their_bytes_and_reject_undeclared_switches() {
    for wire in [
        r#"{"kind":"pdf","max_pages":2,"excerpt_max_chars":512}"#,
        r#"{"kind":"pdf","max_pages":2,"excerpt_max_chars":512,"selected_pages":[1,2]}"#,
    ] {
        let limits: ParserLimits = serde_json::from_str(wire).unwrap();
        limits.validate().unwrap();
        assert_eq!(serde_json::to_string(&limits).unwrap(), wire);
        let mut extra = request(serde_json::from_str(wire).unwrap());
        extra["offline_ocr"] = json!(true);
        assert!(serde_json::from_value::<ParseRequest>(extra).is_err());
    }
}

#[test]
fn image_ocr_is_a_declared_request_with_an_explicit_bounded_page_list() {
    let limits = json!({"kind":"pdf_image_ocr","max_pages":2,"excerpt_max_chars":8192,"selected_pages":[1,2]});
    let wire = request(limits.clone());
    let parsed: ParseRequest =
        serde_json::from_value(wire.clone()).expect("worker v4 must accept the typed OCR variant");
    parsed.validate().unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), wire);
    for (field, invalid) in [
        ("max_pages", json!(4)),
        ("excerpt_max_chars", json!(8193)),
        ("selected_pages", json!([])),
        ("selected_pages", json!([1, 1])),
        ("selected_pages", json!([0])),
        ("offline_ocr", json!(true)),
    ] {
        let mut bad = limits.clone();
        bad[field] = invalid;
        let value = serde_json::from_value::<ParseRequest>(request(bad));
        assert!(
            value.map_or(true, |r| r.validate().is_err()),
            "accepted invalid {field}"
        );
    }
}
