use ai_daily_report::{
    config::{AppConfig, ReportProfile},
    reports::{
        data::ReportSource,
        personal::{
            ItemKind, Material, PersonalItem, PersonalReport, Selection, StatementKind, TEMPLATE,
        },
    },
    types::{parse_date, RunContext},
};
use chrono::{TimeZone, Utc};
use std::{collections::BTreeMap, fs};

fn sample() -> PersonalReport {
    let now = Utc.with_ymd_and_hms(2026, 10, 20, 1, 0, 0).unwrap();
    let context = RunContext::new(
        "a".repeat(32),
        now,
        parse_date("2026-10-19").unwrap(),
        "test".into(),
    )
    .unwrap();
    let text = "金额由 100 调整为 120，仍待审批；条件通过后生效，截止日期未确认。";
    let mut selection = Selection::default();
    selection.summary.selected_chars = text.chars().count();
    selection.materials.push(Material {
        source: ReportSource::user(text, now),
        text: text.into(),
        evidence: vec![],
        priority: 0,
    });
    selection.validate().unwrap();
    let mut report = PersonalReport::from_selection(&context, "work", &selection);
    report.items.push(PersonalItem {
        title: "金额调整待审批".into(),
        text: text.into(),
        kind: ItemKind::Attention,
        statement_kind: StatementKind::UserStatement,
        source_ids: vec!["user_input".into()],
    });
    report
}

#[test]
fn user_material_keeps_conditions_and_renders_independent_evidence() {
    let report = sample();
    report.validate().unwrap();
    let body = report.render(TEMPLATE).unwrap();
    for text in [
        "100",
        "120",
        "仍待审批",
        "条件通过后生效",
        "截止日期未确认",
        "用户补充",
        "查看依据",
    ] {
        assert!(body.contains(text), "{text}");
    }
    assert!(!body.contains('\r'));
    assert!(!body.contains("## 用户补充原文"));
    let attachment = report.render_attachment(TEMPLATE).unwrap().unwrap();
    assert!(body.contains(&attachment.link(&report.date)));
    assert!(attachment.markdown.contains("## 用户补充原文"));
    assert!(attachment.markdown.contains("S001"));
    assert!(attachment.markdown.contains("id=\"item-1\""));
    assert!(attachment.markdown.contains(&report.user_notes));
    attachment.validate().unwrap();
}

#[test]
fn repeated_gap_messages_merge_without_losing_their_source_locations() {
    use ai_daily_report::reports::data::CoverageGap;
    let mut report = sample();
    for index in 0..31 {
        let mut gap = CoverageGap::new(
            "BEFORE_MISSING",
            "selection",
            Some(&format!("source-{index}")),
            "缺少目标日前的可用版本，本次读取不能认定为新增成果。",
        );
        gap.relative_path = Some(format!("folder/file-{index}.xlsx"));
        report.gaps.push(gap);
    }
    report.gaps.push(CoverageGap::new(
        "READ_FAILED",
        "read",
        None,
        "部分材料读取失败。",
    ));
    let body = report.render(TEMPLATE).unwrap();
    assert_eq!(body.matches("缺少目标日前的可用版本").count(), 1);
    assert!(body.contains("合并 31 条记录"));
    assert!(body.contains("部分材料读取失败。"));
    let attachment = report.render_attachment(TEMPLATE).unwrap().unwrap();
    assert_eq!(
        attachment
            .markdown
            .matches("缺少目标日前的可用版本")
            .count(),
        1
    );
    for index in 0..31 {
        assert!(attachment
            .markdown
            .contains(&format!("folder/file-{index}.xlsx")));
    }
    assert_eq!(report.gaps.len(), 32);
}

#[test]
fn extended_limits_preserve_legacy_reports_and_preview_has_local_evidence() {
    use ai_daily_report::reports::personal::{body_chars, MAX_BODY_CHARS};
    let mut report = sample();
    report.items[0].text = "中🦀".repeat(290);
    report.evidence_attachment = report.render_attachment(TEMPLATE).unwrap();
    assert!(body_chars(&report.items) <= MAX_BODY_CHARS);
    report.validate().unwrap();
    let preview = report.render_preview(TEMPLATE).unwrap();
    assert!(preview.contains("[查看依据](#item-1)"));
    assert!(preview.contains("id=\"item-1\""));
    assert!(preview.contains("## 用户补充原文"));
    assert!(!preview.contains("attachments/"));
    report.items[0].text.push_str(&"字".repeat(MAX_BODY_CHARS));
    assert!(body_chars(&report.items) > MAX_BODY_CHARS);
    assert!(report.validate().is_err());
    report.evidence_attachment = None;
    report.items[0].text = "中🦀".repeat(290);
    report.validate().unwrap();
    let legacy = serde_json::to_value(&report).unwrap();
    assert!(legacy.get("evidence_attachment").is_none());
    serde_json::from_value::<PersonalReport>(legacy)
        .unwrap()
        .validate()
        .unwrap();
    assert!(report
        .render_attachment("# {{ date }}\n{{ user_notes }}")
        .unwrap()
        .is_none());

    let mut report = sample();
    report.items = vec![report.items[0].clone(); 6];
    report.validate().unwrap();
    report.evidence_attachment = report.render_attachment(TEMPLATE).unwrap();
    assert!(report.validate().is_err());
}

#[test]
fn attached_evidence_rejects_tampering() {
    let mut report = sample();
    report.evidence_attachment = report.render_attachment(TEMPLATE).unwrap();
    report.validate().unwrap();
    report
        .evidence_attachment
        .as_mut()
        .unwrap()
        .markdown
        .push_str("篡改");
    assert!(report.validate().is_err());
}

#[test]
fn rejects_unknown_sources_false_confirmation_and_tampered_text() {
    let mut report = sample();
    report.items[0].source_ids.push("missing".into());
    assert!(report.validate().is_err());
    let mut report = sample();
    report.items[0].statement_kind = StatementKind::Observed;
    assert!(report.validate().is_err());
    let mut report = sample();
    report.user_notes = "已完成审批".into();
    assert!(report.validate().is_err());
    let mut report = sample();
    report.quality = "no_changes".into();
    assert!(report.validate().is_err());
}

#[test]
fn enforces_unicode_body_item_count_and_dates() {
    let mut report = sample();
    report.items = vec![report.items[0].clone(); 11];
    assert!(report.validate().is_err());
    let mut report = sample();
    report.items[0].text = "中🦀".repeat(751);
    assert!(report.validate().is_err());
    let mut report = sample();
    report.window_end_utc = report.window_start_utc.clone();
    assert!(report.validate().is_err());
}

#[test]
fn evidence_profile_preserves_old_serialization_and_fingerprints() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    let path = dir.path().join("config/settings.yaml");
    fs::write(&path, "paths:\n  work_dir: work\n").unwrap();
    let old = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    assert_eq!(old.report.profile, ReportProfile::Evidence);
    assert!(serde_json::to_value(&old.report)
        .unwrap()
        .get("profile")
        .is_none());
    fs::write(
        &path,
        "paths:\n  work_dir: work\nreport:\n  profile: evidence\n",
    )
    .unwrap();
    let explicit = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    assert_eq!(
        old.fingerprint(true).unwrap(),
        explicit.fingerprint(true).unwrap()
    );
    fs::write(
        path,
        "paths:\n  work_dir: work\nreport:\n  profile: personal\n",
    )
    .unwrap();
    let personal = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    assert_eq!(personal.report.profile, ReportProfile::Personal);
    assert_ne!(
        old.fingerprint(true).unwrap(),
        personal.fingerprint(true).unwrap()
    );
}
