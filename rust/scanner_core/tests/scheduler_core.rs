//! Budget model part of the scheduler core: nominal rank + ContextBudgetModel +
//! two-phase admission plans (spec Part 1). Task 2 of Plan 2.

use ai_daily_scanner_core::admission::{
    ClassificationPlan, ContentAdmissionPlan, NotParsedReason, PdfClassificationPlan,
    PdfClassificationResult, PlanAction, PlanCandidate, RejectReason,
};
use ai_daily_scanner_core::budget_model::{
    count_chars, max_omitted_row_chars, omitted_summary_reservation, BudgetError,
    ContextBudgetModel, OmittedCandidate, OmittedSummaryPlan, RouteHint, RouteKind,
    SECTION_SEPARATOR_CHARS,
};
use ai_daily_scanner_core::compressor::build_context;
use ai_daily_scanner_core::config::normalize_scanner_profile_v2;
use ai_daily_scanner_core::nominal::nominal_rank;
use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheStatus, ContextAction, ContextProfile, ContextProfileV2, Diagnostic,
    DiagnosticStage, ErrorCode, Nullable, ParseStatus, PdfClassificationStatus, RawScannerProfileV2,
    ReportMode, ScannerProfile, SourceGuardKind,
};

fn v2_profile(mode: ReportMode) -> ai_daily_scanner_contract::NormalizedScannerProfileV2 {
    let raw: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
        "schema_version": "scanner_profile_v2"
    }))
    .expect("minimal v2 raw profile");
    normalize_scanner_profile_v2(&ScannerProfile::V2(raw), mode).expect("normalized v2 profile")
}

fn candidate(path: &str, extension: &str, size_bytes: u64) -> PlanCandidate {
    PlanCandidate {
        file_identity: format!("fixture:{path}"),
        relative_path: path.to_string(),
        extension: extension.to_string(),
        size_bytes,
        source_guard_kind: SourceGuardKind::WindowsFileIdChangeTimeV1,
    }
}

fn context_profile(global_max_chars: u64, per_file_max_chars: u64) -> ContextProfileV2 {
    ContextProfileV2 {
        profile_name: "daily_balanced_v1".to_string(),
        global_max_chars,
        per_file_max_chars,
        small_file_max_bytes: 65_536,
        medium_file_max_bytes: 1_048_576,
        large_file_max_bytes: 10_485_760,
        priority_policy_version: "budget_nominal_v2".to_string(),
        compression_policy_version: "markdown_context_v2".to_string(),
    }
}

// ---------------------------------------------------------------------------
// nominal rank (spec Part 1.1)
// ---------------------------------------------------------------------------

#[test]
fn nominal_rank_puts_error_before_text_but_order_is_parse_independent() {
    // office/pdf 优先级 20 < 文本 30：位置不依赖解析结果
    let office = nominal_rank(r"\A\b.xlsx", ".xlsx");
    let text = nominal_rank(r"\A\b.md", ".md");
    assert!(office.0 < text.0);
    // 同 priority 用 lower path -> path -> identity 稳定 tie-break
    let a = nominal_rank(r"\B\a.md", ".md");
    let b = nominal_rank(r"\B\b.md", ".md");
    assert!(a.1 < b.1);
}

