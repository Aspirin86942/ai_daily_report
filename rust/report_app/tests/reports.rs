use ai_daily_report::{
    evidence::EvidenceRef,
    reports::{
        data::ReportSource,
        personal::{validate_items, ItemKind, PersonalItem, StatementKind},
    },
    snapshots::{canonical_json, digest},
    types::{parse_date, RunContext},
};
use serde_json::{json, Value};

const V3: &[u8] = include_bytes!("fixtures/retired/report-v3.json");
const V4: &str = include_str!("fixtures/readability/v4-input.json");
const V4_BODY: &str = include_str!("fixtures/readability/v4-expected.md");

#[test]
fn invalid_source_identity_and_window_fail_at_the_report_boundary() {
    let report: Value = serde_json::from_slice(V3).unwrap();
    let original: ReportSource = serde_json::from_value(report["sources"][0].clone()).unwrap();
    original.validate().unwrap();
    let mut changed = original.clone();
    changed.content_sha256 = Some("wrong".into());
    assert!(changed.validate().is_err());
    let mut changed = original.clone();
    changed.model_chars += 1;
    assert!(changed.validate().is_err());
    let mut changed = original;
    changed.source_id = format!("src_{}", digest(b"other"));
    assert!(changed.validate().is_err());

    let mut ctx = RunContext::new(
        "1".repeat(32),
        serde_json::from_value(report["generated_at"].clone()).unwrap(),
        parse_date(report["date"].as_str().unwrap()).unwrap(),
        "a".repeat(64),
    )
    .unwrap();
    ctx.window_start_utc = serde_json::from_value(report["window_start_utc"].clone()).unwrap();
    ctx.window_end_utc = serde_json::from_value(report["window_end_utc"].clone()).unwrap();
    ctx.validate().unwrap();
    ctx.window_end_utc = "2026-10-12T16:00:00Z".parse().unwrap();
    assert!(ctx.validate().is_err());

    let saved: Value = serde_json::from_str(V4).unwrap();
    let mut source: ReportSource = serde_json::from_value(saved["sources"][0].clone()).unwrap();
    let fact = &saved["facts"][0]["fact"];
    let observed = [PersonalItem {
        title: fact["topic"].as_str().unwrap().into(),
        text: fact["detail"].as_str().unwrap().into(),
        kind: ItemKind::Progress,
        statement_kind: StatementKind::Observed,
        source_ids: vec![source.source_id.clone()],
    }];
    validate_items(&observed, std::slice::from_ref(&source)).unwrap();
    source.coverage_limits = json!({"ocr":{"text_unverified":true}});
    assert!(validate_items(&observed, &[source]).is_err());
}

#[test]
fn historical_fact_keeps_its_conditions_status_and_evidence() {
    let report: Value = serde_json::from_slice(V3).unwrap();
    assert_eq!(serde_json::to_vec(&report).unwrap(), V3);
    assert_eq!(
        digest(V3),
        "897daeed22e5365de801398e89b21922326c50ca21cdedc80ec4100375d7c957"
    );
    assert_eq!(
        report["facts"][0]["fact"]["detail"],
        report["content"]["matters"][0]["details"][0]["text"]
    );
    assert_eq!(report["facts"][0]["fact"]["status"], "草稿待评审");

    let saved: Value = serde_json::from_str(V4).unwrap();
    assert_eq!(
        digest(&canonical_json(&saved).unwrap()),
        "f43b6e3d3f7ed65d2f1661dcb7fecba3699d7b19dc755e958d45d3c0ca5e322b"
    );
    assert_eq!(
        digest(V4_BODY.as_bytes()),
        "4ac9ae0db26eadf5d5f50ae8dee79db926bf20fbcfed4eaaf8e3bf370be45d5e"
    );
    for source in serde_json::from_value::<Vec<ReportSource>>(saved["sources"].clone()).unwrap() {
        source.validate().unwrap();
    }
    for fact in saved["facts"].as_array().unwrap() {
        for reference in
            serde_json::from_value::<Vec<EvidenceRef>>(fact["evidence"].clone()).unwrap()
        {
            assert_eq!(digest(reference.quote.as_bytes()), reference.quote_sha256);
            assert!(V4_BODY.contains(&reference.quote));
            assert!(V4_BODY.contains(&reference.version_id));
            assert!(V4_BODY.contains(&reference.quote_sha256));
        }
    }
}
