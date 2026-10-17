use ai_daily_report::{
    model::contracts::{CitedText, ComposedReport, Fact, Matter, SelectedFact},
    reports::{
        data::{CoverageGap, DailyReport, ReportSource},
        render_daily, DAILY_TEMPLATE_V3, DAILY_TEMPLATE_V4,
    },
    snapshots::digest,
};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::BTreeSet;

fn v4_fixture() -> DailyReport {
    serde_json::from_str(include_str!("fixtures/readability/v4-input.json")).unwrap()
}

fn fixture_with_matters(count: usize) -> DailyReport {
    let mut report = v4_fixture();
    let fact = report.facts[0].clone();
    let content = report.content.as_mut().unwrap();
    let matter = content.matters[0].clone();
    for index in 2..count {
        let mut fact = fact.clone();
        fact.fact_id = format!("extra:{index:03}");
        fact.matter_id = Some(format!("matter_{}", index + 1));
        fact.evidence.clear();
        let mut matter = matter.clone();
        matter.matter_id = fact.matter_id.clone();
        matter.title = format!("采购审批(新)[集团]事项{}", index + 1);
        for item in matter
            .background
            .iter_mut()
            .chain(&mut matter.details)
            .chain(&mut matter.status)
        {
            item.fact_ids = vec![fact.fact_id.clone()];
        }
        content.matters.push(matter);
        report.facts.push(fact);
    }
    report.validate().unwrap();
    report
}

fn section<'a>(body: &'a str, heading: &str) -> &'a str {
    body.split_once(&format!("{heading}\n"))
        .unwrap()
        .1
        .split("\n## ")
        .next()
        .unwrap()
}

fn assert_clean_whitespace(body: &str) {
    assert!(!body.contains('\r'));
    assert!(!body.contains("\n\n\n"));
    assert!(body.ends_with('\n'));
    assert!(!body.ends_with("\n\n"));
    assert!(body.lines().all(|line| line == line.trim_end()));
}

fn report(source_count: usize) -> DailyReport {
    let now = DateTime::parse_from_rfc3339("2026-10-15T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let sources = (0..source_count)
        .map(|index| {
            let mut source = ReportSource::user("合成材料正文", now);
            source.source_id = format!("src_{}", digest(format!("source-{index}").as_bytes()));
            source.source_kind = "file".into();
            source.relative_path = Some(format!("材料{index}.txt"));
            source.extension = Some(".txt".into());
            source.model_chars = source.excerpt_chars;
            source
        })
        .collect();
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
        sources,
        gaps: vec![],
        missed_dates: vec![],
        generated_at: now.to_rfc3339(),
        user_notes: String::new(),
        facts: vec![],
        content: None,
    }
}

fn add_fact(report: &mut DailyReport, pending: bool, source_indices: &[usize]) -> CitedText {
    let fact_id = format!("batch_000:{:03}", report.facts.len());
    let text = format!("合成事实{}", report.facts.len() + 1);
    let source_ids: Vec<_> = source_indices
        .iter()
        .map(|&index| report.sources[index].source_id.clone())
        .collect();
    let cited = CitedText {
        text: text.clone(),
        fact_ids: vec![fact_id.clone()],
        source_ids: source_ids.clone(),
    };
    report.facts.push(SelectedFact {
        fact_id,
        pending,
        matter_id: None,
        evidence: vec![],
        fact: Fact {
            topic: text.clone(),
            background: None,
            detail: text.clone(),
            conditions: vec![],
            status: None,
            source_ids,
            uncertainty: None,
        },
    });
    let content = report.content.get_or_insert_with(|| ComposedReport {
        schema_version: report.schema_version,
        overview: vec![],
        matters: vec![],
        pending: vec![],
    });
    if pending {
        content.pending.push(cited.clone());
    } else {
        content.overview.push(cited.clone());
        content.matters.push(Matter {
            matter_id: None,
            title: text,
            background: None,
            details: vec![cited.clone()],
            status: None,
        });
    }
    cited
}

