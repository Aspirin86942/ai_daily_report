use ai_daily_scanner_contract::{AuditWorkerLane, CacheStatus, ParseStatus};
use ai_daily_scanner_core::{decision::ContextFileEvidence, source_allocation::allocate_excerpts};
use ai_daily_worker_contract::{AuditTextBuilder, ContentUnitKind, SourceRange, SourceRangeUnit};

fn evidence(name: &str, extension: &str, rows: usize) -> ContextFileEvidence {
    let mut builder = AuditTextBuilder::new(100_000, "text");
    for row in 1..=rows {
        let range = SourceRange::new(SourceRangeUnit::Paragraph, row as u64, row as u64 + 1, None);
        builder.read(range.clone());
        builder.push(
            &format!("{name} 记录{row}:计划金额1234.56元𠀀，日期2026-09-22。"),
            ContentUnitKind::Paragraph,
            vec![range],
        );
    }
    let (content, audit, _) = builder.finish();
    ContextFileEvidence {
        file_identity: format!("file:{name}"),
        absolute_path: format!("C:\\fixtures\\{name}"),
        relative_path: name.into(),
        extension: extension.into(),
        size_bytes: Some(content.len() as u64),
        content,
        evidence_audit: Some(audit),
        parser_backend: "light_text_v2".into(),
        worker_lane: AuditWorkerLane::RustCore,
        cache_status: CacheStatus::Miss,
        parse_status: ParseStatus::Success,
        truncated: false,
        error: None,
        reason: None,
    }
}

#[test]
fn section_qualifications_share_context_without_repeating_the_heading() {
    let mut file = evidence("qualified.md", ".md", 1);
    let mut builder = AuditTextBuilder::new(10_000, "text");
    builder.push("# 资料摘要", ContentUnitKind::Heading, vec![]);
    builder.push("文件记载验收结论。", ContentUnitKind::Paragraph, vec![]);
    builder.push("## 验收不代表业务确认", ContentUnitKind::Heading, vec![]);
    for (index, text) in [
        "检查结构完整且无环。",
        "逐格回读和写入内容一致。",
        "页行范围覆盖本次读取窗口。",
        "业务含义需要按原文条件理解。",
        "职责分工由审批流实际内容确定。",
        "分支之间的关系按各自条件保留。",
        "技术检查只能说明列出的检查结果。",
        "最后一项限定仍应进入完整摘录。",
    ]
    .into_iter()
    .enumerate()
    {
        let range = SourceRange::new(
            SourceRangeUnit::Paragraph,
            index as u64 + 1,
            index as u64 + 2,
            None,
        );
        builder.read(range.clone());
        builder.push(text, ContentUnitKind::Paragraph, vec![range]);
    }
    let (text, audit, _) = builder.finish();
    file.content = text;
    file.evidence_audit = Some(audit);
    let result = allocate_excerpts(&[&file], 200);
    assert!(result[0].text.contains("最后一项限定仍应进入完整摘录"));
    assert_eq!(result[0].text.matches("## 验收不代表业务确认").count(), 1);
    assert!(result[0].text.chars().count() <= 200);
    result[0]
        .audit
        .as_ref()
        .unwrap()
        .validate(&result[0].text)
        .unwrap();
}

#[test]
fn nonnumeric_qualifications_keep_their_meaning_before_bulk_depth() {
    let mut file = evidence("rules.md", ".md", 1);
    let mut builder = AuditTextBuilder::new(20_000, "text");
    for row in 1..=35 {
        let range = SourceRange::new(SourceRangeUnit::Paragraph, row, row + 1, None);
        builder.read(range.clone());
        builder.push(
            &format!("项目说明第{row}段：{}", "常规背景内容".repeat(30)),
            ContentUnitKind::Paragraph,
            vec![range],
        );
    }
    for (number, text) in [
        (
            36,
            "ok=true仅证明结构与排版，业务口径未经确认，不能据此当OA运行规则。",
        ),
        (37, "每个公司与合同类型组合占两行，超出配置上限必须报错。"),
        (38, "保留覆盖断言：assert nodes == all_nodes。"),
    ] {
        let range = SourceRange::new(SourceRangeUnit::Paragraph, number, number + 1, None);
        builder.read(range.clone());
        builder.push(text, ContentUnitKind::Paragraph, vec![range]);
    }
    let (text, audit, _) = builder.finish();
    file.content = text;
    file.evidence_audit = Some(audit);
    let result = allocate_excerpts(&[&file], 700);
    assert!(result[0].text.contains("不能据此当OA运行规则"));
    assert!(result[0].text.contains("组合占两行"));
    assert!(result[0].text.contains("assert nodes == all_nodes"));
    assert!(result[0].text.chars().count() <= 700);
    result[0]
        .audit
        .as_ref()
        .unwrap()
        .validate(&result[0].text)
        .unwrap();
    assert!(result[0]
        .ranges
        .as_ref()
        .unwrap()
        .iter()
        .any(|range| range.start == 36));
}

