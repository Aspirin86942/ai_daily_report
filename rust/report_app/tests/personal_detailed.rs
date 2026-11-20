use ai_daily_report::{
    config::AppConfig,
    evidence::EvidenceRef,
    personal_recovery,
    publish::Publisher,
    reports::{
        data::{CoverageGap, ReportSource},
        personal::{
            ItemKind, Material, PersonalItem, PersonalReport, Selection, StatementKind,
            DETAILED_SCAN_TEMPLATE, MAX_OVERVIEW_CHARS, MAX_REPORT_BYTES,
        },
    },
    run::{repair_by_id, repair_output, App},
    snapshots::{digest, SnapshotRef},
    store::StagedReport,
    types::{new_run_id, parse_date, RunContext},
};
use ai_daily_worker_contract::{SourceRange, SourceRangeUnit};
use chrono::{TimeZone, Utc};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

fn context() -> RunContext {
    RunContext::new(
        new_run_id(),
        Utc.with_ymd_and_hms(2026, 11, 19, 1, 0, 0).unwrap(),
        parse_date("2026-11-18").unwrap(),
        "isolated".into(),
    )
    .unwrap()
}

fn row(start: u64) -> SourceRange {
    let mut range = SourceRange::new(
        SourceRangeUnit::SheetRow,
        start,
        start + 1,
        Some("采购".into()),
    );
    range.column_start = Some(1);
    range.column_end = Some(4);
    range
}

fn material(path: &str, start: u64, text: &str) -> Material {
    let mut source = ReportSource::user(text, context().started_at);
    source.source_id = format!("src_{}", digest(format!("{path}:{start}").as_bytes()));
    source.source_kind = "file".into();
    source.relative_path = Some(path.into());
    source.extension = Some(".xlsx".into());
    let raw = digest(path.as_bytes());
    source.content_sha256 = Some(raw.clone());
    let evidence = vec![EvidenceRef {
        source_id: source.source_id.clone(),
        version_id: format!("file:{raw}"),
        extraction_id: "scan:fixture".into(),
        unit_id: format!("r{start}"),
        side: "current".into(),
        raw_sha256: raw,
        unit_sha256: source.excerpt_sha256.clone(),
        quote: text.into(),
        quote_sha256: source.excerpt_sha256.clone(),
        unit_char_start: 0,
        input_start: 0,
        input_end: source.excerpt_chars,
        ranges: vec![row(start)],
    }];
    Material {
        source,
        text: text.into(),
        evidence,
        priority: 1,
    }
}

fn report(materials: Vec<Material>) -> PersonalReport {
    let mut selection = Selection {
        materials,
        ..Default::default()
    };
    let files = selection
        .materials
        .iter()
        .filter_map(|m| m.source.relative_path.as_deref())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    selection.summary.observed_files = files;
    selection.summary.inspected_files = files;
    selection.summary.selected_files = files;
    selection.summary.selected_chars = selection
        .materials
        .iter()
        .map(|m| m.source.excerpt_chars)
        .sum();
    selection.validate().unwrap();
    let mut report = PersonalReport::from_selection(&context(), "work", &selection);
    report.schema_version = 6;
    report
}

fn item(title: &str, text: &str, id: &str) -> PersonalItem {
    PersonalItem {
        title: title.into(),
        text: text.into(),
        kind: ItemKind::Progress,
        statement_kind: StatementKind::Observed,
        source_ids: vec![id.into()],
    }
}

