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

fn file_from_builder(name: &str, builder: AuditTextBuilder) -> ContextFileEvidence {
    let mut file = evidence(name, ".md", 1);
    let (text, audit, truncated) = builder.finish();
    assert!(!truncated);
    file.content = text;
    file.evidence_audit = Some(audit);
    file
}

#[test]
fn unfit_first_total_cannot_block_the_later_complete_total_or_other_files() {
    let mut builder = AuditTextBuilder::new(5000, "xlsx");
    builder.push("| 项目 | 金额 |", ContentUnitKind::TableHeader, vec![]);
    builder.push("单位：万元", ContentUnitKind::Unit, vec![]);
    let huge = format!("| 上期总计{} | 5.20 |", "情况说明".repeat(250));
    for (n, row) in [(2, huge.as_str()), (3, "| 合计 | -812.30 |")] {
        let range = SourceRange::new(SourceRangeUnit::SheetRow, n, n + 1, Some("结算".into()));
        builder.read(range.clone());
        builder.push(row, ContentUnitKind::Total, vec![range]);
    }
    let file = file_from_builder("table.md", builder);
    let other = evidence("small.txt", ".txt", 1);
    let excerpts = allocate_excerpts(&[&file, &other], 100);
    let kept = &excerpts[0];
    assert_eq!(kept.text, "| 项目 | 金额 |\n单位：万元\n| 合计 | -812.30 |");
    assert!(kept.truncated && !excerpts[1].text.is_empty());
    assert_eq!(
        kept.ranges
            .as_ref()
            .unwrap()
            .iter()
            .map(|r| r.start)
            .collect::<Vec<_>>(),
        [3]
    );
    kept.audit.as_ref().unwrap().validate(&kept.text).unwrap();
    assert_eq!(kept.block_ends, [3]);
    assert_eq!(kept.block_context, [Vec::<usize>::new()]);
    let full = allocate_excerpts(&[&file], file.content.chars().count());
    assert_eq!(full[0].text, file.content);
    assert!(!full[0].truncated);
}

#[test]
fn whole_long_text_fits_and_partial_allocation_retains_exact_suffix_range() {
    let mut builder = AuditTextBuilder::new(10_000, "text");
    let suffix = "下一步复核－1234.56万元𠀀";
    let text = format!("{}。  {suffix}", "完整说明".repeat(200));
    let original_start = 37;
    let range = SourceRange::new(
        SourceRangeUnit::TextChar,
        original_start,
        original_start + text.chars().count() as u64,
        None,
    );
    builder.read(range.clone());
    builder.push(&text, ContentUnitKind::Paragraph, vec![range.clone()]);
    let file = file_from_builder("long.md", builder);
    let full = allocate_excerpts(&[&file], text.chars().count());
    assert_eq!(full[0].text, text);
    assert!(!full[0].truncated);
    let limited = allocate_excerpts(&[&file], suffix.chars().count());
    assert_eq!(limited[0].text, suffix);
    assert!(limited[0].truncated);
    let suffix_range = &limited[0].ranges.as_ref().unwrap()[0];
    assert_eq!(
        (suffix_range.start, suffix_range.end),
        (range.end - suffix.chars().count() as u64, range.end)
    );
    limited[0].audit.as_ref().unwrap().validate(suffix).unwrap();
}

#[test]
fn tight_first_round_reserves_one_complete_fact_from_every_feasible_source() {
    let mut a = AuditTextBuilder::new(4000, "text");
    a.push(
        &format!("异常{}", "长".repeat(65)),
        ContentUnitKind::Paragraph,
        vec![],
    );
    a.push("甲方同意。", ContentUnitKind::Paragraph, vec![]);
    let a = file_from_builder("a.md", a);
    let mut b = AuditTextBuilder::new(4000, "text");
    b.push(
        &format!("已完成{}。", "核对".repeat(28)),
        ContentUnitKind::Paragraph,
        vec![],
    );
    let b = file_from_builder("b.md", b);
    let budget = b.content.chars().count() + "甲方同意。".chars().count();
    let result = allocate_excerpts(&[&a, &b], budget);
    assert_eq!(result[0].text, "甲方同意。");
    assert_eq!(result[1].text, b.content);
}

