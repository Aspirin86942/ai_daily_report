use ai_daily_report::reports::evidence_text;
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
}