#[test]
fn eight_detailed_items_link_to_individual_blocks_and_keep_other_material() {
    let path = "报表/成本.xlsx";
    let quote = "设备\t12\r\n</pre><script>原文</script>\n[原始链接](相对位置)";
    let header = json!([{"text":"科目\t金额（万元）", "ranges":[row(1)]}]);
    let mut a = material(path, 2, quote);
    let mut b = material(path, 3, quote);
    a.source.coverage_limits["personal"] = json!({"context":header});
    b.source.coverage_limits["personal"] = json!({"context":header});
    let mut extra = material(path, 4, "补充部分已有标题\n预算\t150");
    extra.source.coverage_limits["personal"] =
        json!({"context":[{"text":"补充部分已有标题","ranges":[]}]});
    let mut report = report(vec![a, b, extra]);
    for index in 1..=8 {
        let text = format!("第 {index} 项完整条件。{}", "仍待确认。".repeat(55));
        report.items.push(item(
            &format!("进展 {index}"),
            &text,
            &report.sources[0].source_id,
        ));
    }
    report.items[0]
        .source_ids
        .push(report.sources[1].source_id.clone());
    report.items[1].title = "[审批](伪链接)".into();
    report.items[7].kind = ItemKind::NextStep;
    report.items[7].statement_kind = StatementKind::Suggestion;
    let body = report
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    let (overview, details) = body.split_once("## 事项详述").unwrap();
    assert!(overview.contains("## 今日概览"));
    assert_eq!(overview.matches("](#detail-").count(), 3);
    assert!(overview.contains("[\\[审批\\](伪链接)](#detail-2)"));
    for (i, item) in report.items.iter().enumerate() {
        assert!(details.contains(&format!("id=\"detail-{}\"", i + 1)));
        assert!(details.contains(&item.text));
    }
    assert!(details.contains("下一步 · 建议"));
    assert!(details.contains("依据编号 S001、S002"));
    let attachment = report.evidence_attachment.as_ref().unwrap();
    assert!(body.contains(&attachment.link(&report.date)));
    let (cited, supplemental) = attachment.markdown.split_once("## 补充材料").unwrap();
    assert!(cited.contains("## 正文引用"));
    assert!(cited.contains("S001 · 报表/成本.xlsx"));
    assert!(cited.contains("S002 · 报表/成本.xlsx"));
    assert!(cited.contains("采购 · 第 2 行，第 1–3 列"));
    assert!(cited.contains("采购 · 第 3 行，第 1–3 列"));
    assert_eq!(cited.matches("科目\t金额（万元）").count(), 1);
    assert_eq!(
        cited
            .matches("设备\t12\n&lt;/pre&gt;&lt;script&gt;原文&lt;/script&gt;")
            .count(),
        1
    );
    assert!(!cited.contains("<script>"));
    assert!(cited.contains("[查看相同原文](#s001-p2)"));
    assert!(cited.contains("[S001](#s001)、[S002](#s002)"));
    assert!(supplemental.contains("S003 · 报表/成本.xlsx"));
    assert_eq!(supplemental.matches("补充部分已有标题").count(), 1);
    assert!(supplemental.contains("预算\t150"));
    let preview = report.render_preview(DETAILED_SCAN_TEMPLATE).unwrap();
    assert!(preview.ends_with(&attachment.markdown));
    assert!(preview.contains("[查看依据](#item-8)"));
    assert!(!preview.contains("attachments/"));
}

#[test]
fn no_model_items_keeps_uncited_originals_and_recovery_reason() {
    let mut source = material("财务.pdf", 7, "金额\t500\n待补单据");
    source.source.extension = Some(".pdf".into());
    source.evidence[0].ranges = vec![SourceRange::new(SourceRangeUnit::PdfPage, 7, 9, None)];
    source.source.coverage_limits["personal"] = json!({"context":[{
        "text": "第二章 付款条件", "ranges": [SourceRange::new(SourceRangeUnit::PdfPage, 6, 7, None)]}]});
    let mut report = report(vec![source]);
    report.gaps.push(CoverageGap::new(
        "MODEL_INVALID",
        "model",
        None,
        "缺少有效回复。",
    ));
    let body = report
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    assert!(!body.contains("## 今日概览"));
    assert!(body.contains("未能形成可用的事项总结"));
    let saved = serde_json::to_value(&report).unwrap();
    let reasons = personal_recovery::reasons(&saved);
    assert!(reasons.iter().any(|r| r == "MODEL_INVALID"));
    assert!(reasons.iter().any(|r| r == "PERSONAL_SUMMARY_UNAVAILABLE"));
    let attachment = report.evidence_attachment.as_ref().unwrap();
    assert!(attachment.markdown.contains("## 补充材料"));
    assert!(attachment.markdown.contains("第 7–8 页"));
    assert!(attachment.markdown.contains("第 6 页"));
    assert!(attachment.markdown.contains("第二章 付款条件"));
    assert!(attachment.markdown.contains("金额\t500\n待补单据"));
    assert!(!attachment.markdown.contains("## 正文引用"));
    assert_eq!(
        report
            .render_attachment(DETAILED_SCAN_TEMPLATE)
            .unwrap()
            .unwrap()
            .markdown,
        attachment.markdown
    );
}