#[test]
fn late_small_sources_get_first_excerpts_before_large_sources_get_depth() {
    let mut files = Vec::new();
    for n in 0..9 {
        files.push(evidence(&format!("a{n}.md"), ".md", 150));
    }
    for n in 0..12 {
        files.push(evidence(&format!("z{n}.md"), ".md", 1));
    }
    let excerpts = allocate_excerpts(&files.iter().collect::<Vec<_>>(), 50_000);
    assert_eq!(excerpts.iter().filter(|e| !e.text.is_empty()).count(), 21);
    assert!(
        excerpts
            .iter()
            .map(|e| e.text.chars().count())
            .sum::<usize>()
            <= 50_000
    );
    for excerpt in excerpts {
        excerpt.audit.unwrap().validate(&excerpt.text).unwrap();
    }
}

#[test]
fn limited_round_robin_is_independent_of_enumeration_order() {
    let mut files = [
        evidence("a.md", ".md", 30),
        evidence("b.md", ".md", 30),
        evidence("x.pdf", ".pdf", 1),
        evidence("z.xlsx", ".xlsx", 1),
    ];
    let first = allocate_excerpts(&files.iter().collect::<Vec<_>>(), 900);
    let by_id: std::collections::BTreeMap<_, _> = files
        .iter()
        .zip(first)
        .map(|(f, e)| (f.file_identity.clone(), e.text))
        .collect();
    files.reverse();
    let second = allocate_excerpts(&files.iter().collect::<Vec<_>>(), 900);
    assert_eq!(
        by_id,
        files
            .iter()
            .zip(second)
            .map(|(f, e)| (f.file_identity.clone(), e.text))
            .collect()
    );
}

#[test]
fn oversized_unbroken_text_and_headings_do_not_displace_complete_rows() {
    let mut large = evidence("a.md", ".md", 1);
    large.content = "X".repeat(6000);
    large.evidence_audit = None;
    let small = evidence("z.xlsx", ".xlsx", 1);
    let result = allocate_excerpts(&[&large, &small], 200);
    assert!(result[0].text.is_empty());
    assert!(result[0].truncated);
    assert!(result[1].text.contains("1234.56"));
    assert!(result[1].text.ends_with('。'));
    let mut title = small.clone();
    title.evidence_audit.as_mut().unwrap().usable_chars = Some(0);
    assert!(allocate_excerpts(&[&title], 200)[0].text.is_empty());
}

#[test]
fn table_prefix_and_amount_row_are_kept_together_or_omitted() {
    let mut file = evidence("money.xlsx", ".xlsx", 1);
    let mut builder = AuditTextBuilder::new(1000, "xlsx");
    let range = SourceRange::new(SourceRangeUnit::SheetRow, 3, 4, Some("采购".into()));
    builder.read(range.clone());
    builder.push("## Sheet: 采购", ContentUnitKind::Heading, vec![]);
    builder.push("单位: 元", ContentUnitKind::Unit, vec![]);
    builder.push("| 日期 | 金额 |", ContentUnitKind::TableHeader, vec![]);
    builder.push(
        "| 2026-09-22 | 1234.56 |",
        ContentUnitKind::TableRow,
        vec![range.clone()],
    );
    let (text, audit, _) = builder.finish();
    file.content = text;
    file.evidence_audit = Some(audit);
    let complete = allocate_excerpts(&[&file], 200);
    assert!(complete[0].text.contains("单位: 元"));
    assert_eq!(complete[0].ranges, Some(vec![range]));
    let tiny = allocate_excerpts(&[&file], 20);
    assert!(tiny[0].text.is_empty());
    assert_eq!(tiny[0].audit.as_ref().unwrap().usable_chars, Some(0));
}