#[test]
fn each_block_explicitly_identifies_only_its_own_reused_table_context() {
    let mut builder = AuditTextBuilder::new(10_000, "text");
    builder.push("# 两张表", ContentUnitKind::Heading, vec![]);
    builder.push("单位：元", ContentUnitKind::Unit, vec![]);
    builder.push("| 部门 | 金额 |", ContentUnitKind::TableHeader, vec![]);
    builder.push(
        &format!("| 甲{} | 21.50 |", "介绍".repeat(140)),
        ContentUnitKind::TableRow,
        vec![],
    );
    builder.push("| 乙 | 89.12 |", ContentUnitKind::TableRow, vec![]);
    builder.push("另行按计划执行。", ContentUnitKind::Paragraph, vec![]);
    builder.push("| 人员 | 工时 |", ContentUnitKind::TableHeader, vec![]);
    builder.push("单位：小时", ContentUnitKind::Unit, vec![]);
    builder.push(
        &format!("| 丙{} | 12 |", "职责".repeat(270)),
        ContentUnitKind::TableRow,
        vec![],
    );
    builder.push("| 丁 | 8 |", ContentUnitKind::TableRow, vec![]);
    let file = file_from_builder("tables.md", builder);
    let excerpts = allocate_excerpts(&[&file], file.content.chars().count());
    let excerpt = &excerpts[0];
    assert_eq!(excerpt.text, file.content);
    let audit = excerpt.audit.as_ref().unwrap();
    audit.validate(&excerpt.text).unwrap();
    let units = audit.content_units.as_ref().unwrap();
    let chars: Vec<_> = excerpt.text.chars().collect();
    assert_eq!(excerpt.block_ends.last(), Some(&units.len()));
    assert_eq!(excerpt.block_ends.len(), excerpt.block_context.len());
    let mut begin = 0;
    let mut verified = 0;
    for (&end, prior) in excerpt.block_ends.iter().zip(&excerpt.block_context) {
        assert!(begin < end);
        assert!(prior.windows(2).all(|w| w[0] < w[1]) && prior.iter().all(|&i| i < begin));
        let body: String = chars
            [units[begin].text_start as usize..units[end - 1].text_end as usize]
            .iter()
            .collect();
        if body.contains("| 乙 |") {
            assert_eq!(prior, &[0, 1, 2]);
            verified += 1;
        }
        if body.contains("另行按计划执行") {
            assert_eq!(prior, &[0]);
            verified += 1;
        }
        if body.contains("| 丁 |") {
            assert_eq!(prior, &[0, 6, 7]);
            verified += 1;
        }
        begin = end;
    }
    assert_eq!(verified, 3);
}

#[test]
fn split_coarse_ranges_obey_the_same_serialized_audit_limit_as_the_parser() {
    let mut builder = AuditTextBuilder::new(20_000, "text");
    let label = "阶段".repeat(230);
    let original = SourceRange::new(SourceRangeUnit::Paragraph, 12, 13, Some(label));
    builder.read(original.clone());
    let mut paragraph = String::from("完成登记。").repeat(700);
    paragraph += "下一步处理错误。";
    builder.push(
        &paragraph,
        ContentUnitKind::Paragraph,
        vec![original.clone()],
    );
    let file = file_from_builder("large.md", builder);
    let excerpts = allocate_excerpts(&[&file], 2800);
    let excerpt = &excerpts[0];
    assert!(excerpt.truncated && excerpt.text.contains("下一步处理错误。"));
    assert_eq!(excerpt.ranges.as_deref(), Some([original].as_slice()));
    let audit = excerpt.audit.as_ref().unwrap();
    audit.validate(&excerpt.text).unwrap();
    assert_eq!(
        excerpt.block_ends.last(),
        Some(&audit.content_units.as_ref().unwrap().len())
    );
}