#[test]
fn required_quote_that_does_not_fit_is_never_truncated_to_publish_a_plan() {
    let quoted = "&".repeat(179_999);
    let mut report = report(vec![
        material("计划.xlsx", 2, &quoted),
        material("补充.xlsx", 7, "附注"),
    ]);
    let mut plan = item(
        "核对计划",
        &"界".repeat(35_000),
        &report.sources[0].source_id,
    );
    plan.kind = ItemKind::NextStep;
    plan.statement_kind = StatementKind::Suggestion;
    report.items.push(plan);
    assert_eq!(
        report
            .render_detailed_output(DETAILED_SCAN_TEMPLATE)
            .unwrap_err()
            .code,
        "PERSONAL_REPORT_TOO_LARGE"
    );
    assert_eq!(report.items.len(), 1);
    assert_eq!(
        report.evidence[&report.sources[0].source_id][0].quote,
        quoted
    );
    assert!(report.evidence_attachment.is_none());
    assert!(report.gaps.is_empty());
}

#[test]
fn all_items_are_validated_even_after_the_historical_tenth_item() {
    let mut report = report(vec![material("数据.xlsx", 2, "材料")]);
    report.items = vec![item("待核", "记录", &report.sources[0].source_id); 12];
    report
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    report.items[11].source_ids.push("missing".into());
    assert_eq!(
        report.validate().unwrap_err().code,
        "PERSONAL_REPORT_INVALID"
    );
    report.items[11].source_ids.pop();
    report.items[11].text = "  ".into();
    assert_eq!(
        report.validate().unwrap_err().code,
        "PERSONAL_REPORT_INVALID"
    );
}

#[test]
fn overview_fits_whole_titles_into_160_unicode_characters_using_original_indices() {
    let mut report = report(vec![material("概览.xlsx", 2, "有依据的记录")]);
    let titles = [
        "🦀".repeat(80),
        "批".repeat(81),
        "行".repeat(79),
        "尾".into(),
        "后一项".into(),
    ];
    report.items = titles
        .iter()
        .map(|title| item(title, "详述保留。", &report.sources[0].source_id))
        .collect();
    let body = report
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    let (overview, details) = body.split_once("## 事项详述").unwrap();
    for index in [1, 3, 4] {
        assert!(overview.contains(&format!("[{}](#detail-{index})", titles[index - 1])));
    }
    assert_eq!(overview.matches("](#detail-").count(), 3);
    for title in &titles {
        assert!(details.contains(title));
    }

    report.items[0].title = "字".repeat(MAX_OVERVIEW_CHARS);
    let body = report
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    assert_eq!(
        body.split_once("## 事项详述")
            .unwrap()
            .0
            .matches("](#detail-")
            .count(),
        1
    );
    report.items[0].title.push('界');
    assert_eq!(
        report
            .render_detailed_output(DETAILED_SCAN_TEMPLATE)
            .unwrap_err()
            .code,
        "PERSONAL_REPORT_INVALID"
    );
    report.schema_version = 5;
    report.evidence_attachment = None;
    report.validate().unwrap();
}

#[test]
fn inherited_header_links_to_its_first_original_block_in_the_same_file() {
    let first = material("汇总.xlsx", 1, "科目\t金额（万元）\n设备\t12");
    let mut later = material("汇总.xlsx", 4, "辅料\t2");
    later.source.coverage_limits["personal"] =
        json!({"context":[{"text":"科目\t金额（万元）","ranges":[row(1)]}]});
    let mut report = report(vec![first, later]);
    report
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    let appendix = report.evidence_attachment.unwrap().markdown;
    assert_eq!(appendix.matches("科目\t金额（万元）").count(), 1);
    assert!(appendix.contains("[查看相同原文](#s001-p1)"));
    assert!(appendix.contains("第 4 行"));
}