#[test]
fn late_total_retains_sheet_header_unit_and_actual_row_before_bulk_depth() {
    let mut file = evidence("payroll.xlsx", ".xlsx", 1);
    let mut builder = AuditTextBuilder::new(20_000, "xlsx");
    builder.push("## Sheet: 费用", ContentUnitKind::Heading, vec![]);
    builder.push("单位: 元", ContentUnitKind::Unit, vec![]);
    builder.push("| 项目 | 金额 |", ContentUnitKind::TableHeader, vec![]);
    for row in 2..=35 {
        let range = SourceRange::new(SourceRangeUnit::SheetRow, row, row + 1, Some("费用".into()));
        builder.read(range.clone());
        builder.push(
            &format!("| 项目{row}{} | 20.25 |", "说明".repeat(24)),
            ContentUnitKind::TableRow,
            vec![range],
        );
    }
    let total_range = SourceRange::new(SourceRangeUnit::SheetRow, 36, 37, Some("费用".into()));
    builder.read(total_range.clone());
    builder.push(
        "| 总计 | 688.50 |",
        ContentUnitKind::Total,
        vec![total_range.clone()],
    );
    let (text, audit, _) = builder.finish();
    file.content = text;
    file.evidence_audit = Some(audit);
    let result = allocate_excerpts(&[&file], 400);
    assert!(result[0].text.contains("| 总计 | 688.50 |"));
    assert!(result[0].text.contains("| 项目 | 金额 |") && result[0].text.contains("单位: 元"));
    assert!(result[0].ranges.as_ref().unwrap().contains(&total_range));
    assert!(result[0].truncated && result[0].text.chars().count() <= 1100);
    result[0]
        .audit
        .as_ref()
        .unwrap()
        .validate(&result[0].text)
        .unwrap();
}

#[test]
fn late_numeric_condition_keeps_exception_as_a_complete_unit() {
    let mut file = evidence("policy.md", ".md", 1);
    let mut builder = AuditTextBuilder::new(20_000, "text");
    builder.push("# 审批条件", ContentUnitKind::Heading, vec![]);
    for row in 1..=50 {
        let range = SourceRange::new(SourceRangeUnit::Paragraph, row, row + 1, None);
        builder.read(range.clone());
        builder.push(
            &format!("常规说明{row}：{}。", "已归档事项".repeat(12)),
            ContentUnitKind::Paragraph,
            vec![range],
        );
    }
    let range = SourceRange::new(SourceRangeUnit::Paragraph, 51, 52, None);
    builder.read(range.clone());
    builder.push(
        "一个会计年度累计超过5万美元须特别批准；正常业务经营过程中发生的除外。",
        ContentUnitKind::Paragraph,
        vec![range.clone()],
    );
    let (text, audit, _) = builder.finish();
    file.content = text;
    file.evidence_audit = Some(audit);
    let result = allocate_excerpts(&[&file], 1100);
    assert!(result[0]
        .text
        .contains("累计超过5万美元须特别批准；正常业务经营过程中发生的除外。"));
    assert!(result[0].ranges.as_ref().unwrap().contains(&range));
    assert!(result[0].truncated);
    result[0]
        .audit
        .as_ref()
        .unwrap()
        .validate(&result[0].text)
        .unwrap();
}

#[test]
fn historical_depth_keeps_every_first_opportunity_and_the_same_global_cap() {
    use ai_daily_scanner_core::source_allocation::allocate_excerpts_with_priority;
    let a = evidence("a.md", ".md", 100);
    let z = evidence("z.md", ".md", 100);
    let files = [&a, &z];
    let regular = allocate_excerpts(&files, 1800);
    let priority = allocate_excerpts_with_priority(&files, 1800, &["z.md".into()]);
    assert!(priority.iter().all(|item| !item.text.is_empty()));
    assert!(priority[1].text.chars().count() > regular[1].text.chars().count());
    assert!(
        priority
            .iter()
            .map(|item| item.text.chars().count())
            .sum::<usize>()
            <= 1800
    );
    for excerpt in priority {
        excerpt.audit.unwrap().validate(&excerpt.text).unwrap();
    }
    let reversed = allocate_excerpts_with_priority(&[&z, &a], 1800, &["z.md".into()]);
    assert_eq!(
        reversed[0].text,
        allocate_excerpts_with_priority(&files, 1800, &["z.md".into()])[1].text
    );
}
