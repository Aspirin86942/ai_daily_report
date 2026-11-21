use ai_daily_report::reports::{evidence_text, render};
#[test]
fn evidence_escape_preserves_text_and_protects_markdown() {
    assert_eq!(
        evidence_text("<tag> **重点**\r\n[x]"),
        "&lt;tag&gt; \\*\\*重点\\*\\*  [x]"
    );
    for (input, expected) in [
        ("(新)", "(新)"),
        ("+38909%", "+38909%"),
        ("A#N/A", "A#N/A"),
        ("#N/A", "\\#N/A"),
        ("{x}!", "{x}!"),
        ("&<>", "&amp;&lt;&gt;"),
        ("\\`*_[x]|", "\\\\\\`\\*\\_[x]\\|"),
        ("A\n#N/A\rB", "A #N/A B"),
        (" #N/A", " #N/A"),
        ("", ""),
    ] {
        assert_eq!(evidence_text(input), expected, "{input:?}");
    }
    let result = render(
        "{% for x in report.items %}{{x}}{% else %}无{% endfor %}|{{report.value or '缺失'}}",
        &serde_json::json!({"items":[],"value":null}),
        "",
    )
    .unwrap();
    assert_eq!(result, "无|缺失");
    assert_eq!(
        render(
            "{{report.count}}/{{report.ratio}}",
            &serde_json::json!({"count":32,"ratio":0.75}),
            ""
        )
        .unwrap(),
        "32/0.75"
    );
}