#[test]
fn ample_budget_preserves_parser_bytes_offsets_and_structural_evidence() {
    let mut builder = AuditTextBuilder::new(2000, "xlsx");
    builder.push("# 工作表", ContentUnitKind::Heading, vec![]);
    builder.push("# 费用", ContentUnitKind::Heading, vec![]);
    builder.push("项目 | 金额", ContentUnitKind::TableHeader, vec![]);
    builder.push("预留 | 0", ContentUnitKind::Placeholder, vec![]);
    builder.push("采购 | 66.80", ContentUnitKind::TableRow, vec![]);
    builder.push("预留 | 0", ContentUnitKind::Placeholder, vec![]);
    let file = file_from_builder("complete.md", builder);
    let excerpts = allocate_excerpts(&[&file], file.content.chars().count());
    let excerpt = &excerpts[0];
    assert_eq!(excerpt.text, file.content);
    assert_eq!(excerpt.audit, file.evidence_audit);
    assert!(!excerpt.truncated);
    assert_eq!(excerpt.block_ends, [6]);
    assert_eq!(excerpt.block_context, [Vec::<usize>::new()]);
}

#[test]
fn a_new_unit_then_a_new_header_do_not_inherit_the_previous_tables_columns() {
    let mut builder = AuditTextBuilder::new(2000, "xlsx");
    for (text, kind) in [
        ("# 当日", ContentUnitKind::Heading),
        ("供应商 | 金额", ContentUnitKind::TableHeader),
        ("单位：元", ContentUnitKind::Unit),
        ("甲 | 20.20", ContentUnitKind::TableRow),
        ("单位：小时", ContentUnitKind::Unit),
        ("人员 | 工时", ContentUnitKind::TableHeader),
        ("乙 | 8", ContentUnitKind::TableRow),
    ] {
        builder.push(text, kind, vec![]);
    }
    let file = file_from_builder("contexts.md", builder);
    let excerpts = allocate_excerpts(&[&file], file.content.chars().count());
    assert_eq!(excerpts[0].block_ends, [4, 7]);
    assert_eq!(excerpts[0].block_context, [vec![], vec![0]]);
}

#[test]
fn full_long_paragraph_has_exact_independently_selectable_sentences() {
    let paragraph = format!(
        "{}。  审核金额 1234.56 万元。\r\n下一步核对𠀀账户。",
        "资料已登记".repeat(120)
    );
    for origin in [
        SourceRange::new(
            SourceRangeUnit::TextChar,
            75,
            75 + paragraph.chars().count() as u64,
            None,
        ),
        SourceRange::new(SourceRangeUnit::Paragraph, 7, 8, Some("第七段".into())),
    ] {
        let mut b = AuditTextBuilder::new(10_000, "text");
        b.push("# 原始说明", ContentUnitKind::Heading, vec![]);
        b.read(origin.clone());
        b.push(&paragraph, ContentUnitKind::Paragraph, vec![origin.clone()]);
        let file = file_from_builder("report.md", b);
        let result = allocate_excerpts(&[&file], 10_000);
        let e = &result[0];
        assert_eq!(e.text.as_bytes(), file.content.as_bytes());
        assert!(!e.truncated);
        let audit = e.audit.as_ref().unwrap();
        audit.validate(&e.text).unwrap();
        assert_eq!(
            audit.read_ranges.as_deref(),
            Some([origin.clone()].as_slice())
        );
        let units = audit.content_units.as_ref().unwrap();
        assert_eq!(e.block_ends, [2, 3, 4]);
        assert_eq!(e.block_context, [vec![], vec![0], vec![0]]);
        let chars: Vec<_> = e.text.chars().collect();
        let mut start = 0;
        let mut frozen = String::new();
        let mut narrow = Vec::new();
        for &end in &e.block_ends {
            let next = units
                .get(end)
                .map_or(chars.len(), |u| u.text_start as usize);
            let quote: String = chars[start..next].iter().collect();
            if quote.chars().count() + "# 原始说明".chars().count() <= 32 {
                narrow.push(quote.clone());
            }
            frozen += &quote;
            start = next;
        }
        assert_eq!(frozen, file.content);
        assert_eq!(
            narrow,
            ["  审核金额 1234.56 万元。\r\n", "下一步核对𠀀账户。"]
        );
        for u in &units[1..] {
            let r = &u.ranges[0];
            if origin.unit == SourceRangeUnit::TextChar {
                assert_eq!(r.start, origin.start + u.text_start - units[1].text_start);
                assert_eq!(r.end, origin.start + u.text_end - units[1].text_start);
            } else {
                assert_eq!(r, &origin);
            }
        }
    }
}