#[test]
fn nominal_rank_table_and_normalization_are_frozen() {
    assert_eq!(nominal_rank(r"\a\.pytest_cache\keep.md", ".md").0, 70);
    assert_eq!(nominal_rank(r"\a\data\benchmarks\keep.xlsx", ".xlsx").0, 70);
    assert_eq!(nominal_rank(r"\a\logs\app.log", ".log").0, 60);
    for extension in [".doc", ".docx", ".pdf", ".ppt", ".pptx", ".xls", ".xlsm", ".xlsx"] {
        assert_eq!(
            nominal_rank(&format!(r"\A\b{extension}"), extension).0,
            20,
            "office/pdf extension {extension}"
        );
    }
    for extension in [".md", ".txt"] {
        assert_eq!(
            nominal_rank(&format!(r"\A\b{extension}"), extension).0,
            30,
            "text extension {extension}"
        );
    }
    for extension in [".csv", ".json", ".log"] {
        assert_eq!(
            nominal_rank(&format!(r"\A\b{extension}"), extension).0,
            50,
            "other extension {extension}"
        );
    }
    // `/` is normalized to `\` for the priority path_key, but the sort-key
    // tie-break element is literally `relative_path.to_lowercase()`.
    assert_eq!(nominal_rank("a/b/报告.md", ".md").1, "a/b/报告.md");
    assert_eq!(nominal_rank("\\a\\B.md", ".md").1, "\\a\\b.md");
    // case-insensitive path tie-break keeps the same lower path
    let upper = nominal_rank(r"\B\A.md", ".md");
    let lower = nominal_rank(r"\B\a.md", ".md");
    assert_eq!(upper.1, lower.1);
}

// ---------------------------------------------------------------------------
// omitted summary reservation and plan (spec Part 1.3)
// ---------------------------------------------------------------------------

#[test]
fn omitted_summary_reservation_is_capped_and_percentage_based() {
    assert_eq!(omitted_summary_reservation(50_000), 10_000);
    assert_eq!(omitted_summary_reservation(60_000), 12_000);
    assert_eq!(omitted_summary_reservation(100_000), 12_000);
    assert_eq!(omitted_summary_reservation(1_000), 200);
    assert_eq!(omitted_summary_reservation(9), 1);
}

#[test]
fn omitted_summary_plan_pre_selects_detail_slots_within_reservation() {
    let files: Vec<OmittedCandidate> = (0..200)
        .map(|index| OmittedCandidate {
            file_identity: format!("fixture:file{index}"),
            relative_path: format!(r"\data\file{index:03}.md"),
            extension: ".md".to_string(),
        })
        .collect();
    let plan = OmittedSummaryPlan::build(&files, 50_000);
    assert_eq!(plan.reservation, 10_000);
    assert!(!plan.detail_slots.is_empty());
    assert!(plan.detail_slots.len() < files.len());

    // Worst case: header + count + catch-all + every pre-selected detail row
    // (rendered at max allowed reason length) must stay inside the reservation.
    let header = format!(
        "## 省略文件摘要\n- 省略文件数: {}\n",
        "9".repeat(20)
    );
    let catch_all = format!("- 其他 | action=omit | count={}\n", "9".repeat(20));
    let mut used = count_chars(&header) + count_chars(&catch_all);
    for slot in &plan.detail_slots {
        used += max_omitted_row_chars(&slot.relative_path) + 1;
    }
    assert!(used <= plan.reservation, "omitted summary {used} > reservation {}", plan.reservation);
}

#[test]
fn omitted_summary_plan_respects_nominal_rank_order() {
    let files: Vec<OmittedCandidate> = vec![
        OmittedCandidate {
            file_identity: "fixture:z".to_string(),
            relative_path: r"\z.md".to_string(),
            extension: ".md".to_string(),
        },
        OmittedCandidate {
            file_identity: "fixture:a".to_string(),
            relative_path: r"\a.md".to_string(),
            extension: ".md".to_string(),
        },
        OmittedCandidate {
            file_identity: "fixture:m".to_string(),
            relative_path: r"\m.md".to_string(),
            extension: ".md".to_string(),
        },
    ];
    let plan = OmittedSummaryPlan::build(&files, 50_000);
    let paths: Vec<&str> = plan.detail_slots.iter().map(|s| s.relative_path.as_str()).collect();
    // Nominal rank orders a.md before m.md before z.md regardless of input order.
    assert_eq!(paths, vec![r"\a.md", r"\m.md", r"\z.md"]);
}

// ---------------------------------------------------------------------------
// ContextBudgetModel (spec Part 1.3)
// ---------------------------------------------------------------------------