#[test]
fn byte_budget_removes_whole_supplements_and_keeps_all_details_and_the_next_step() {
    let mut report = report(vec![
        material("计划.xlsx", 2, "请在下次核对原始凭证"),
        material("A.xlsx", 2, &"补充甲".repeat(1000)),
        material("B.xlsx", 2, &"补充乙".repeat(1000)),
    ]);
    report.items = vec![item(
        "核对凭证",
        "下一步核对凭证。",
        &report.sources[0].source_id,
    )];
    report.items[0].kind = ItemKind::NextStep;
    report.items[0].statement_kind = StatementKind::Suggestion;
    report.quality = "complete".into();
    let original = report
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    let headroom = MAX_REPORT_BYTES - report.output_bytes(&original);
    report.items[0]
        .text
        .push_str(&"界".repeat(headroom / 3 + 1));
    let reduced = report
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    assert!(report.output_bytes(&reduced) <= MAX_REPORT_BYTES);
    assert!(reduced.contains(&report.items[0].text));
    assert!(reduced.contains("下一步 · 建议"));
    let attachment = report.evidence_attachment.as_ref().unwrap();
    assert!(attachment.markdown.contains("请在下次核对原始凭证"));
    assert!(attachment.markdown.contains(&"补充甲".repeat(1000)));
    assert!(!attachment.markdown.contains("补充乙"));
    assert!(attachment.markdown.contains("省略了 1 块补充材料"));
    assert!(report.gaps.iter().any(|gap| gap.message.contains("B.xlsx")));
    assert_eq!(report.quality, "partial");
    assert_eq!(report.sources.len(), 3);
    let saved = serde_json::to_vec(&report).unwrap();
    assert_eq!(
        report
            .render_detailed_output(DETAILED_SCAN_TEMPLATE)
            .unwrap(),
        reduced
    );
    assert_eq!(serde_json::to_vec(&report).unwrap(), saved);
}