fn speed(report: &DailyReport) -> String {
    render_daily(
        report,
        "{{ report.speed.sources_total }}/{{ report.speed.sources_usable }}/{{ report.speed.sources_failed }}/{{ report.speed.facts_total }}/{{ report.speed.facts_confirmed }}/{{ report.speed.facts_pending }}/{{ report.speed.gaps_total }}/{{ report.speed.gaps_codes }}/{{ report.speed.quality }}/{{ report.speed | length }}",
    )
    .unwrap()
}

fn paragraphs(report: &DailyReport, path: &str) -> Vec<String> {
    let template = format!(
        "{{% for paragraph in report.{path}.paragraphs %}}{{{{ paragraph }}}}<paragraph>{{% endfor %}}"
    );
    let rendered = render_daily(report, &template).unwrap();
    rendered
        .strip_suffix("<paragraph>\n")
        .unwrap()
        .split("<paragraph>")
        .map(str::to_owned)
        .collect()
}

#[test]
fn speed_counts_complete_reports() {
    let mut report = report(2);
    add_fact(&mut report, false, &[0]);
    add_fact(&mut report, true, &[1]);
    assert_eq!(speed(&report), "2/2/0/2/1/1/0/0/complete/9\n");
}

#[test]
fn speed_uses_source_usability_for_partial_reports() {
    let mut report = report(8);
    report.quality = "partial".into();
    for (source, status) in report.sources[1..4]
        .iter_mut()
        .zip(["error", "timeout", "not_parsed"])
    {
        source.parse_status = status.into();
    }
    report.sources[4].excerpt_chars = 0;
    report.sources[4].model_chars = 0;
    report.sources[5].content_sha256 = None;
    report.sources[6].coverage_limits =
        json!({"evidence_audit": {"primary_status": "empty_excerpt"}});
    add_fact(&mut report, false, &[0]);
    add_fact(&mut report, true, &[7]);
    for index in [1, 2] {
        report.gaps.push(CoverageGap::new(
            "PARSE_FAILED",
            "parse",
            Some(&report.sources[index].source_id),
            "解析失败",
        ));
    }
    assert_eq!(speed(&report), "8/2/6/2/1/1/2/1/partial/9\n");
}

#[test]
fn speed_counts_no_changes_and_views_stay_empty() {
    let mut report = report(0);
    report.quality = "no_changes".into();
    assert_eq!(speed(&report), "0/0/0/0/0/0/0/0/no_changes/9\n");
    assert_eq!(
        render_daily(
            &report,
            "{{ report.toc | length }}/{{ report.matters_view | length }}/{{ report.overview_view | length }}/{{ report.pending_view | length }}/{{ report.gaps_grouped | length }}",
        )
        .unwrap(),
        "0/0/0/0/0\n"
    );
}

#[test]
fn reports_without_composed_content_still_have_speed_and_empty_views() {
    let report = report(1);
    assert_eq!(speed(&report), "1/1/0/0/0/0/0/0/complete/9\n");
    assert_eq!(
        render_daily(
            &report,
            "{{ report.toc | length }}/{{ report.matters_view | length }}/{{ report.overview_view | length }}/{{ report.pending_view | length }}",
        )
        .unwrap(),
        "0/0/0/0\n"
    );
}

#[test]
fn gaps_group_by_first_code_and_keep_first_message_and_unique_labels() {
    let mut report = report(4);
    report.quality = "partial".into();
    for (code, source, message) in [
        ("Z_GAP", Some(1), "首条消息"),
        ("A_GAP", Some(3), "另一类"),
        ("Z_GAP", Some(0), "后续消息"),
        ("Z_GAP", Some(1), "重复来源"),
        ("GLOBAL", None, "全局首条"),
        ("GLOBAL", None, "全局后续"),
        ("A_GAP", None, "同类无来源"),
    ] {
        report.gaps.push(CoverageGap::new(
            code,
            "excerpt",
            source.map(|index| report.sources[index].source_id.as_str()),
            message,
        ));
    }
    assert_eq!(
        render_daily(
            &report,
            "{% for gap in report.gaps_grouped %}[{{ gap.code }}|{{ gap.message }}|{{ gap.label_summary }}|{{ gap.count }}|{{ gap | length }}]{% endfor %}",
        )
        .unwrap(),
        "[Z_GAP|首条消息|S002、S001|3|4][A_GAP|另一类|S004|2|4][GLOBAL|全局首条||2|4]\n"
    );
    assert_eq!(speed(&report), "4/4/0/0/0/0/7/3/partial/9\n");
}