#[test]
fn full_sentence_mapping_stays_bounded_without_losing_any_original_characters() {
    let mut b = AuditTextBuilder::new(30_000, "text");
    let text: String = (0..1500).map(|i| format!("第{i}项。 ")).collect();
    let r = SourceRange::new(SourceRangeUnit::Paragraph, 21, 22, Some("材料".repeat(220)));
    b.read(r.clone());
    b.push(&text, ContentUnitKind::Paragraph, vec![r]);
    let f = file_from_builder("many.md", b);
    let e = allocate_excerpts(&[&f], 30_000).pop().unwrap();
    assert_eq!(e.text.as_bytes(), f.content.as_bytes());
    let audit = e.audit.as_ref().unwrap();
    audit.validate(&e.text).unwrap();
    let units = audit.content_units.as_ref().unwrap();
    assert!(units.len() > 1 && units.len() <= 512);
    assert_eq!(e.block_ends.last(), Some(&units.len()));
    assert!(units.windows(2).all(|w| w[0].text_end == w[1].text_start));
    assert_eq!(
        (units[0].text_start, units.last().unwrap().text_end),
        (0, e.text.chars().count() as u64)
    );
}

#[test]
fn multiline_csv_and_native_table_rows_stay_atomic_at_full_budget() {
    let row = format!("{}。\n下一步核对 -123.45 万元。", "跨行单元格".repeat(120));
    for (name, kind, range) in [
        (
            "sheet.xlsx",
            ContentUnitKind::TableRow,
            SourceRangeUnit::SheetRow,
        ),
        ("raw.csv", ContentUnitKind::Text, SourceRangeUnit::Paragraph),
    ] {
        let mut b = AuditTextBuilder::new(4000, "text");
        b.push(&row, kind, vec![SourceRange::new(range, 1, 2, None)]);
        let mut file = file_from_builder(name, b);
        file.extension = format!(".{}", name.split_once('.').unwrap().1);
        let e = allocate_excerpts(&[&file], 4000).pop().unwrap();
        assert_eq!(e.block_ends, [1]);
        assert_eq!(e.text, row);
        assert_eq!(e.audit, file.evidence_audit);
    }
}

#[test]
fn a_changed_table_header_only_reuses_units_declared_for_the_new_table() {
    for explicit_unit in [false, true] {
        let mut b = AuditTextBuilder::new(4000, "xlsx");
        let mut push = |n, text: &str, kind| {
            let r = SourceRange::new(SourceRangeUnit::SheetRow, n, n + 1, Some("表单".into()));
            b.read(r.clone());
            b.push(text, kind, vec![r]);
        };
        push(1, "# 同一张工作表", ContentUnitKind::Heading);
        push(2, "单位：万元", ContentUnitKind::Unit);
        push(3, "| 公司 | 金额 |", ContentUnitKind::TableHeader);
        push(4, "| 甲 | 1234 |", ContentUnitKind::TableRow);
        if explicit_unit {
            push(5, "单位：小时", ContentUnitKind::Unit);
        }
        push(6, "| 人员 | 工时(小时) |", ContentUnitKind::TableHeader);
        push(
            7,
            &format!("| 派工{} | 3 |", "描述".repeat(300)),
            ContentUnitKind::TableRow,
        );
        push(8, "| 合计 | 8 |", ContentUnitKind::Total);
        let f = file_from_builder("hours.md", b);
        let e = allocate_excerpts(&[&f], 4000).pop().unwrap();
        let units = e.audit.as_ref().unwrap().content_units.as_ref().unwrap();
        let last = e.block_context.last().unwrap();
        let parents: Vec<_> = last.iter().map(|&i| units[i].ranges[0].start).collect();
        assert_eq!(
            parents,
            if explicit_unit {
                vec![1, 5, 6]
            } else {
                vec![1, 6]
            }
        );
        let expected = if explicit_unit {
            "# 同一张工作表\n单位：小时\n| 人员 | 工时(小时) |\n| 合计 | 8 |"
        } else {
            "# 同一张工作表\n| 人员 | 工时(小时) |\n| 合计 | 8 |"
        };
        let limited = allocate_excerpts(&[&f], expected.chars().count())
            .pop()
            .unwrap();
        assert_eq!(limited.text, expected);
        assert!(!limited
            .ranges
            .as_ref()
            .unwrap()
            .iter()
            .any(|r| r.start == 2));
    }
}
