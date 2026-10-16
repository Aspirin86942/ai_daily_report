use ai_daily_report::reports::{evidence_text, render};
use serde_json::Value;
use std::path::Path;

#[test]
fn historical_templates_match_python_bytes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (fixture, template) in [
        ("daily-v2", "evidence_report_template.md"),
        ("weekly-v1", "weekly_template.md"),
        ("monthly-v1", "monthly_template.md"),
    ] {
        let data: Value = serde_json::from_slice(
            &std::fs::read(root.join(format!("tests/fixtures/{fixture}.json"))).unwrap(),
        )
        .unwrap();
        let source = std::fs::read_to_string(root.join("../../templates").join(template))
            .unwrap()
            .replace("\r\n", "\n");
        let mut result = render(&source, &data, "2026-10-12 09:00").unwrap();
        if fixture == "daily-v2" {
            result = format!("{}\n", result.trim_end());
        }
        assert_eq!(
            result.as_bytes(),
            std::fs::read(root.join(format!("tests/fixtures/{fixture}.md"))).unwrap(),
            "{fixture}"
        );
    }
}

#[test]
fn evidence_escape_preserves_text_and_protects_markdown() {
    assert_eq!(
        evidence_text("<tag> **重点**\r\n[x]"),
        "&lt;tag&gt; \\*\\*重点\\*\\*  \\[x\\]"
    );
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
