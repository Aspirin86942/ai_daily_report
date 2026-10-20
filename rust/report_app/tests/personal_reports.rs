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
        "S001",
    ] {
        assert!(body.contains(text), "{text}");
    }
    assert!(!body.contains('\r'));
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
