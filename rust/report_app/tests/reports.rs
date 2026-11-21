use ai_daily_report::{
    model::contracts::{CitedText, ComposedReport, Fact, Matter, SelectedFact},
    reports::data::{DailyReport, ReportSource},
    snapshots::digest,
};
use chrono::{DateTime, Utc};
use serde_json::json;

fn report() -> DailyReport {
    let now = DateTime::parse_from_rfc3339("2026-10-15T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let original="普通采购：含税金额不超过50000元且预算内，部门负责人审批；超过50000元或预算外，财务复核并交分管负责人审批。紧急采购先经书面确认，2个工作日内补齐单据；关联交易不适用。草稿待评审，生效日期尚未确定。";
    let mut source = ReportSource::user(original, now);
    source.model_chars = source.excerpt_chars;
    let detail = CitedText {
        text: original.into(),
        fact_ids: vec!["batch_000:000".into()],
        source_ids: vec!["user_input".into()],
    };
    DailyReport {
        schema_version: 3,
        report_kind: "file_evidence".into(),
        date: "2026-10-11".into(),
        scope_id: "personal_work".into(),
        root_alias: "合成材料".into(),
        timezone: "Asia/Shanghai".into(),
        window_start_utc: "2026-10-10T16:00:00Z".into(),
        window_end_utc: "2026-10-11T16:00:00Z".into(),
        quality: "complete".into(),
        sources: vec![source],
        gaps: vec![],
        missed_dates: vec![],
        generated_at: now.to_rfc3339(),
        user_notes: String::new(),
        facts: vec![SelectedFact {
            matter_id: None,
            evidence: vec![],
            fact_id: "batch_000:000".into(),
            pending: false,
            fact: Fact {
                topic: "采购审批调整".into(),
                background: None,
                detail: original.into(),
                conditions: vec![],
                status: Some("草稿待评审".into()),
                source_ids: vec!["user_input".into()],
                uncertainty: None,
            },
        }],
        content: Some(ComposedReport {
            schema_version: 3,
            overview: vec![CitedText {
                text: "采购审批方案仍待评审，生效日期尚未确定。".into(),
                ..detail.clone()
            }],
            matters: vec![Matter {
                matter_id: None,
                title: "采购审批调整".into(),
                background: None,
                details: vec![detail],
                status: None,
            }],
            pending: vec![],
        }),
    }
}

#[test]
fn invalid_source_identity_and_window_fail_at_the_report_boundary() {
    let mut r = report();
    r.sources[0].content_sha256 = Some("wrong".into());
    assert!(r.validate().is_err());
    r = report();
    r.sources[0].model_chars += 1;
    assert!(r.validate().is_err());
    r = report();
    r.window_end_utc = "2026-10-12T16:00:00Z".into();
    assert!(r.validate().is_err());
    r = report();
    r.sources[0].coverage_limits = json!({"ocr":{"text_unverified":true}});
    assert!(r.validate().is_err());
    r = report();
    r.sources[0].source_id = format!("src_{}", digest(b"other"));
    assert!(r.validate().is_err());
}

#[test]
fn historical_fact_keeps_its_conditions_status_and_evidence() {
    let original = report();
    let bytes = serde_json::to_vec(&original).unwrap();
    let decoded: DailyReport = serde_json::from_slice(&bytes).unwrap();
    decoded.validate().unwrap();
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);

    let saved: DailyReport =
        serde_json::from_str(include_str!("fixtures/readability/v4-input.json")).unwrap();
    saved.validate().unwrap();
    let body = include_str!("fixtures/readability/v4-expected.md");
    for fact in &saved.facts {
        for reference in &fact.evidence {
            assert!(body.contains(&reference.quote));
            assert!(body.contains(&reference.version_id));
            assert!(body.contains(&reference.quote_sha256));
        }
    }
}