#[test]
fn gap_label_summary_abbreviates_only_above_six_unique_sources() {
    for count in [6, 7, 11] {
        let mut report = report(12);
        report.quality = "partial".into();
        for index in (1..=count).chain([1]) {
            report.gaps.push(CoverageGap::new(
                "PARSE_BOUNDED",
                "parse",
                Some(&report.sources[index].source_id),
                "解析受限",
            ));
        }
        let summary = if count == 6 {
            "S002、S003、S004、S005、S006、S007".into()
        } else {
            format!("S002 等 {count} 个来源")
        };
        assert_eq!(
            render_daily(
                &report,
                "{{ report.gaps_grouped[0].label_summary }}|{{ report.gaps_grouped[0].count }}",
            )
            .unwrap(),
            format!("{summary}|{}\n", count + 1)
        );
    }
}

#[test]
fn long_background_and_detail_split_without_changing_unicode_text() {
    let text = format!(
        "{}。{}；{}。",
        "甲".repeat(181),
        "乙".repeat(181),
        "🦀".repeat(182)
    );
    assert_eq!(text.chars().count(), 547);
    let mut report = report(1);
    let mut cited = add_fact(&mut report, false, &[0]);
    cited.text = text.clone();
    let matter = &mut report.content.as_mut().unwrap().matters[0];
    matter.background = Some(cited.clone());
    matter.details = vec![cited];
    for path in ["matters_view[0].background", "matters_view[0].details[0]"] {
        let paragraphs = paragraphs(&report, path);
        assert_eq!(paragraphs.len(), 3);
        assert_eq!(paragraphs.concat(), text);
        assert_eq!(
            paragraphs
                .iter()
                .map(|part| part.chars().count())
                .collect::<Vec<_>>(),
            [182, 182, 183]
        );
    }
}

#[test]
fn paragraph_count_respects_character_threshold_and_three_paragraph_limit() {
    for chars in [239_usize, 240, 241, 480, 481, 960] {
        let text = format!("{}{}", "甲。".repeat(chars / 2), "尾".repeat(chars % 2));
        let mut report = report(1);
        add_fact(&mut report, false, &[0]);
        report.content.as_mut().unwrap().matters[0].details[0].text = text.clone();
        let paragraphs = paragraphs(&report, "matters_view[0].details[0]");
        assert_eq!(paragraphs.len(), chars.div_ceil(240).min(3), "{chars}");
        assert_eq!(paragraphs.concat(), text);
        assert!(paragraphs[..paragraphs.len() - 1]
            .iter()
            .all(|part| part.ends_with('。')));
    }
}

#[test]
fn paragraph_splitting_handles_missing_and_sparse_cut_points() {
    for (text, expected) in [
        ("甲".repeat(547), 1),
        (format!("{}。", "甲".repeat(546)), 1),
        ("甲，".repeat(274), 1),
        (format!("{}；{}", "甲".repeat(273), "乙".repeat(273)), 2),
        (format!("。；{}", "甲".repeat(545)), 3),
        (
            format!(
                " {}。\n{}；{} ",
                "甲".repeat(180),
                "乙".repeat(180),
                "丙".repeat(180)
            ),
            3,
        ),
    ] {
        let mut report = report(1);
        add_fact(&mut report, false, &[0]);
        report.content.as_mut().unwrap().matters[0].details[0].text = text.clone();
        let paragraphs = paragraphs(&report, "matters_view[0].details[0]");
        assert_eq!(paragraphs.len(), expected);
        assert_eq!(paragraphs.concat(), text);
        assert!(paragraphs.iter().all(|part| !part.is_empty()));
    }
}