#[test]
fn context_budget_model_base_chars_includes_omitted_reservation() {
    let model = ContextBudgetModel::new(&context_profile(50_000, 8_000), &["## 文件证据".to_string()])
        .expect("valid budget model");
    let exact = count_chars("## 文件证据") + SECTION_SEPARATOR_CHARS;
    assert_eq!(model.omitted_summary_reservation(), 10_000);
    assert_eq!(model.base_chars(), exact + 10_000);
    // global budget invariant
    assert!(model.admits(0, 1));
    assert!(!model.admits(model.global_max_chars(), 1));
}

#[test]
fn context_budget_model_fixed_sections_over_budget_is_non_retryable() {
    let error = ContextBudgetModel::new(&context_profile(50_000, 8_000), &["x".repeat(49_999)])
        .expect_err("fixed sections over budget must be rejected");
    assert_eq!(error, BudgetError::ContextFixedSectionsOverBudget);
    assert_eq!(error.error_code(), "CONTEXT_FIXED_SECTIONS_OVER_BUDGET");
}

#[test]
fn rendered_over_reserved_is_budget_model_mismatch() {
    let model = ContextBudgetModel::new(&context_profile(50_000, 8_000), &[]).expect("valid model");
    assert_eq!(model.check_rendered_within_reserved(10, 10), Ok(()));
    let error = model
        .check_rendered_within_reserved(11, 10)
        .expect_err("rendered over reserved must mismatch");
    assert_eq!(error, BudgetError::BudgetModelMismatch);
    assert_eq!(error.error_code(), "BUDGET_MODEL_MISMATCH");
}

// Worst-case section renders matching the formats the reserved_delta formula
// prices (spec Part 1.3 covers path/title, action/reason/backend/lane, max body,
// input/output chars, fences/newlines, metadata multi-line, fixed notices).
fn render_success_section(path: &str, backend: &str, lane: &str, body: &str) -> String {
    format!(
        "### {path}\n- action: compress\n- reason: {}\n- parser_backend: {backend}\n- worker_lane: {lane}\n- input_chars: ~{}\n- output_chars: {}\n- 内容已按单文件预算或解析预算截断\n```text\n{body}\n```",
        "r".repeat(64),
        "9".repeat(20),
        "9".repeat(20),
    )
}

fn render_metadata_section(path: &str, backend: &str, lane: &str, extension: &str) -> String {
    format!(
        "### {path}\n- action: metadata_only\n- reason: {}\n- parser_backend: {backend}\n- worker_lane: {lane}\n- file_type: {extension}\n- size_bytes: {}\n- input_chars: ~{}\n- body: omitted_by_metadata_only_policy",
        "r".repeat(64),
        "9".repeat(20),
        "9".repeat(20),
    )
}

fn render_error_section(path: &str) -> String {
    format!(
        "- {path} | reason={} | error={}",
        "r".repeat(64),
        "e".repeat(64)
    )
}