#[test]
fn exact_byte_limit_restores_frozen_files_and_rejects_oversize_or_external_edits() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: absent-work\narchive:\n  allow_work_dir_for_model: true\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    let ctx = context();
    let now = ctx.started_at;
    let app = App::open_history(config.clone(), now).unwrap();
    let mut report = report(vec![
        material("金额.xlsx", 2, "金额\t125\n单位：万元"),
        material("金额.xlsx", 3, "金额\t18"),
    ]);
    for source in &mut report.sources {
        source.coverage_limits["personal"]["absolute_path"] =
            json!(config.paths.work_dir.join("金额.xlsx"));
    }
    report.items.push(item(
        "款项待核实",
        "记录中的款项仍待核实。",
        &report.sources[0].source_id,
    ));
    report.items[0]
        .source_ids
        .push(report.sources[1].source_id.clone());
    let frozen = DETAILED_SCAN_TEMPLATE
        .replace("事项详述", "已冻结的事项详述")
        .replace("观察时间", "已冻结的观察时间");
    let base = report.render_detailed_output(&frozen).unwrap();
    let remaining = MAX_REPORT_BYTES - report.output_bytes(&base);
    report.items[0].text.push_str(&"🦀".repeat(remaining / 4));
    report.items[0].text.push_str(&"x".repeat(remaining % 4));
    let body = report.render_detailed_output(&frozen).unwrap();
    assert_eq!(report.output_bytes(&body), MAX_REPORT_BYTES);
    let value = serde_json::to_value(&report).unwrap();
    let attachment = report.evidence_attachment.as_ref().unwrap().clone();
    let publisher = Publisher::new(&config.paths.reports_dir, &app.store).unwrap();
    let path = publisher.output_path(&report.date).unwrap();
    let dest = path.to_str().unwrap();
    let appendix = path.parent().unwrap().join(attachment.link(&report.date));
    let staged = StagedReport {
        report: &value,
        markdown: &body,
        output_path: dest,
        publication_fingerprint: &digest(body.as_bytes()),
    };
    let prepare = |ctx: &RunContext, reason| {
        app.store
            .create_personal(ctx, "report", &ctx.run_id, reason, now)
            .unwrap();
        let expired = SnapshotRef {
            path: "unavailable/input".into(),
            sha256: digest(b"expired"),
        };
        app.store
            .prepare(&ctx.run_id, &expired, &expired.sha256, &[], now)
            .unwrap();
    };
    prepare(&ctx, None);
    app.store.stage_report(&ctx.run_id, &staged, now).unwrap();
    publisher
        .publish_with_config(&ctx.run_id, now, &config)
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), body.as_bytes());
    assert_eq!(fs::read(&appendix).unwrap(), attachment.markdown.as_bytes());
    let (history, missing) = app
        .store
        .reports_in_range(ctx.target_date, ctx.target_date)
        .unwrap();
    assert!(missing.is_empty());
    assert_eq!(history.as_slice(), std::slice::from_ref(&value));
    let indexed: u32 = app
        .store
        .connection()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM report_run_sources WHERE run_id=?1",
            [&ctx.run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed, 2);
    let connection = app.store.connection().unwrap();
    connection
        .execute(
            "UPDATE report_runs SET markdown=markdown||'界' WHERE run_id=?1",
            [&ctx.run_id],
        )
        .unwrap();
    assert_eq!(
        app.store
            .reports_in_range(ctx.target_date, ctx.target_date)
            .unwrap_err()
            .code,
        "PERSONAL_REPORT_TOO_LARGE"
    );
    connection
        .execute(
            "UPDATE report_runs SET markdown=?1 WHERE run_id=?2",
            [&body, &ctx.run_id],
        )
        .unwrap();
    // Even matching hashes cannot make a body that links to a different attachment usable.
    let wrong_link = body.replace(&attachment.sha256, &"0".repeat(64));
    let change_body = |markdown: &str| {
        connection
            .execute(
                "UPDATE report_runs SET markdown=?1,expected_sha256=?2 WHERE run_id=?3",
                rusqlite::params![markdown, digest(markdown.as_bytes()), ctx.run_id],
            )
            .unwrap();
        fs::write(&path, markdown).unwrap();
    };
    change_body(&wrong_link);
    assert_eq!(
        app.store
            .reports_in_range(ctx.target_date, ctx.target_date)
            .unwrap_err()
            .code,
        "PERSONAL_REPORT_INVALID"
    );
    assert_eq!(
        publisher.publish(&ctx.run_id, now).unwrap_err().code,
        "PERSONAL_REPORT_INVALID"
    );
    let health = ai_daily_report::health::check(&config, ctx.target_date, now).unwrap();
    assert!(health
        .checks
        .iter()
        .any(|check| check.code == "REPORT_EVIDENCE_MISMATCH"
            && check.evidence["error_code"] == "PERSONAL_REPORT_INVALID"));
    change_body(&body);
    drop(connection);

    let candidate = context();
    prepare(&candidate, Some("校验边界"));
    report.items[0].text.push('x');
    assert_eq!(
        report.render_detailed_output(&frozen).unwrap_err().code,
        "PERSONAL_REPORT_TOO_LARGE"
    );
    assert_eq!(
        app.store
            .stage_report(
                &candidate.run_id,
                &StagedReport {
                    markdown: &format!("{body}x"),
                    ..staged
                },
                now
            )
            .unwrap_err()
            .code,
        "PERSONAL_REPORT_TOO_LARGE"
    );
    let mut no_attachment = value.clone();
    no_attachment
        .as_object_mut()
        .unwrap()
        .remove("evidence_attachment");
    assert_eq!(
        app.store
            .stage_report(
                &candidate.run_id,
                &StagedReport {
                    report: &no_attachment,
                    ..staged
                },
                now
            )
            .unwrap_err()
            .code,
        "PERSONAL_REPORT_INVALID"
    );
    assert_eq!(
        app.store.current(&report.date).unwrap().unwrap().run_id,
        ctx.run_id
    );
    assert_eq!(fs::read(&path).unwrap(), body.as_bytes());
    drop(app);

    let reopened = App::open_history(config, now).unwrap();
    fs::remove_file(&appendix).unwrap();
    repair_by_id(&reopened, &ctx.run_id, now).unwrap();
    assert_eq!(fs::read(&appendix).unwrap(), attachment.markdown.as_bytes());
    fs::remove_file(&path).unwrap();
    fs::remove_file(&appendix).unwrap();
    let restored = repair_output(&reopened, &report.date, now).unwrap();
    assert_eq!(restored.model_calls, 0);
    assert_eq!(restored.markdown, body);
    assert_eq!(fs::read(&path).unwrap(), body.as_bytes());
    assert_eq!(fs::read(&appendix).unwrap(), attachment.markdown.as_bytes());

    fs::write(&appendix, "附件人工修改").unwrap();
    let health = ai_daily_report::health::check(&reopened.config, ctx.target_date, now).unwrap();
    assert!(health
        .checks
        .iter()
        .any(|check| check.code == "REPORT_EVIDENCE_MISMATCH" && check.level == "error"));
    assert_eq!(
        repair_output(&reopened, &report.date, now)
            .unwrap_err()
            .code,
        "REPORT_EVIDENCE_CONFLICT"
    );
    assert_eq!(fs::read_to_string(&appendix).unwrap(), "附件人工修改");
    assert_eq!(fs::read(&path).unwrap(), body.as_bytes());
    fs::write(&path, "正文人工修改").unwrap();
    assert_eq!(
        repair_by_id(&reopened, &ctx.run_id, now).unwrap_err().code,
        "OUTPUT_CONFLICT"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "正文人工修改");
    assert!(reopened.store.calls(&ctx.run_id).unwrap().is_empty());
}