#[test]
fn toc_threshold_and_matter_anchors_preserve_order_and_titles() {
    for count in [3, 9, 10, 12] {
        let mut report = report(1);
        add_fact(&mut report, false, &[0]);
        let content = report.content.as_mut().unwrap();
        let matter = content.matters[0].clone();
        content.matters = (1..=count)
            .map(|index| Matter {
                title: format!("事项{index}"),
                ..matter.clone()
            })
            .collect();
        let expected: String = (1..=count)
            .map(|index| format!("m{index}:事项{index};"))
            .collect();
        assert_eq!(
            render_daily(&report, "{% for matter in report.matters_view %}{{ matter.anchor }}:{{ matter.title }};{% endfor %}").unwrap(),
            format!("{expected}\n")
        );
        let toc = if count >= 10 { expected } else { String::new() };
        assert_eq!(
            render_daily(
                &report,
                "{% for item in report.toc %}{{ item.anchor }}:{{ item.title }};{% endfor %}"
            )
            .unwrap(),
            format!("{toc}\n")
        );
    }
}

#[test]
fn details_deduplicate_only_exact_fact_ids_and_text_within_each_matter() {
    let mut report = report(3);
    let mut first = add_fact(&mut report, false, &[0, 2]);
    let mut second = add_fact(&mut report, false, &[0, 2]);
    first.text = "共同文本".into();
    second.text = first.text.clone();
    let mut duplicate = first.clone();
    duplicate.source_ids.reverse();
    let mut changed = first.clone();
    changed.text = "不同文本".into();
    let mut combined = first.clone();
    combined.fact_ids.extend(second.fact_ids.clone());
    let mut reordered = combined.clone();
    reordered.fact_ids.reverse();
    let content = report.content.as_mut().unwrap();
    content.matters[0].details = vec![
        first.clone(),
        duplicate,
        changed,
        second,
        combined,
        reordered,
    ];
    content.matters[1].details = vec![first];
    let before = serde_json::to_value(&report).unwrap();
    assert_eq!(
        render_daily(
            &report,
            "{{ report.matters_view[0].details | length }}/{{ report.matters_view[1].details | length }}/{{ report.matters_view[0].details[0].refs }}|{% for item in report.matters_view[0].details %}{{ item.paragraphs | join('') }};{% endfor %}",
        )
        .unwrap(),
        "5/1/[S001](#s001)、[S003](#s003)|共同文本;不同文本;共同文本;共同文本;共同文本;\n"
    );
    assert_eq!(serde_json::to_value(&report).unwrap(), before);
}

#[test]
fn overview_pending_and_status_keep_complete_references_without_splitting() {
    let mut report = report(3);
    let mut confirmed = add_fact(&mut report, false, &[2, 0, 1]);
    let mut pending = add_fact(&mut report, true, &[0, 2]);
    let text = "完整保留。".repeat(110);
    confirmed.text = text.clone();
    pending.text = text.clone();
    let content = report.content.as_mut().unwrap();
    content.overview = vec![confirmed.clone()];
    content.pending = vec![pending];
    content.matters[0].status = Some(confirmed);
    for path in [
        "overview_view[0]",
        "pending_view[0]",
        "matters_view[0].status",
    ] {
        assert_eq!(paragraphs(&report, path), std::slice::from_ref(&text));
    }
    assert_eq!(
        render_daily(
            &report,
            "{{ report.overview_view[0].refs }}|{{ report.pending_view[0].refs }}|{{ report.matters_view[0].status.refs }}",
        )
        .unwrap(),
        "[S003](#s003)、[S001](#s001)、[S002](#s002)|[S001](#s001)、[S003](#s003)|[S003](#s003)、[S001](#s001)、[S002](#s002)\n"
    );
}

#[test]
fn optional_matter_fields_are_null_and_views_support_both_schema_versions() {
    let mut report = report(1);
    add_fact(&mut report, false, &[0]);
    for version in [3, 4] {
        report.schema_version = version;
        report.facts[0].matter_id = Some("matter_1".into());
        let content = report.content.as_mut().unwrap();
        content.schema_version = version;
        content.matters[0].matter_id = Some("matter_1".into());
        assert_eq!(
            render_daily(
                &report,
                "{{ report.matters_view[0].background is none }}/{{ report.matters_view[0].status is none }}/{{ report.matters_view[0].details[0].refs }}/{{ report.matters_view[0] | length }}/{{ report.overview_view[0] | length }}",
            )
            .unwrap(),
            "True/True/[S001](#s001)/5/2\n"
        );
    }
}