#[test]
fn every_route_reserved_covers_rendered() {
    let routes = [
        RouteKind::LightText,
        RouteKind::RustOffice,
        RouteKind::RustXlsx,
        RouteKind::Pdf,
        RouteKind::PythonOffice,
        RouteKind::PythonSharepointText,
    ];
    let long_path = format!("{}\\file.md", "\\deep\\nested\\".repeat(30));
    let paths = ["\\a.md".to_string(), "\\工作\\目录\\report.md".to_string(), long_path];
    for mode in [ReportMode::Daily, ReportMode::Weekly, ReportMode::Monthly] {
        let profile = v2_profile(mode);
        let model = ContextBudgetModel::new(&profile.context, &[]).expect("valid model");
        for path in &paths {
            for route in routes {
                let hint = RouteHint {
                    relative_path: path.to_string(),
                    extension: ".md".to_string(),
                    backend: route.backend().to_string(),
                    worker_lane: route.worker_lane().to_string(),
                    max_excerpt_chars: route.max_excerpt_chars(&profile),
                };
                let delta = model.reserved_delta(&hint, Some(1_000_000));
                let body_chars =
                    model.per_file_max_chars().min(route.max_excerpt_chars(&profile)) as usize;
                let body = "x".repeat(body_chars);

                let success = render_success_section(path, &hint.backend, &hint.worker_lane, &body);
                assert!(
                    count_chars(&success) + SECTION_SEPARATOR_CHARS <= delta,
                    "{mode:?} route {route:?} success {} > reserved {delta}",
                    count_chars(&success) + SECTION_SEPARATOR_CHARS
                );

                let metadata = render_metadata_section(path, &hint.backend, &hint.worker_lane, ".md");
                assert!(
                    count_chars(&metadata) + SECTION_SEPARATOR_CHARS <= delta,
                    "{mode:?} route {route:?} metadata {} > reserved {delta}",
                    count_chars(&metadata) + SECTION_SEPARATOR_CHARS
                );

                let error = render_error_section(path);
                assert!(
                    count_chars(&error) + SECTION_SEPARATOR_CHARS <= delta,
                    "{mode:?} route {route:?} error {} > reserved {delta}",
                    count_chars(&error) + SECTION_SEPARATOR_CHARS
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ClassificationPlan (spec Part 1.2 stage A)
// ---------------------------------------------------------------------------

#[test]
fn classification_plan_handles_no_io_dispositions_before_candidate_slots() {
    let profile = v2_profile(ReportMode::Daily);
    let files = vec![
        candidate(r"\big.log", ".log", 1024 * 1024 * 1024), // file too large -> policy
        candidate(r"\a.docx", ".docx", 100),                 // office candidate
        candidate(r"\b.pdf", ".pdf", 100),                   // pdf candidate
        candidate(r"\c.md", ".md", 100),                     // text candidate
    ];
    let plan = ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let big = plan.iter().find(|p| p.relative_path == r"\big.log").expect("big.log");
    assert_eq!(
        big.action,
        PlanAction::NotParsed {
            reason: NotParsedReason::FileSizePolicy
        }
    );
    let pdf = plan.iter().find(|p| p.relative_path == r"\b.pdf").expect("b.pdf");
    assert!(matches!(pdf.action, PlanAction::Admit { route: RouteKind::Pdf }));
    assert_eq!(
        pdf.pdf_classification,
        PdfClassificationPlan::Classify { charged_pages: profile.parse.pdf.max_pages }
    );
    let text = plan.iter().find(|p| p.relative_path == r"\c.md").expect("c.md");
    assert_eq!(text.pdf_classification, PdfClassificationPlan::NotPdf);
}

#[test]
fn classification_plan_marks_source_guard_unavailable_as_error_reject() {
    let profile = v2_profile(ReportMode::Daily);
    let mut unavailable = candidate(r"\a.pdf", ".pdf", 100);
    unavailable.source_guard_kind = SourceGuardKind::Unavailable;
    let plan = ClassificationPlan::build(vec![unavailable], &profile, profile.max_total_pdf_classification_pages);
    assert_eq!(
        plan[0].action,
        PlanAction::Reject {
            reason: RejectReason::SourceGuardUnavailable
        }
    );
}

#[test]
fn classification_plan_reserves_full_pdf_max_pages_per_pdf_in_nominal_order() {
    let profile = v2_profile(ReportMode::Daily); // page budget 80, pdf_max_pages 5
    let files: Vec<PlanCandidate> = (0..17)
        .map(|index| candidate(&format!(r"\p{index:02}.pdf"), ".pdf", 100))
        .collect();
    let plan = ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);

    let classified: Vec<_> = plan
        .iter()
        .filter(|p| matches!(p.pdf_classification, PdfClassificationPlan::Classify { .. }))
        .collect();
    assert_eq!(classified.len(), 16, "16 pdfs fit in the 80-page budget");
    let not_classified: Vec<_> = plan
        .iter()
        .filter(|p| p.pdf_classification == PdfClassificationPlan::NotClassifiedByBudget)
        .collect();
    assert_eq!(not_classified.len(), 1);
    assert_eq!(
        not_classified[0].action,
        PlanAction::NotParsed {
            reason: NotParsedReason::PdfClassificationPageQuotaExhausted
        }
    );
    // every classified pdf charges the full pdf_max_pages, cache-independent
    for item in &classified {
        match item.pdf_classification {
            PdfClassificationPlan::Classify { charged_pages } => {
                assert_eq!(charged_pages, profile.parse.pdf.max_pages)
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn classification_plan_assigns_candidate_slots_by_nominal_rank() {
    let mut profile = v2_profile(ReportMode::Daily);
    profile.max_candidate_files = 2;
    let files = vec![
        candidate(r"\z.pdf", ".pdf", 100),
        candidate(r"\a.md", ".md", 100),
        candidate(r"\m.pdf", ".pdf", 100),
        candidate(r"\b.md", ".md", 100),
    ];
    let plan = ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    // Nominal order: priority 20 pdfs first (m.pdf < z.pdf tie-break), then
    // priority 30 text (a.md < b.md). The two candidate slots go to the pdfs.
    let paths: Vec<&str> = plan.iter().map(|p| p.relative_path.as_str()).collect();
    assert_eq!(paths, vec![r"\m.pdf", r"\z.pdf", r"\a.md", r"\b.md"]);
    for pdf in [r"\m.pdf", r"\z.pdf"] {
        let item = plan.iter().find(|p| p.relative_path == pdf).expect(pdf);
        assert!(matches!(item.action, PlanAction::Admit { .. }), "{pdf}");
    }
    for text in [r"\a.md", r"\b.md"] {
        let item = plan.iter().find(|p| p.relative_path == text).expect(text);
        assert_eq!(
            item.action,
            PlanAction::NotParsed {
                reason: NotParsedReason::SemanticFileQuotaExhausted
            },
            "{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// ContentAdmissionPlan (spec Part 1.2 stage B)
// ---------------------------------------------------------------------------

fn classify_map(items: &[(&str, PdfClassificationStatus)]) -> std::collections::BTreeMap<String, PdfClassificationResult> {
    items
        .iter()
        .map(|(identity, status)| {
            (
                format!("fixture:{identity}"),
                PdfClassificationResult {
                    file_identity: format!("fixture:{identity}"),
                    status: *status,
                    page_count: Some(2),
                    result_examined_pages: Some(2),
                    error_code: None,
                },
            )
        })
        .collect()
}

fn admission_context(global_max_chars: u64, per_file_max_chars: u64) -> ContextBudgetModel {
    ContextBudgetModel::new(
        &context_profile(global_max_chars, per_file_max_chars),
        &["## 文件证据".to_string()],
    )
    .expect("valid admission model")
}

#[test]
fn content_admission_continues_to_smaller_files_after_a_budget_omit() {
    // big.xlsx (priority 20, office excerpt 8000) reserves far more than
    // small.md (priority 30, text excerpt 100). The single pass omits the big
    // file on the global budget, then CONTINUES to the smaller file and admits
    // it; there is no backfill into the big file's slot.
    let mut profile = v2_profile(ReportMode::Daily);
    profile.parse.office.document_excerpt_max_chars = 8_000;
    profile.parse.text.excerpt_max_chars = 100;
    let model = admission_context(2_000, 2_000);
    let files = vec![
        candidate(r"\big.xlsx", ".xlsx", 1_000_000),
        candidate(r"\small.md", ".md", 100),
    ];
    let classified =
        ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let decisions =
        ContentAdmissionPlan::build(&classified, &profile, &model, &std::collections::BTreeMap::new());

    let big = decisions.iter().find(|d| d.relative_path == r"\big.xlsx").expect("big");
    assert_eq!(
        big.action,
        PlanAction::NotParsed {
            reason: NotParsedReason::GlobalContextBudgetExceeded
        }
    );
    assert_eq!(big.reserved_chars, 0);
    let small = decisions.iter().find(|d| d.relative_path == r"\small.md").expect("small");
    assert!(matches!(small.action, PlanAction::Admit { route: RouteKind::LightText }));
    assert!(small.reserved_chars > 0);
    // admitted reserved chars are charged against the base + running budget
    let base = model.base_chars();
    assert!(base + small.reserved_chars <= model.global_max_chars());
}

#[test]
fn content_admission_text_pdf_requires_budget_and_extraction_slot() {
    let profile = v2_profile(ReportMode::Daily); // max_pdf_text_extractions 8
    let mut profile = profile;
    profile.max_pdf_text_extractions = 1;
    let model = admission_context(50_000, 8_000);
    let files = vec![
        candidate(r"\one.pdf", ".pdf", 100),
        candidate(r"\two.pdf", ".pdf", 100),
        candidate(r"\three.pdf", ".pdf", 100),
    ];
    let classified = ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let classifications = classify_map(&[
        (r"\one.pdf", PdfClassificationStatus::TextInParseWindow),
        (r"\two.pdf", PdfClassificationStatus::TextInParseWindow),
        (r"\three.pdf", PdfClassificationStatus::TextInParseWindow),
    ]);
    let decisions = ContentAdmissionPlan::build(&classified, &profile, &model, &classifications);

    let one = decisions.iter().find(|d| d.relative_path == r"\one.pdf").expect("one");
    assert!(matches!(one.action, PlanAction::Admit { route: RouteKind::Pdf }));
    let two = decisions.iter().find(|d| d.relative_path == r"\two.pdf").expect("two");
    assert_eq!(
        two.action,
        PlanAction::NotParsed {
            reason: NotParsedReason::PdfTextExtractionQuotaExhausted
        }
    );
    let three = decisions.iter().find(|d| d.relative_path == r"\three.pdf").expect("three");
    assert_eq!(
        three.action,
        PlanAction::NotParsed {
            reason: NotParsedReason::PdfTextExtractionQuotaExhausted
        }
    );
    // cache hit/miss both charge a slot: one admitted consumes the single slot.
    assert_eq!(decisions.iter().filter(|d| matches!(d.action, PlanAction::Admit { .. })).count(), 1);
}

#[test]
fn content_admission_no_text_pdf_is_metadata_only_without_extraction_slot() {
    let profile = v2_profile(ReportMode::Daily);
    let model = admission_context(50_000, 8_000);
    let files = vec![
        candidate(r"\image.pdf", ".pdf", 100),
        candidate(r"\text.pdf", ".pdf", 100),
    ];
    let classified = ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let classifications = classify_map(&[
        (r"\image.pdf", PdfClassificationStatus::NoTextInParseWindow),
        (r"\text.pdf", PdfClassificationStatus::TextInParseWindow),
    ]);
    let decisions = ContentAdmissionPlan::build(&classified, &profile, &model, &classifications);

    let image = decisions.iter().find(|d| d.relative_path == r"\image.pdf").expect("image");
    assert!(matches!(image.action, PlanAction::Admit { route: RouteKind::Pdf }));
    let text = decisions.iter().find(|d| d.relative_path == r"\text.pdf").expect("text");
    assert!(matches!(text.action, PlanAction::Admit { route: RouteKind::Pdf }));
    // 2 pdfs admitted but only 1 extraction slot consumed (image is metadata-only).
    let admit = decisions
        .iter()
        .filter(|d| matches!(d.action, PlanAction::Admit { route: RouteKind::Pdf }))
        .count();
    assert_eq!(admit, 2);
}

#[test]
fn content_admission_classifier_failure_is_not_admitted() {
    let profile = v2_profile(ReportMode::Daily);
    let model = admission_context(50_000, 8_000);
    let files = vec![candidate(r"\broken.pdf", ".pdf", 100)];
    let classified = ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let classifications = classify_map(&[(
        r"\broken.pdf",
        PdfClassificationStatus::Error,
    )]);
    let decisions = ContentAdmissionPlan::build(&classified, &profile, &model, &classifications);
    let broken = decisions.iter().find(|d| d.relative_path == r"\broken.pdf").expect("broken");
    assert!(matches!(
        broken.action,
        PlanAction::ClassifierFailed { status: PdfClassificationStatus::Error }
    ));
    assert_eq!(broken.reserved_chars, 0);
}

// ---------------------------------------------------------------------------
// Compressor shares the budget-model counting function and no longer silently
// omits a successful file when the global budget is exhausted (spec Part 2.2).
// ---------------------------------------------------------------------------

fn v1_profile(global_max_chars: u64, per_file_max_chars: u64) -> ContextProfile {
    ContextProfile {
        profile_name: "daily_balanced_v1".to_string(),
        global_max_chars,
        per_file_max_chars,
        small_file_max_bytes: 65_536,
        medium_file_max_bytes: 1_048_576,
        large_file_max_bytes: 10_485_760,
        priority_policy_version: "default_v1".to_string(),
        compression_policy_version: "markdown_context_v1".to_string(),
    }
}

fn evidence(path: &str, extension: &str, content: &str) -> ai_daily_scanner_core::decision::ContextFileEvidence {
    ai_daily_scanner_core::decision::ContextFileEvidence {
        file_identity: format!("fixture:{path}"),
        absolute_path: format!("C:\\fixture\\{}", path.replace('/', "\\")),
        relative_path: path.replace('/', "\\"),
        extension: extension.to_string(),
        size_bytes: Some(content.len() as u64),
        content: content.to_string(),
        parser_backend: "light_text_v1".to_string(),
        worker_lane: AuditWorkerLane::RustCore,
        cache_status: CacheStatus::Miss,
        parse_status: ParseStatus::Success,
        truncated: false,
        error: None,
    }
}

fn parse_error(path: &str) -> ai_daily_scanner_core::decision::ContextFileEvidence {
    let mut item = evidence(path, ".md", "");
    item.parse_status = ParseStatus::Error;
    item.error = Some(Diagnostic {
        error_code: ErrorCode::ParserFailed,
        message: "synthetic parser failure".to_string(),
        retryable: false,
        stage: DiagnosticStage::Parse,
        file_path: Nullable(Some(item.absolute_path.clone())),
        backend: Nullable(Some(item.parser_backend.clone())),
    });
    item
}

#[test]
fn compressor_success_omit_branch_is_budget_model_mismatch() {
    let result = build_context(
        vec![
            evidence("a.md", ".md", &"A".repeat(260)),
            evidence("b.md", ".md", &"B".repeat(260)),
            evidence("c.md", ".md", &"C".repeat(260)),
        ],
        &v1_profile(1_250, 300),
        ReportMode::Daily,
    );
    let error = result.expect_err("budget overflow must be an internal error, not an Omit");
    assert!(error.contains("BUDGET_MODEL_MISMATCH"), "unexpected error: {error}");
}

#[test]
fn compressor_still_renders_golden_keep_compress_metadata_error() {
    let mut compressed = evidence("notes/large.md", ".md", &"A".repeat(120));
    compressed.size_bytes = Some(100_000);
    let mut metadata = evidence("book.xlsx", ".xlsx", "sensitive body");
    metadata.parser_backend = "rust_xlsx_bounded_v1".to_string();
    metadata.worker_lane = AuditWorkerLane::RustOfficeProcess;
    metadata.size_bytes = Some(10_485_761);

    let result = build_context(
        vec![
            evidence("notes/small.md", ".md", "daily evidence"),
            compressed,
            metadata,
            parse_error("broken.md"),
        ],
        &v1_profile(4_000, 80),
        ReportMode::Daily,
    )
    .expect("golden context");

    let actions: Vec<_> = result
        .decisions
        .iter()
        .map(|record| {
            (
                record.decision.relative_path.as_str(),
                record.decision.action,
            )
        })
        .collect();
    assert_eq!(
        actions,
        vec![
            ("book.xlsx", ContextAction::MetadataOnly),
            ("notes\\large.md", ContextAction::Compress),
            ("notes\\small.md", ContextAction::Keep),
            ("broken.md", ContextAction::Error),
        ]
    );
    assert_eq!(result.included_file_count, 3);
    assert_eq!(result.omitted_file_count, 0);
    assert_eq!(result.error_file_count, 1);
}
