use ai_daily_report::{
    model::contracts::{CitedText, ComposedReport, Fact, Matter, SelectedFact},
    reports::{
        data::{CoverageGap, DailyReport, ReportSource},
        render_daily, DAILY_TEMPLATE_V3,
    },
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
fn daily_v3_keeps_conditions_status_sources_and_moves_technical_details_to_appendix() {
    let r = report();
    let body = render_daily(&r, DAILY_TEMPLATE_V3).unwrap();
    for required in [
        "历史补录",
        "不超过50000元且预算内",
        "超过50000元或预算外",
        "财务复核",
        "2个工作日",
        "关联交易不适用",
        "草稿待评审",
        "生效日期尚未确定",
        "[S001](#s001)",
        "<a id=\"s001\"></a>",
    ] {
        assert!(body.contains(required), "{required}");
    }
    assert_eq!(body.matches("### 采购审批调整").count(), 1);
    let appendix = body.find("## 来源与覆盖附录").unwrap();
    assert!(body.find("SHA-256").unwrap() > appendix);
    assert!(body.find("摘录").unwrap() > appendix);
    assert!(!body.contains("$serde_json"));
    assert!(!body.contains('\r'));
    assert!(body.ends_with('\n'));
    assert!(!body.ends_with("\n\n"));
    assert_eq!(body, render_daily(&r, DAILY_TEMPLATE_V3).unwrap());
}

#[test]
fn empty_day_and_no_confirmed_fact_do_not_invent_work() {
    let mut r = report();
    r.quality = "no_changes".into();
    r.sources.clear();
    r.facts.clear();
    r.content = None;
    let body = render_daily(&r, DAILY_TEMPLATE_V3).unwrap();
    assert!(body.contains("未发现修改的文件"));
    assert!(!body.contains("事项详情"));
    r = report();
    r.facts.clear();
    r.content = None;
    r.quality = "partial".into();
    r.gaps.push(CoverageGap::new(
        "MODEL_NO_FACTS",
        "model",
        Some("user_input"),
        "现有材料未提供可确认的具体事项。",
    ));
    let body = render_daily(&r, DAILY_TEMPLATE_V3).unwrap();
    assert!(body.contains("暂不扩写工作进展"));
    assert!(body.contains("MODEL_NO_FACTS"));
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
fn source_excerpt_identity_is_checked_before_model_input_is_prepared() {
    let r = report();
    let source = &r.sources[0];
    let text = &r.facts[0].fact.detail;
    let input = source.input(text).unwrap().unwrap();
    assert_eq!(input.source_id, "user_input");
    assert_eq!(input.text, *text);
    assert_eq!(
        source.input("修改过的正文").unwrap_err().code,
        "SOURCE_EXCERPT_HASH_MISMATCH"
    );
}