#[test]
fn v4_golden_report_matches_reviewed_markdown_bytes() {
    let report = v4_fixture();
    report.validate().unwrap();
    for evidence in &report.facts[0].evidence {
        assert_eq!(digest(evidence.quote.as_bytes()), evidence.quote_sha256);
        assert_eq!(
            evidence.input_end - evidence.input_start,
            evidence.quote.chars().count()
        );
        for range in &evidence.ranges {
            range.validate().unwrap();
        }
    }
    let body = render_daily(&report, DAILY_TEMPLATE_V4).unwrap();
    assert_clean_whitespace(&body);
    assert_eq!(
        body.as_bytes(),
        include_bytes!("fixtures/readability/v4-expected.md")
    );
}

#[test]
fn twelve_matters_render_ordered_sections_toc_and_unique_anchors_in_both_templates() {
    let report = fixture_with_matters(12);
    for template in [DAILY_TEMPLATE_V3, DAILY_TEMPLATE_V4] {
        let body = render_daily(&report, template).unwrap();
        let mut expected = vec![
            "## 内容概览",
            "## 事项目录",
            "## 事项详情",
            "## 待确认",
            "## 来源与覆盖附录",
        ];
        if template == DAILY_TEMPLATE_V4 {
            expected.push("## 变化依据");
        }
        let headings: Vec<_> = body
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect();
        assert_eq!(headings, expected);
        assert!(body.find("> 本报告基于 ").unwrap() < body.find(headings[0]).unwrap());
        let toc: Vec<_> = section(&body, "## 事项目录")
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        assert_eq!(toc.len(), 12);
        for (index, matter) in report.content.as_ref().unwrap().matters.iter().enumerate() {
            let anchor = format!("m{}", index + 1);
            assert_eq!(body.matches(&format!("<a id=\"{anchor}\"></a>")).count(), 1);
            assert_eq!(toc[index], format!("- [{}](#{anchor})", matter.title));
        }
        assert_clean_whitespace(&body);
    }
    let v3 = render_daily(&report, DAILY_TEMPLATE_V3).unwrap();
    let v4 = render_daily(&report, DAILY_TEMPLATE_V4).unwrap();
    assert!(!v3.contains("## 变化依据"));
    for heading in ["## 内容概览", "## 事项目录", "## 事项详情", "## 待确认"] {
        assert_eq!(section(&v3, heading), section(&v4, heading));
    }
}

#[test]
fn nine_matters_have_no_toc_heading_or_toc_links_in_both_templates() {
    let report = fixture_with_matters(9);
    for template in [DAILY_TEMPLATE_V3, DAILY_TEMPLATE_V4] {
        let body = render_daily(&report, template).unwrap();
        assert!(!body.contains("## 事项目录"));
        assert!(!body
            .lines()
            .any(|line| line.starts_with("- [") && line.contains("](#m")));
        for index in 1..=9 {
            assert_eq!(body.matches(&format!("<a id=\"m{index}\"></a>")).count(), 1);
        }
        assert_clean_whitespace(&body);
    }
}

#[test]
fn rendered_prose_preserves_symbols_and_escapes_only_leading_hashes() {
    let report = fixture_with_matters(12);
    for template in [DAILY_TEMPLATE_V3, DAILY_TEMPLATE_V4] {
        let body = render_daily(&report, template).unwrap();
        let prose = body.split_once("## 来源与覆盖附录").unwrap().0;
        for noise in [r"\+", r"\(", r"\["] {
            assert!(!prose.contains(noise), "{noise}");
        }
        for text in ["(新)", "[集团]", "+38909%", "错误值#N/A"] {
            assert!(prose.contains(text), "{text}");
        }
        let pending = section(prose, "## 待确认");
        assert!(pending.contains("- \\#制度附件与空字段口径"));
        for line in prose.lines().filter(|line| line.contains(r"\#")) {
            assert!(line.starts_with("- \\#"));
            assert_eq!(line.matches(r"\#").count(), 1);
        }
    }
}

#[test]
fn rendered_long_detail_keeps_text_and_cites_only_the_last_paragraph() {
    let report = v4_fixture();
    let original = &report.content.as_ref().unwrap().matters[0].details[0].text;
    assert!(original.chars().count() > 240);
    assert!(original.contains('。') && original.contains('；'));
    let parts = paragraphs(&report, "matters_view[0].details[0]");
    assert!(parts.len() > 1);
    assert_eq!(parts.concat(), *original);
    for template in [DAILY_TEMPLATE_V3, DAILY_TEMPLATE_V4] {
        let body = render_daily(&report, template).unwrap();
        let matter = body
            .split_once("<a id=\"m1\"></a>\n")
            .unwrap()
            .1
            .split_once("<a id=\"m2\"></a>\n")
            .unwrap()
            .0;
        for (index, part) in parts.iter().enumerate() {
            let rendered = matter.lines().find(|line| line.starts_with(part)).unwrap();
            if index + 1 == parts.len() {
                assert_eq!(rendered, format!("{part} [S001](#s001)"));
            } else {
                assert_eq!(rendered, part);
            }
        }
    }
}

#[test]
fn rendered_gaps_have_one_line_per_code_and_complete_source_summaries() {
    let report = v4_fixture();
    let codes: BTreeSet<_> = report.gaps.iter().map(|gap| &gap.code).collect();
    assert_eq!(codes.len(), 2);
    for template in [DAILY_TEMPLATE_V3, DAILY_TEMPLATE_V4] {
        let body = render_daily(&report, template).unwrap();
        let gaps: Vec<_> = section(&body, "### 覆盖缺口")
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        assert_eq!(gaps.len(), codes.len());
        assert_eq!(
            gaps[0],
            "- S001、S002：材料存在覆盖缺口，相关附件内容待补齐（`MATERIAL_INCOMPLETE`）"
        );
        assert_eq!(
            gaps[1],
            "- 生效日期尚未确认（`EFFECTIVE_DATE_UNCONFIRMED`）"
        );
    }
}

#[test]
fn rendered_statistics_count_partial_reports_and_skip_no_changes() {
    let report = v4_fixture();
    for template in [DAILY_TEMPLATE_V3, DAILY_TEMPLATE_V4] {
        let body = render_daily(&report, template).unwrap();
        let statistics: Vec<_> = body
            .lines()
            .filter(|line| line.starts_with("> 本报告基于 "))
            .collect();
        assert_eq!(
            statistics,
            ["> 本报告基于 2 份材料（读取成功 1、失败 1），提取事实 3 条（确定 2、待确认 1）。覆盖缺口 2 类共 3 项。"]
        );
        let mut no_changes = report.clone();
        no_changes.quality = "no_changes".into();
        no_changes.sources.clear();
        no_changes.facts.clear();
        no_changes.gaps.clear();
        no_changes.content = None;
        let body = render_daily(&no_changes, template).unwrap();
        assert!(!body.contains("> 本报告基于 "));
        assert_clean_whitespace(&body);
    }
}

#[test]
fn v4_change_evidence_keeps_versions_ranges_quotes_and_hashes() {
    let report = v4_fixture();
    let body = render_daily(&report, DAILY_TEMPLATE_V4).unwrap();
    let appendix = section(&body, "## 变化依据");
    let evidence = &report.facts[0].evidence[0];
    for value in [
        &evidence.version_id,
        &evidence.extraction_id,
        &evidence.unit_id,
        &evidence.quote,
        &evidence.quote_sha256,
    ] {
        assert!(appendix.contains(value), "{value}");
    }
    assert!(appendix.contains("本次版本"));
    assert!(appendix.contains("text_char 0–35（方案正文(新)）"));
    for fact in &report.facts {
        assert!(appendix.contains(&format!("### {}\n", fact.fact.topic)));
        if let Some(matter_id) = &fact.matter_id {
            assert!(appendix.contains(&format!("事项标识：`{matter_id}`。")));
        }
    }
}
