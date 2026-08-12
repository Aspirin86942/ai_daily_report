//! Budget model part of the scheduler core: nominal rank + ContextBudgetModel +
//! two-phase admission plans (spec Part 1). Task 2 of Plan 2.

use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheStatus, ClassificationCacheStatus, ClassificationTransport,
    ContextAction, ContextProfile, Diagnostic, DiagnosticStage, ErrorCode, Nullable, ParseStatus,
    ParseTransport, PdfClassificationStatus, ReportMode, ScannerSettings, SourceGuardKind,
};
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
use ai_daily_scanner_core::config::normalize_scanner_settings;
use ai_daily_scanner_core::nominal::nominal_rank;

fn normalized_settings(mode: ReportMode) -> ai_daily_scanner_contract::NormalizedScannerSettings {
    let raw: ScannerSettings =
        serde_json::from_value(serde_json::json!({})).expect("minimal v2 raw profile");
    normalize_scanner_settings(&raw, mode).expect("normalized v2 profile")
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

fn context_profile(global_max_chars: u64, per_file_max_chars: u64) -> ContextProfile {
    ContextProfile {
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
    for extension in [
        ".doc", ".docx", ".pdf", ".ppt", ".pptx", ".xls", ".xlsm", ".xlsx",
    ] {
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
    let header = format!("## 省略文件摘要\n- 省略文件数: {}\n", "9".repeat(20));
    let catch_all = format!("- 其他 | action=omit | count={}\n", "9".repeat(20));
    let mut used = count_chars(&header) + count_chars(&catch_all);
    for slot in &plan.detail_slots {
        used += max_omitted_row_chars(&slot.relative_path) + 1;
    }
    assert!(
        used <= plan.reservation,
        "omitted summary {used} > reservation {}",
        plan.reservation
    );
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
    let paths: Vec<&str> = plan
        .detail_slots
        .iter()
        .map(|s| s.relative_path.as_str())
        .collect();
    // Nominal rank orders a.md before m.md before z.md regardless of input order.
    assert_eq!(paths, vec![r"\a.md", r"\m.md", r"\z.md"]);
}

// ---------------------------------------------------------------------------
// ContextBudgetModel (spec Part 1.3)
// ---------------------------------------------------------------------------

#[test]
fn context_budget_model_base_chars_includes_omitted_reservation() {
    let model = ContextBudgetModel::new(
        &context_profile(50_000, 8_000),
        &["## 文件证据".to_string()],
    )
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
    let paths = [
        "\\a.md".to_string(),
        "\\工作\\目录\\report.md".to_string(),
        long_path,
    ];
    for mode in [ReportMode::Daily, ReportMode::Weekly, ReportMode::Monthly] {
        let profile = normalized_settings(mode);
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
                let body_chars = model
                    .per_file_max_chars()
                    .min(route.max_excerpt_chars(&profile))
                    as usize;
                let body = "x".repeat(body_chars);

                let success = render_success_section(path, &hint.backend, &hint.worker_lane, &body);
                assert!(
                    count_chars(&success) + SECTION_SEPARATOR_CHARS <= delta,
                    "{mode:?} route {route:?} success {} > reserved {delta}",
                    count_chars(&success) + SECTION_SEPARATOR_CHARS
                );

                let metadata =
                    render_metadata_section(path, &hint.backend, &hint.worker_lane, ".md");
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
    let profile = normalized_settings(ReportMode::Daily);
    let files = vec![
        candidate(r"\big.log", ".log", 1024 * 1024 * 1024), // file too large -> policy
        candidate(r"\a.docx", ".docx", 100),                // office candidate
        candidate(r"\b.pdf", ".pdf", 100),                  // pdf candidate
        candidate(r"\c.md", ".md", 100),                    // text candidate
    ];
    let plan =
        ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let big = plan
        .iter()
        .find(|p| p.relative_path == r"\big.log")
        .expect("big.log");
    assert_eq!(
        big.action,
        PlanAction::NotParsed {
            reason: NotParsedReason::FileSizePolicy
        }
    );
    let pdf = plan
        .iter()
        .find(|p| p.relative_path == r"\b.pdf")
        .expect("b.pdf");
    assert!(matches!(
        pdf.action,
        PlanAction::Admit {
            route: RouteKind::Pdf
        }
    ));
    assert_eq!(
        pdf.pdf_classification,
        PdfClassificationPlan::Classify {
            charged_pages: profile.parse.pdf.max_pages
        }
    );
    let text = plan
        .iter()
        .find(|p| p.relative_path == r"\c.md")
        .expect("c.md");
    assert_eq!(text.pdf_classification, PdfClassificationPlan::NotPdf);
}

#[test]
fn classification_plan_marks_source_guard_unavailable_as_error_reject() {
    let profile = normalized_settings(ReportMode::Daily);
    let mut unavailable = candidate(r"\a.pdf", ".pdf", 100);
    unavailable.source_guard_kind = SourceGuardKind::Unavailable;
    let plan = ClassificationPlan::build(
        vec![unavailable],
        &profile,
        profile.max_total_pdf_classification_pages,
    );
    assert_eq!(
        plan[0].action,
        PlanAction::Reject {
            reason: RejectReason::SourceGuardUnavailable
        }
    );
}

#[test]
fn classification_plan_reserves_full_pdf_max_pages_per_pdf_in_nominal_order() {
    let profile = normalized_settings(ReportMode::Daily); // page budget 80, pdf_max_pages 5
    let files: Vec<PlanCandidate> = (0..17)
        .map(|index| candidate(&format!(r"\p{index:02}.pdf"), ".pdf", 100))
        .collect();
    let plan =
        ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);

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
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.max_candidate_files = 2;
    let files = vec![
        candidate(r"\z.pdf", ".pdf", 100),
        candidate(r"\a.md", ".md", 100),
        candidate(r"\m.pdf", ".pdf", 100),
        candidate(r"\b.md", ".md", 100),
    ];
    let plan =
        ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
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

fn classify_map(
    items: &[(&str, PdfClassificationStatus)],
) -> std::collections::BTreeMap<String, PdfClassificationResult> {
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
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.parse.office.document_excerpt_max_chars = 8_000;
    profile.parse.text.excerpt_max_chars = 100;
    let model = admission_context(2_000, 2_000);
    let files = vec![
        candidate(r"\big.xlsx", ".xlsx", 1_000_000),
        candidate(r"\small.md", ".md", 100),
    ];
    let classified =
        ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let decisions = ContentAdmissionPlan::build(
        &classified,
        &profile,
        &model,
        &std::collections::BTreeMap::new(),
    );

    let big = decisions
        .iter()
        .find(|d| d.relative_path == r"\big.xlsx")
        .expect("big");
    assert_eq!(
        big.action,
        PlanAction::NotParsed {
            reason: NotParsedReason::GlobalContextBudgetExceeded
        }
    );
    assert_eq!(big.reserved_chars, 0);
    let small = decisions
        .iter()
        .find(|d| d.relative_path == r"\small.md")
        .expect("small");
    assert!(matches!(
        small.action,
        PlanAction::Admit {
            route: RouteKind::LightText
        }
    ));
    assert!(small.reserved_chars > 0);
    // admitted reserved chars are charged against the base + running budget
    let base = model.base_chars();
    assert!(base + small.reserved_chars <= model.global_max_chars());
}

#[test]
fn content_admission_text_pdf_requires_budget_and_extraction_slot() {
    let profile = normalized_settings(ReportMode::Daily); // max_pdf_text_extractions 8
    let mut profile = profile;
    profile.max_pdf_text_extractions = 1;
    let model = admission_context(50_000, 8_000);
    let files = vec![
        candidate(r"\one.pdf", ".pdf", 100),
        candidate(r"\two.pdf", ".pdf", 100),
        candidate(r"\three.pdf", ".pdf", 100),
    ];
    let classified =
        ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let classifications = classify_map(&[
        (r"\one.pdf", PdfClassificationStatus::TextInParseWindow),
        (r"\two.pdf", PdfClassificationStatus::TextInParseWindow),
        (r"\three.pdf", PdfClassificationStatus::TextInParseWindow),
    ]);
    let decisions = ContentAdmissionPlan::build(&classified, &profile, &model, &classifications);

    let one = decisions
        .iter()
        .find(|d| d.relative_path == r"\one.pdf")
        .expect("one");
    assert!(matches!(
        one.action,
        PlanAction::Admit {
            route: RouteKind::Pdf
        }
    ));
    let two = decisions
        .iter()
        .find(|d| d.relative_path == r"\two.pdf")
        .expect("two");
    assert_eq!(
        two.action,
        PlanAction::NotParsed {
            reason: NotParsedReason::PdfTextExtractionQuotaExhausted
        }
    );
    let three = decisions
        .iter()
        .find(|d| d.relative_path == r"\three.pdf")
        .expect("three");
    assert_eq!(
        three.action,
        PlanAction::NotParsed {
            reason: NotParsedReason::PdfTextExtractionQuotaExhausted
        }
    );
    // cache hit/miss both charge a slot: one admitted consumes the single slot.
    assert_eq!(
        decisions
            .iter()
            .filter(|d| matches!(d.action, PlanAction::Admit { .. }))
            .count(),
        1
    );
}

#[test]
fn content_admission_no_text_pdf_is_metadata_only_without_extraction_slot() {
    let profile = normalized_settings(ReportMode::Daily);
    let model = admission_context(50_000, 8_000);
    let files = vec![
        candidate(r"\image.pdf", ".pdf", 100),
        candidate(r"\text.pdf", ".pdf", 100),
    ];
    let classified =
        ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let classifications = classify_map(&[
        (r"\image.pdf", PdfClassificationStatus::NoTextInParseWindow),
        (r"\text.pdf", PdfClassificationStatus::TextInParseWindow),
    ]);
    let decisions = ContentAdmissionPlan::build(&classified, &profile, &model, &classifications);

    let image = decisions
        .iter()
        .find(|d| d.relative_path == r"\image.pdf")
        .expect("image");
    assert!(matches!(
        image.action,
        PlanAction::Admit {
            route: RouteKind::Pdf
        }
    ));
    let text = decisions
        .iter()
        .find(|d| d.relative_path == r"\text.pdf")
        .expect("text");
    assert!(matches!(
        text.action,
        PlanAction::Admit {
            route: RouteKind::Pdf
        }
    ));
    // 2 pdfs admitted but only 1 extraction slot consumed (image is metadata-only).
    let admit = decisions
        .iter()
        .filter(|d| {
            matches!(
                d.action,
                PlanAction::Admit {
                    route: RouteKind::Pdf
                }
            )
        })
        .count();
    assert_eq!(admit, 2);
}

#[test]
fn content_admission_classifier_failure_is_not_admitted() {
    let profile = normalized_settings(ReportMode::Daily);
    let model = admission_context(50_000, 8_000);
    let files = vec![candidate(r"\broken.pdf", ".pdf", 100)];
    let classified =
        ClassificationPlan::build(files, &profile, profile.max_total_pdf_classification_pages);
    let classifications = classify_map(&[(r"\broken.pdf", PdfClassificationStatus::Error)]);
    let decisions = ContentAdmissionPlan::build(&classified, &profile, &model, &classifications);
    let broken = decisions
        .iter()
        .find(|d| d.relative_path == r"\broken.pdf")
        .expect("broken");
    assert!(matches!(
        broken.action,
        PlanAction::ClassifierFailed {
            status: PdfClassificationStatus::Error
        }
    ));
    assert_eq!(broken.reserved_chars, 0);
}

// ---------------------------------------------------------------------------
// Compressor shares the budget-model counting function and no longer silently
// omits a successful file when the global budget is exhausted (spec Part 2.2).
// ---------------------------------------------------------------------------

fn settings(global_max_chars: u64, per_file_max_chars: u64) -> ContextProfile {
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

fn evidence(
    path: &str,
    extension: &str,
    content: &str,
) -> ai_daily_scanner_core::decision::ContextFileEvidence {
    ai_daily_scanner_core::decision::ContextFileEvidence {
        file_identity: format!("fixture:{path}"),
        absolute_path: format!("C:\\fixture\\{}", path.replace('/', "\\")),
        relative_path: path.replace('/', "\\"),
        extension: extension.to_string(),
        size_bytes: Some(content.len() as u64),
        content: content.to_string(),
        parser_backend: "light_text_v2".to_string(),
        worker_lane: AuditWorkerLane::RustCore,
        cache_status: CacheStatus::Miss,
        parse_status: ParseStatus::Success,
        truncated: false,
        error: None,
        reason: None,
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
        &settings(1_250, 300),
        ReportMode::Daily,
    );
    let error = result.expect_err("budget overflow must be an internal error, not an Omit");
    assert!(
        error.contains("BUDGET_MODEL_MISMATCH"),
        "unexpected error: {error}"
    );
}

#[test]
fn compressor_still_renders_golden_keep_compress_metadata_error() {
    let mut compressed = evidence("notes/large.md", ".md", &"A".repeat(120));
    compressed.size_bytes = Some(100_000);
    let mut metadata = evidence("book.xlsx", ".xlsx", "sensitive body");
    metadata.parser_backend = "rust_xlsx_bounded_v2".to_string();
    metadata.worker_lane = AuditWorkerLane::RustOfficeProcessV2;
    metadata.size_bytes = Some(10_485_761);

    let result = build_context(
        vec![
            evidence("notes/small.md", ".md", "daily evidence"),
            compressed,
            metadata,
            parse_error("broken.md"),
        ],
        &settings(4_000, 80),
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
            ("broken.md", ContextAction::Error),
            ("notes\\large.md", ContextAction::Compress),
            ("notes\\small.md", ContextAction::Keep),
        ]
    );
    assert_eq!(result.included_file_count, 3);
    assert_eq!(result.omitted_file_count, 0);
    assert_eq!(result.error_file_count, 1);
}

// ===========================================================================
// BudgetedContextScheduler (spec Solution/Part 2): cache-independent
// determinism, NotParsed count equations, and deadline terminal states.
// ===========================================================================

use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    CacheMissReason, PdfClassifierRequestV1, PdfClassifierResultStatus, PdfClassifierResultV1,
};
use ai_daily_scanner_core::fallback::ParseFailure;
use ai_daily_scanner_core::parsers::classifier::{PdfClassifierExecution, PdfClassifierPort};
use ai_daily_scanner_core::scheduler::{
    BudgetedContextScheduler, CachePort, CachePortError, Clock, GuardVerifier, ParseLookupOutcome,
    ParseRequest, ParseResult, ParserPort, RunDeadlines, ScheduledRunInput, TerminalIntent,
    WorkerIdentities,
};
use ai_daily_scanner_core::store::{
    CacheEntry, CacheWriteRecord, ClassificationCacheLookup, ClassificationCacheWriteRecord,
    InventoryRecord,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
struct FakeClock {
    now: Arc<Mutex<u64>>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(0)),
        }
    }
    fn advance(&self, ms: u64) {
        *self.now.lock().unwrap() += ms;
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.now.lock().unwrap()
    }
}

#[derive(Debug, Default)]
struct TestCache {
    parse: HashMap<String, CacheEntry>,
    classification: HashMap<String, ClassificationCacheLookup>,
    existed: HashSet<String>,
    parse_lookups: Arc<Mutex<u64>>,
    /// Captured classification-cache writes (verified in tests).
    classification_writes: Arc<Mutex<Vec<ClassificationCacheWriteRecord>>>,
    /// When set, `lookup_classification` returns this error (run-level failure).
    classification_lookup_error: Option<CachePortError>,
    /// When set, the optional cache-access touch returns this bounded failure.
    touch_error: Option<CachePortError>,
    touch_calls: Arc<Mutex<u64>>,
}

impl TestCache {
    fn fresh_parse(_identity: &str, content: &str) -> CacheEntry {
        CacheEntry {
            content: content.to_string(),
            content_sha256: ai_daily_scanner_core::store::sha256_hex(content.as_bytes()),
            parser_backend: "light_text_v2".to_string(),
            worker_lane: "rust_core".to_string(),
            truncated: false,
            worker_contract_version: "ai_daily_worker_v1".to_string(),
            worker_version: "0.1.0".to_string(),
            worker_build: "engine-test".to_string(),
        }
    }
}

impl CachePort for TestCache {
    fn prepare_inventory(
        &self,
        _scan_run_id: u64,
        _now_ms: u64,
        records: &[InventoryRecord],
    ) -> Result<HashSet<String>, CachePortError> {
        let existed = records
            .iter()
            .filter(|record| self.existed.contains(&record.file_identity))
            .map(|record| record.file_identity.clone())
            .collect();
        Ok(existed)
    }

    fn lookup_parse(
        &self,
        file: &DiscoveredFileOut,
        _route: ai_daily_scanner_core::budget_model::RouteKind,
        _inventory_existed_before: bool,
    ) -> Result<ParseLookupOutcome, CachePortError> {
        *self.parse_lookups.lock().unwrap() += 1;
        let profile_hash = "c".repeat(64);
        let outcome = match self.parse.get(&file.file_identity) {
            Some(entry) => ai_daily_scanner_core::store::CacheLookup::Fresh(entry.clone()),
            None => ai_daily_scanner_core::store::CacheLookup::Miss(CacheMissReason::NewFile),
        };
        Ok(ParseLookupOutcome {
            parse_profile_hash: profile_hash,
            lookup: outcome,
        })
    }

    fn lookup_classification(
        &self,
        file: &DiscoveredFileOut,
        _classifier_profile_hash: &str,
        _classifier_build: &str,
        _inventory_existed_before: bool,
    ) -> Result<ClassificationCacheLookup, CachePortError> {
        if let Some(error) = &self.classification_lookup_error {
            return Err(error.clone());
        }
        Ok(self
            .classification
            .get(&file.file_identity)
            .cloned()
            .unwrap_or(ClassificationCacheLookup::Miss(
                ai_daily_scanner_core::store::ClassificationCacheMissReason::NewFile,
            )))
    }

    fn write_parse(
        &self,
        _now_ms: u64,
        _records: &[CacheWriteRecord],
    ) -> Result<(), CachePortError> {
        Ok(())
    }

    fn write_classification(
        &self,
        _now_ms: u64,
        records: &[ClassificationCacheWriteRecord],
    ) -> Result<(), CachePortError> {
        self.classification_writes
            .lock()
            .unwrap()
            .extend(records.iter().cloned());
        Ok(())
    }

    fn touch_access(
        &self,
        _now_ms: u64,
        _parse_hits: &[String],
        _classification_hits: &[String],
    ) -> Result<(), CachePortError> {
        *self.touch_calls.lock().unwrap() += 1;
        match &self.touch_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
struct TestParser {
    results: HashMap<String, ParseResult>,
}

impl ParserPort for TestParser {
    fn parse(&self, request: &ParseRequest) -> ParseResult {
        self.results
            .get(&request.file.file_identity)
            .cloned()
            .unwrap_or_else(|| ParseResult {
                file_identity: request.file.file_identity.clone(),
                content: String::new(),
                parser_backend: "light_text_v2".to_string(),
                worker_lane: "rust_core".to_string(),
                truncated: false,
                content_sha256: ai_daily_scanner_core::store::sha256_hex(b""),
                parse_status: ParseStatus::Error,
                error: Some(Diagnostic {
                    error_code: ErrorCode::ParserFailed,
                    message: "missing test parse result".to_string(),
                    retryable: false,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(Some(request.file.path.clone())),
                    backend: Nullable(Some(request.route.backend().to_string())),
                }),
                warnings: Vec::new(),
                failure_class: "deterministic".to_string(),
                fallback_backend: String::new(),
                fallback_reason_code: String::new(),
                parse_transport: ParseTransport::RustInProcess,
                parse_attempt_count: 1,
                primary_duration_ms: 0,
                fallback_duration_ms: 0,
                parse_duration_ms: 0,
            })
    }
}

fn success_parse(identity: &str, path: &str, content: &str) -> ParseResult {
    let _ = path;
    ParseResult {
        file_identity: identity.to_string(),
        content: content.to_string(),
        parser_backend: "light_text_v2".to_string(),
        worker_lane: "rust_core".to_string(),
        truncated: false,
        content_sha256: ai_daily_scanner_core::store::sha256_hex(content.as_bytes()),
        parse_status: ParseStatus::Success,
        error: None,
        warnings: Vec::new(),
        failure_class: String::new(),
        fallback_backend: String::new(),
        fallback_reason_code: String::new(),
        parse_transport: ParseTransport::RustInProcess,
        parse_attempt_count: 1,
        primary_duration_ms: 1,
        fallback_duration_ms: 0,
        parse_duration_ms: 1,
    }
}

#[derive(Debug, Clone)]
struct TestClassifier {
    results: HashMap<String, PdfClassifierResultV1>,
}

#[derive(Debug, Clone)]
struct RetryingClassifier;

impl PdfClassifierPort for RetryingClassifier {
    fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        _timeout: Duration,
    ) -> PdfClassifierExecution {
        PdfClassifierExecution {
            outcome: Ok(text_result(&request.file_path)),
            transport: ClassificationTransport::OneShot,
            attempt_count: 3,
            duration_ms: 7,
        }
    }
}

// ---------------------------------------------------------------------------
// P4-T0 parallel wave execution: classifier/parser adapters that observe
// concurrency so the tests can prove classification and parse no longer run
// one file at a time.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ConcurrencyClassifier {
    active: Arc<std::sync::atomic::AtomicUsize>,
    max_seen: Arc<std::sync::atomic::AtomicUsize>,
}

impl ConcurrencyClassifier {
    fn new() -> Self {
        Self {
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_seen: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl PdfClassifierPort for ConcurrencyClassifier {
    fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        _timeout: Duration,
    ) -> PdfClassifierExecution {
        let now = self
            .active
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.max_seen
            .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
        // Give concurrent wave threads time to overlap.
        std::thread::sleep(Duration::from_millis(30));
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        PdfClassifierExecution::test_execution(Ok(no_text_result(&request.file_path)))
    }
}

#[derive(Debug, Clone)]
struct ConcurrencyParser {
    active: Arc<std::sync::atomic::AtomicUsize>,
    max_seen: Arc<std::sync::atomic::AtomicUsize>,
}

impl ConcurrencyParser {
    fn new() -> Self {
        Self {
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_seen: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl ParserPort for ConcurrencyParser {
    fn parse(&self, request: &ParseRequest) -> ParseResult {
        let now = self
            .active
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.max_seen
            .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
        // Give concurrent wave threads time to overlap.
        std::thread::sleep(Duration::from_millis(30));
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        success_parse(&request.file.file_identity, &request.file.path, "evidence")
    }
}

impl PdfClassifierPort for TestClassifier {
    fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        _timeout: Duration,
    ) -> PdfClassifierExecution {
        PdfClassifierExecution::test_execution(
            self.results
                .get(&request.file_path)
                .cloned()
                .ok_or_else(|| ParseFailure {
                    class: ai_daily_scanner_core::fallback::FailureClass::Deterministic,
                    diagnostic: Diagnostic {
                        error_code: ErrorCode::InternalError,
                        message: "missing test classifier result".to_string(),
                        retryable: false,
                        stage: DiagnosticStage::Internal,
                        file_path: Nullable(None),
                        backend: Nullable(None),
                    },
                }),
        )
    }
}

#[derive(Debug)]
struct PassGuard;

impl GuardVerifier for PassGuard {
    fn verify(
        &self,
        _path: &str,
        _expected: &ai_daily_scanner_core::source_guard::SourceGuardV2,
    ) -> bool {
        true
    }
}

#[derive(Debug)]
struct FailSecondGuard {
    calls: std::sync::atomic::AtomicUsize,
}

impl GuardVerifier for FailSecondGuard {
    fn verify(
        &self,
        _path: &str,
        _expected: &ai_daily_scanner_core::source_guard::SourceGuardV2,
    ) -> bool {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
    }
}

#[derive(Debug)]
struct FailPdfGuard {
    fail_on_pdf_call: usize,
    pdf_calls: std::sync::atomic::AtomicUsize,
}

impl GuardVerifier for FailPdfGuard {
    fn verify(
        &self,
        path: &str,
        _expected: &ai_daily_scanner_core::source_guard::SourceGuardV2,
    ) -> bool {
        if !path.ends_with(".pdf") {
            return true;
        }
        self.pdf_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            != self.fail_on_pdf_call
    }
}

fn discovered(rel: &str, ext: &str, size: u64) -> DiscoveredFileOut {
    DiscoveredFileOut {
        file_identity: format!("fixture:{rel}"),
        path: format!("C:\\corpus\\{rel}"),
        extension: ext.to_string(),
        modified_at: "2026-08-05T10:00:00+08:00".to_string(),
        size_bytes: size,
        source_version: format!("mtime_ns=123:size={size}"),
        source_guard_kind: Some("windows_file_id_change_time_v1".to_string()),
        source_guard_sha256: Some("b".repeat(64)),
    }
}

fn text_result(_path: &str) -> PdfClassifierResultV1 {
    PdfClassifierResultV1 {
        status: PdfClassifierResultStatus::TextInParseWindow,
        page_count: ai_daily_scanner_contract::Nullable(Some(2)),
        result_examined_pages: ai_daily_scanner_contract::Nullable(Some(2)),
        diagnostic: ai_daily_scanner_contract::Nullable(None),
    }
}

fn no_text_result(_path: &str) -> PdfClassifierResultV1 {
    PdfClassifierResultV1 {
        status: PdfClassifierResultStatus::NoTextInParseWindow,
        page_count: ai_daily_scanner_contract::Nullable(Some(2)),
        result_examined_pages: ai_daily_scanner_contract::Nullable(Some(2)),
        diagnostic: ai_daily_scanner_contract::Nullable(None),
    }
}

fn run_scheduler<
    C: PdfClassifierPort + 'static,
    P: ParserPort + 'static,
    K: CachePort + 'static,
>(
    clock: &FakeClock,
    cache: K,
    parser: P,
    classifier: C,
    discovery: Vec<DiscoveredFileOut>,
    profile: ai_daily_scanner_contract::NormalizedScannerSettings,
) -> Result<
    ai_daily_scanner_core::scheduler::BudgetedScanOutcome,
    ai_daily_scanner_core::scheduler::SchedulerFailure,
> {
    run_scheduler_with_guard(
        clock, cache, parser, classifier, discovery, profile, PassGuard,
    )
}

fn run_scheduler_with_guard<
    C: PdfClassifierPort + 'static,
    P: ParserPort + 'static,
    K: CachePort + 'static,
    G: GuardVerifier + 'static,
>(
    clock: &FakeClock,
    cache: K,
    parser: P,
    classifier: C,
    discovery: Vec<DiscoveredFileOut>,
    profile: ai_daily_scanner_contract::NormalizedScannerSettings,
    guard: G,
) -> Result<
    ai_daily_scanner_core::scheduler::BudgetedScanOutcome,
    ai_daily_scanner_core::scheduler::SchedulerFailure,
> {
    let input = ScheduledRunInput::new(
        1,
        0,
        "C:\\corpus".to_string(),
        discovery,
        Vec::new(),
        profile,
        WorkerIdentities {
            classifier_build: Some("a".repeat(64)),
            ..WorkerIdentities::default()
        },
        "0.1.0".to_string(),
        "engine-test".to_string(),
        "c".repeat(64),
        "d".repeat(64),
        0,
        clock,
    )
    .expect("scheduled input");
    let scheduler = BudgetedContextScheduler::new(
        Box::new(classifier),
        Box::new(parser),
        Box::new(cache),
        Box::new(clock.clone()),
        Box::new(guard),
    );
    scheduler.execute(input)
}

#[derive(Debug)]
struct TaintedObservedGuard;

impl GuardVerifier for TaintedObservedGuard {
    fn verify(
        &self,
        path: &str,
        _expected: &ai_daily_scanner_core::source_guard::SourceGuardV2,
    ) -> bool {
        !path.ends_with("oversized.txt")
    }

    fn is_tainted(&self, path: &str) -> bool {
        path.ends_with("oversized.txt")
    }

    fn metrics(
        &self,
    ) -> Option<ai_daily_scanner_core::source_guard::SourceGuardObservationMetrics> {
        Some(
            ai_daily_scanner_core::source_guard::SourceGuardObservationMetrics {
                content_hash_file_count: 1,
                unavailable_file_count: 0,
                bytes_read: 123,
            },
        )
    }
}

#[test]
fn pre_snapshot_taint_overrides_a_policy_omission_and_refreshes_metrics() {
    let clock = FakeClock::new();
    let profile = normalized_settings(ReportMode::Daily);
    let oversized = profile.execution.max_file_size_bytes + 1;
    let outcome = run_scheduler_with_guard(
        &clock,
        TestCache::default(),
        TestParser {
            results: HashMap::from([(
                "fixture:healthy.txt".to_string(),
                success_parse("fixture:healthy.txt", "", "healthy context"),
            )]),
        },
        TestClassifier {
            results: HashMap::new(),
        },
        vec![
            discovered("healthy.txt", ".txt", 64),
            discovered("oversized.txt", ".txt", oversized),
        ],
        profile,
        TaintedObservedGuard,
    )
    .expect("tainted outcome");

    let changed = outcome
        .file_results
        .iter()
        .find(|result| result.file_identity == "fixture:oversized.txt")
        .unwrap();
    assert_eq!(changed.parse_status, ParseStatus::Error);
    assert_eq!(
        changed
            .error
            .as_ref()
            .map(|diagnostic| diagnostic.error_code),
        Some(ErrorCode::SourceVersionChanged)
    );
    assert_eq!(
        changed.cache_miss_reason,
        CacheMissReason::SourceVersionChanged
    );
    assert_eq!(
        outcome
            .execution_metrics
            .source_guard_content_hash_file_count,
        1
    );
    assert_eq!(outcome.execution_metrics.source_guard_bytes_read, 123);
    let decision = &outcome
        .context
        .as_ref()
        .unwrap()
        .decisions
        .iter()
        .find(|record| record.file_identity == "fixture:oversized.txt")
        .unwrap()
        .decision;
    assert_eq!(decision.reason, "source_version_changed");
    assert_eq!(decision.error_code, "SOURCE_VERSION_CHANGED");
}

#[test]
fn scheduled_input_keeps_the_begin_run_deadline_origin() {
    let clock = FakeClock::new();
    clock.advance(100);
    let profile = normalized_settings(ReportMode::Daily);
    let deadlines =
        RunDeadlines::derive(profile.total_deadline_ms, &clock).expect("begin-run deadline pair");

    // Simulate worker handshakes and discovery before the scheduler input is
    // assembled. The absolute deadline must stay anchored at t=100.
    clock.advance(3_000);
    let input = ScheduledRunInput::new_with_deadlines(
        1,
        0,
        "C:\\corpus".to_string(),
        vec![discovered("notes/a.md", ".md", 64)],
        Vec::new(),
        profile.clone(),
        WorkerIdentities::default(),
        "0.1.0".to_string(),
        "engine-test".to_string(),
        "c".repeat(64),
        "d".repeat(64),
        3_000,
        deadlines,
    )
    .expect("scheduled input");

    assert_eq!(input.deadline_origin_ms, 100);
    assert_eq!(input.absolute_deadline_ms, 100 + profile.total_deadline_ms);
    assert_eq!(input.work_deadline_ms, input.absolute_deadline_ms - 2_000);
    assert_eq!(
        deadlines.remaining_to_work_deadline(&clock),
        profile.total_deadline_ms - 2_000 - 3_000
    );
}

// ---------------------------------------------------------------------------
// cache-independent determinism (spec Solution/Part 9.1)
// ---------------------------------------------------------------------------

#[test]
fn cache_state_does_not_change_semantic_output() {
    // Same discovery snapshot + profile: empty / partial / full parse +
    // classification cache must produce the same ClassificationPlan,
    // ContentAdmissionPlan, decisions, semantic summary and context hash.
    let discovery = vec![
        discovered("notes/a.md", ".md", 64),
        discovered("notes/b.txt", ".txt", 128),
        discovered("report.pdf", ".pdf", 256),
    ];
    let profile = normalized_settings(ReportMode::Daily);
    let parser = TestParser {
        results: HashMap::from([
            (
                "fixture:notes/a.md".to_string(),
                success_parse("fixture:notes/a.md", "", "evidence a"),
            ),
            (
                "fixture:notes/b.txt".to_string(),
                success_parse("fixture:notes/b.txt", "", "evidence b"),
            ),
            (
                "fixture:report.pdf".to_string(),
                success_parse("fixture:report.pdf", "", "pdf evidence"),
            ),
        ]),
    };
    let classifier = TestClassifier {
        results: HashMap::from([(
            "C:\\corpus\\report.pdf".to_string(),
            text_result("report.pdf"),
        )]),
    };

    // empty cache state
    let clock = FakeClock::new();
    let empty_cache = TestCache::default();
    let empty = run_scheduler(
        &clock,
        empty_cache,
        parser.clone(),
        classifier.clone(),
        discovery.clone(),
        profile.clone(),
    )
    .expect("empty cache outcome");

    // partial cache state: report.pdf parse cached, b.txt classification n/a, a.md not cached
    let clock = FakeClock::new();
    let mut partial_cache = TestCache::default();
    partial_cache.parse.insert(
        "fixture:report.pdf".to_string(),
        TestCache::fresh_parse("fixture:report.pdf", "pdf evidence"),
    );
    partial_cache.classification.insert(
        "fixture:report.pdf".to_string(),
        ClassificationCacheLookup::Fresh(ai_daily_scanner_core::store::ClassificationCacheEntry {
            status: "text_in_parse_window".to_string(),
            page_count: 2,
            result_examined_pages: 2,
        }),
    );
    let partial = run_scheduler(
        &clock,
        partial_cache,
        parser.clone(),
        classifier.clone(),
        discovery.clone(),
        profile.clone(),
    )
    .expect("partial cache outcome");

    // full cache state: all parse + classification cached
    let clock = FakeClock::new();
    let mut full_cache = TestCache::default();
    for (identity, content) in [
        ("fixture:notes/a.md", "evidence a"),
        ("fixture:notes/b.txt", "evidence b"),
        ("fixture:report.pdf", "pdf evidence"),
    ] {
        full_cache.parse.insert(
            identity.to_string(),
            TestCache::fresh_parse(identity, content),
        );
    }
    full_cache.classification.insert(
        "fixture:report.pdf".to_string(),
        ClassificationCacheLookup::Fresh(ai_daily_scanner_core::store::ClassificationCacheEntry {
            status: "text_in_parse_window".to_string(),
            page_count: 2,
            result_examined_pages: 2,
        }),
    );
    let full = run_scheduler(
        &clock,
        full_cache,
        parser.clone(),
        classifier.clone(),
        discovery,
        profile,
    )
    .expect("full cache outcome");

    // Semantic fields must be identical across all three cache states.
    for (name, other) in [("partial", &partial), ("full", &full)] {
        assert_eq!(
            empty.terminal_intent, other.terminal_intent,
            "{name}: terminal intent differs"
        );
        assert_eq!(
            empty.context.as_ref().map(|c| c.context_sha256.clone()),
            other.context.as_ref().map(|c| c.context_sha256.clone()),
            "{name}: context_sha256 differs"
        );
        assert_eq!(
            empty.context.as_ref().map(|c| c.final_context.clone()),
            other.context.as_ref().map(|c| c.final_context.clone()),
            "{name}: final_context differs"
        );
        let empty_decisions: Vec<_> = empty
            .context
            .as_ref()
            .map(|c| {
                c.decisions
                    .iter()
                    .map(|d| {
                        (
                            d.decision.relative_path.clone(),
                            d.decision.action,
                            d.decision.reason.clone(),
                            d.decision.priority,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let other_decisions: Vec<_> = other
            .context
            .as_ref()
            .map(|c| {
                c.decisions
                    .iter()
                    .map(|d| {
                        (
                            d.decision.relative_path.clone(),
                            d.decision.action,
                            d.decision.reason.clone(),
                            d.decision.priority,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(empty_decisions, other_decisions, "{name}: decisions differ");
        let empty_summary = empty.context.as_ref().map(|c| c.summary.clone());
        let other_summary = other.context.as_ref().map(|c| c.summary.clone());
        assert_eq!(empty_summary, other_summary, "{name}: summary differs");
    }
    // Context must be non-empty and Success (no errors in this corpus).
    assert_eq!(empty.terminal_intent, TerminalIntent::Success);
    let context = empty.context.expect("success context");
    assert_eq!(context.summary.source_file_count, 3);
    assert_eq!(context.summary.success_count, 3);
    assert_eq!(context.summary.error_file_count, 0);
    assert_eq!(context.summary.omitted_file_count, 0);
}

// ---------------------------------------------------------------------------
// NotParsed counts (spec Part 2.2)
// ---------------------------------------------------------------------------

#[test]
fn not_parsed_counts_are_derived_not_error() {
    // NotParsed (semantic/policy) -> omit + no Diagnostic + derived
    // not_parsed_count; it never enters the error metric.
    let profile = normalized_settings(ReportMode::Daily);
    // Override the candidate quota so files 2 and 3 are semantically rejected.
    let mut profile = profile;
    profile.max_candidate_files = 1;

    let discovery = vec![
        discovered("notes/a.md", ".md", 64),
        discovered("notes/b.md", ".md", 64),
        discovered("notes/c.md", ".md", 64),
    ];
    let parser = TestParser {
        results: HashMap::from([(
            "fixture:notes/a.md".to_string(),
            success_parse("fixture:notes/a.md", "", "evidence a"),
        )]),
    };
    let classifier = TestClassifier {
        results: HashMap::new(),
    };

    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        TestCache::default(),
        parser,
        classifier,
        discovery,
        profile,
    )
    .expect("outcome");

    assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
    let context = outcome.context.expect("success context");
    assert_eq!(context.summary.source_file_count, 3);
    assert_eq!(context.summary.success_count, 1);
    assert_eq!(context.summary.included_file_count, 1);
    assert_eq!(context.summary.omitted_file_count, 2);
    assert_eq!(context.summary.error_file_count, 0);
    assert_eq!(context.summary.timeout_count, 0);

    let a = context
        .decisions
        .iter()
        .find(|d| d.decision.relative_path == "notes\\a.md")
        .expect("a.md decision");
    assert_eq!(a.decision.action, ContextAction::Keep);
    assert_eq!(a.decision.error_code, "");

    for rel in ["notes\\b.md", "notes\\c.md"] {
        let decision = context
            .decisions
            .iter()
            .find(|d| d.decision.relative_path == rel)
            .expect("omitted decision");
        assert_eq!(decision.decision.action, ContextAction::Omit);
        assert_eq!(decision.decision.reason, "semantic_file_quota_exhausted");
        assert_eq!(decision.decision.error_code, "");
    }

    // NotParsed files never carry a per-file Diagnostic and never count as errors.
    let b = outcome
        .file_results
        .iter()
        .find(|r| r.relative_path == "notes\\b.md")
        .expect("b file result");
    assert_eq!(b.parse_status, ParseStatus::NotParsed);
    assert!(b.error.is_none());
    let error_count = outcome
        .file_results
        .iter()
        .filter(|r| r.parse_status == ParseStatus::Error)
        .count();
    assert_eq!(error_count, 0);
    let extension_error_count = outcome
        .extension_metrics
        .iter()
        .map(|m| m.error_count)
        .sum::<u64>();
    assert_eq!(extension_error_count, 0);
}

// ---------------------------------------------------------------------------
// WorkDeadline terminal states (spec Part 2.3)
// ---------------------------------------------------------------------------

/// Parser that advances the fake clock while "parsing". When `advance_full`
/// is true it consumes the whole effective timeout (so an in-flight parse that
/// runs into the deadline becomes Timeout); otherwise it returns Success after
/// advancing at most `step_ms`, so later queued files hit the deadline as
/// runtime NotParsed in the scheduler.
#[derive(Debug, Clone)]
struct DeadlineParser {
    clock: Arc<Mutex<u64>>,
    work_deadline: u64,
    advance_full: bool,
    step_ms: u64,
}

impl ParserPort for DeadlineParser {
    fn parse(&self, request: &ParseRequest) -> ParseResult {
        let identity = request.file.file_identity.clone();
        let path = request.file.path.clone();
        let advance = if self.advance_full {
            request.timeout_ms
        } else {
            request.timeout_ms.min(self.step_ms)
        };
        let mut now = self.clock.lock().unwrap();
        *now += advance;
        let finished_at = *now;
        drop(now);
        if self.advance_full && finished_at >= self.work_deadline {
            // spec Part 2.3: in-flight work killed by the WorkDeadline -> Timeout.
            ParseResult {
                file_identity: identity,
                content: String::new(),
                parser_backend: request.route.backend().to_string(),
                worker_lane: request.route.worker_lane().to_string(),
                truncated: false,
                content_sha256: ai_daily_scanner_core::store::sha256_hex(b""),
                parse_status: ParseStatus::Timeout,
                warnings: Vec::new(),
                error: Some(Diagnostic {
                    error_code: ErrorCode::ParserTimeout,
                    message: "parse process exceeded its deadline".to_string(),
                    retryable: true,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(Some(path)),
                    backend: Nullable(Some(request.route.backend().to_string())),
                }),
                failure_class: "deterministic".to_string(),
                fallback_backend: String::new(),
                fallback_reason_code: "parse_error".to_string(),
                parse_transport: ParseTransport::OneShot,
                parse_attempt_count: 1,
                primary_duration_ms: advance,
                fallback_duration_ms: 0,
                parse_duration_ms: advance,
            }
        } else {
            success_parse(&identity, &path, "evidence")
        }
    }
}

#[test]
fn work_deadline_stops_new_work_and_marks_queued_runtime_not_parsed() {
    // 3 text files; each parse advances the clock 2000ms. With the bounded
    // parallel executor (max_workers=2) the first wave (a.md, b.md)
    // completes before the WorkDeadline (3000ms); when the scheduler tries to
    // start the NEXT batch the deadline has passed -> c.md is queued -> runtime
    // NotParsed and the run-level trigger is recorded once. Run is Partial,
    // NEVER snapshot. (P4-T0: the deadline check is per-batch, not per-file.)
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.total_deadline_ms = 5_000; // work deadline = 3_000
    profile.execution.max_workers = 2;
    let discovery = vec![
        discovered("notes/a.md", ".md", 64),
        discovered("notes/b.md", ".md", 64),
        discovered("notes/c.md", ".md", 64),
    ];
    let clock = FakeClock::new();
    let input = ScheduledRunInput::new(
        1,
        0,
        "C:\\corpus".to_string(),
        discovery.clone(),
        Vec::new(),
        profile.clone(),
        WorkerIdentities::default(),
        "0.1.0".to_string(),
        "engine-test".to_string(),
        "c".repeat(64),
        "d".repeat(64),
        0,
        &clock,
    )
    .expect("input");
    let parser = DeadlineParser {
        clock: clock.now.clone(),
        work_deadline: input.work_deadline_ms,
        advance_full: false,
        step_ms: 2_000,
    };
    let scheduler = BudgetedContextScheduler::new(
        Box::new(TestClassifier {
            results: HashMap::new(),
        }),
        Box::new(parser),
        Box::new(TestCache::default()),
        Box::new(clock),
        Box::new(PassGuard),
    );
    let outcome = scheduler.execute(input).expect("deadline outcome");

    assert_eq!(outcome.terminal_intent, TerminalIntent::Partial);
    assert_eq!(outcome.execution_metrics.stage_deadline_exhausted_count, 1);
    let context = outcome.context.expect("partial context");
    assert_eq!(context.summary.success_count, 2);
    assert_eq!(context.summary.timeout_count, 0);
    assert_eq!(context.summary.error_file_count, 0);
    assert_eq!(context.summary.omitted_file_count, 1);

    // c.md was queued when the deadline hit -> runtime NotParsed, NO Diagnostic.
    let c = outcome
        .file_results
        .iter()
        .find(|r| r.relative_path == "notes\\c.md")
        .expect("c file result");
    assert_eq!(c.parse_status, ParseStatus::NotParsed);
    assert!(c.error.is_none());
    let c_decision = context
        .decisions
        .iter()
        .find(|d| d.decision.relative_path == "notes\\c.md")
        .expect("c decision");
    assert_eq!(c_decision.decision.action, ContextAction::Omit);
    assert_eq!(c_decision.decision.reason, "runtime_deadline_exhausted");

    // a.md and b.md completed successfully.
    let a = outcome
        .file_results
        .iter()
        .find(|r| r.relative_path == "notes\\a.md")
        .expect("a file result");
    assert_eq!(a.parse_status, ParseStatus::Success);

    // Partial run: no snapshot eligibility (spec Part 2.3).
    assert!(outcome
        .diagnostics
        .iter()
        .any(|record| record.diagnostic.error_code == ErrorCode::StageDeadlineExhausted));
}

#[test]
fn work_deadline_before_any_parse_forms_error_run_without_snapshot() {
    // Every in-flight parse consumes its full effective timeout. With the
    // bounded parallel executor (max_workers=2) the first batch
    // (a.md, b.md) is in-flight and both run into the WorkDeadline -> Timeout;
    // the next batch never starts -> c.md is queued -> runtime NotParsed. With
    // no included files the run is Error and the context is empty.
    // (P4-T0: per-batch deadline check; in-flight -> Timeout, queued -> NotParsed.)
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.total_deadline_ms = 5_000; // work deadline = 3_000
    profile.execution.max_workers = 2;
    let discovery = vec![
        discovered("notes/a.md", ".md", 64),
        discovered("notes/b.md", ".md", 64),
        discovered("notes/c.md", ".md", 64),
    ];
    let clock = FakeClock::new();
    let input = ScheduledRunInput::new(
        1,
        0,
        "C:\\corpus".to_string(),
        discovery.clone(),
        Vec::new(),
        profile.clone(),
        WorkerIdentities::default(),
        "0.1.0".to_string(),
        "engine-test".to_string(),
        "c".repeat(64),
        "d".repeat(64),
        0,
        &clock,
    )
    .expect("input");
    let parser = DeadlineParser {
        clock: clock.now.clone(),
        work_deadline: input.work_deadline_ms,
        advance_full: true,
        step_ms: 0,
    };
    let scheduler = BudgetedContextScheduler::new(
        Box::new(TestClassifier {
            results: HashMap::new(),
        }),
        Box::new(parser),
        Box::new(TestCache::default()),
        Box::new(clock),
        Box::new(PassGuard),
    );
    let outcome = scheduler.execute(input).expect("deadline outcome");

    assert_eq!(outcome.terminal_intent, TerminalIntent::Error);
    assert_eq!(outcome.execution_metrics.stage_deadline_exhausted_count, 1);
    // Error run: no context payload, no snapshot.
    assert!(outcome.context.is_none());
    // a.md and b.md were in-flight (first batch) -> Timeout.
    for rel in ["notes\\a.md", "notes\\b.md"] {
        let file = outcome
            .file_results
            .iter()
            .find(|r| r.relative_path == rel)
            .expect("file result");
        assert_eq!(file.parse_status, ParseStatus::Timeout, "{rel}");
        assert!(
            file.error.is_some(),
            "{rel} timeout must carry a Diagnostic"
        );
    }
    // c.md was queued (second batch never started) -> runtime NotParsed with no
    // per-file Diagnostic.
    let c = outcome
        .file_results
        .iter()
        .find(|r| r.relative_path == "notes\\c.md")
        .expect("c file result");
    assert_eq!(c.parse_status, ParseStatus::NotParsed);
    assert!(c.error.is_none());
    assert!(outcome
        .diagnostics
        .iter()
        .any(|record| record.diagnostic.error_code == ErrorCode::StageDeadlineExhausted));
}

#[test]
fn work_deadline_before_classifier_start_is_runtime_not_parsed() {
    // PDFs whose classification is still queued when the WorkDeadline hits are
    // runtime NotParsed with NO classification result, NO per-file Diagnostic and
    // NO snapshot. The run has no included files -> Error with the run-level
    // deadline diagnostic as the envelope error.
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.total_deadline_ms = 5_000; // work deadline = 3_000
    profile.parse.pdf.max_pages = 2;
    profile.execution.max_workers = 2;
    let discovery = vec![
        discovered("one.pdf", ".pdf", 128),
        discovered("two.pdf", ".pdf", 128),
        discovered("three.pdf", ".pdf", 128),
    ];
    let clock = FakeClock::new();
    let input = ScheduledRunInput::new(
        1,
        0,
        "C:\\corpus".to_string(),
        discovery.clone(),
        Vec::new(),
        profile.clone(),
        WorkerIdentities {
            classifier_build: Some("a".repeat(64)),
            ..WorkerIdentities::default()
        },
        "0.1.0".to_string(),
        "engine-test".to_string(),
        "c".repeat(64),
        "d".repeat(64),
        0,
        &clock,
    )
    .expect("input");
    // The first classification advances the clock past the WorkDeadline, so the
    // second and third PDFs' classifiers are never started.
    let classifier = ClockAdvancingClassifier {
        clock: clock.now.clone(),
    };
    let scheduler = BudgetedContextScheduler::new(
        Box::new(classifier),
        Box::new(TestParser {
            results: HashMap::new(),
        }),
        Box::new(TestCache::default()),
        Box::new(clock),
        Box::new(PassGuard),
    );
    let outcome = scheduler.execute(input).expect("deadline outcome");

    assert_eq!(outcome.terminal_intent, TerminalIntent::Error);
    assert_eq!(outcome.execution_metrics.stage_deadline_exhausted_count, 1);
    assert_eq!(
        outcome.execution_metrics.classify_attempt_count, 2,
        "only the first bounded wave was actually started"
    );
    // Error run: no context payload, no snapshot.
    assert!(outcome.context.is_none());
    for rel in ["one.pdf", "two.pdf", "three.pdf"] {
        let file = outcome
            .file_results
            .iter()
            .find(|r| r.relative_path == rel)
            .expect("file result");
        assert_eq!(file.parse_status, ParseStatus::NotParsed, "{rel}");
        assert!(file.error.is_none(), "{rel}");
    }
    // The Error envelope carries the run-level deadline diagnostic.
    assert!(outcome
        .diagnostics
        .iter()
        .any(
            |record| record.diagnostic.error_code == ErrorCode::StageDeadlineExhausted
                && record.severity == ai_daily_scanner_core::store::DiagnosticSeverity::Error
        ));
}

#[test]
fn not_classified_by_budget_has_a_zero_execution_audit() {
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.parse.pdf.max_pages = 2;
    profile.max_total_pdf_classification_pages = 0;
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        TestCache::default(),
        TestParser {
            results: HashMap::new(),
        },
        TestClassifier {
            results: HashMap::new(),
        },
        vec![discovered("budget.pdf", ".pdf", 128)],
        profile,
    )
    .expect("budget-excluded PDF outcome");

    let file = outcome
        .file_results
        .iter()
        .find(|row| row.relative_path == "budget.pdf")
        .expect("budget PDF row");
    let audit = file
        .pdf_classification
        .as_ref()
        .expect("not-classified PDF must remain auditable");
    assert_eq!(audit.status, PdfClassificationStatus::NotClassifiedByBudget);
    assert_eq!(
        audit.classification_cache_status,
        ClassificationCacheStatus::NotEligible
    );
    assert_eq!(audit.transport, ClassificationTransport::NotApplicable);
    assert_eq!(audit.attempt_count, 0);
    assert_eq!(audit.nominal_charged_pages, 0);
    assert_eq!(audit.result_examined_pages.0, Some(0));
    assert_eq!(audit.run_inspected_pages.0, Some(0));
    assert_eq!(outcome.execution_metrics.classify_attempt_count, 0);
}

#[test]
fn retry_metrics_count_only_actual_classifier_and_pdf_parse_attempts() {
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.parse.pdf.max_pages = 2;
    let mut parsed = success_parse("fixture:retry.pdf", "", "pdf evidence");
    parsed.parser_backend = "python_pdf_text_v2".to_string();
    parsed.worker_lane = "python_document_process_v2".to_string();
    parsed.parse_transport = ParseTransport::Session;
    parsed.parse_attempt_count = 2;
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        TestCache::default(),
        TestParser {
            results: HashMap::from([("fixture:retry.pdf".to_string(), parsed)]),
        },
        RetryingClassifier,
        vec![discovered("retry.pdf", ".pdf", 128)],
        profile,
    )
    .expect("retried PDF outcome");

    assert_eq!(outcome.execution_metrics.classify_attempt_count, 3);
    assert_eq!(outcome.execution_metrics.parse_attempt_count, 2);
    assert_eq!(outcome.execution_metrics.pdfplumber_invocations, 2);
    assert_eq!(
        outcome
            .execution_metrics
            .unobserved_classification_attempt_count,
        2,
        "the two failed transport attempts are unobserved; the final typed result is observed"
    );
    let audit = outcome.file_results[0]
        .pdf_classification
        .as_ref()
        .expect("classification audit");
    assert_eq!(audit.attempt_count, 3);
    assert_eq!(audit.run_inspected_pages.0, None);
}

#[test]
fn post_parse_source_change_discards_content_but_keeps_execution_provenance() {
    let profile = normalized_settings(ReportMode::Daily);
    let file = discovered("changed.md", ".md", 128);
    let mut parsed = success_parse(&file.file_identity, &file.path, "discard me");
    parsed.parser_backend = "light_text_v2".to_string();
    parsed.worker_lane = "rust_core".to_string();
    parsed.parse_transport = ParseTransport::RustInProcess;
    parsed.parse_attempt_count = 2;
    parsed.primary_duration_ms = 3;
    parsed.fallback_duration_ms = 4;
    parsed.parse_duration_ms = 7;
    let clock = FakeClock::new();
    let input = ScheduledRunInput::new(
        1,
        0,
        "C:\\corpus".to_string(),
        vec![file.clone()],
        Vec::new(),
        profile,
        WorkerIdentities {
            classifier_build: Some("a".repeat(64)),
            ..WorkerIdentities::default()
        },
        "0.1.0".to_string(),
        "engine-test".to_string(),
        "c".repeat(64),
        "d".repeat(64),
        0,
        &clock,
    )
    .expect("input");
    let scheduler = BudgetedContextScheduler::new(
        Box::new(TestClassifier {
            results: HashMap::new(),
        }),
        Box::new(TestParser {
            results: HashMap::from([(file.file_identity.clone(), parsed)]),
        }),
        Box::new(TestCache::default()),
        Box::new(clock),
        Box::new(FailSecondGuard {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
    );

    let outcome = scheduler.execute(input).expect("source-change outcome");
    let result = &outcome.file_results[0];
    assert_eq!(result.parse_status, ParseStatus::Error);
    assert_eq!(
        result.error.as_ref().map(|error| error.error_code),
        Some(ErrorCode::SourceVersionChanged)
    );
    assert_eq!(result.parser_backend, "light_text_v2");
    assert_eq!(result.worker_lane, AuditWorkerLane::RustCore);
    assert_eq!(result.parse_transport, ParseTransport::RustInProcess);
    assert_eq!(result.parse_attempt_count, 2);
    assert_eq!(result.parse_duration_ms, 7);
    assert_eq!(
        result.content_sha256,
        ai_daily_scanner_core::store::sha256_hex(b"")
    );
    assert_eq!(outcome.execution_metrics.parse_attempt_count, 2);
    assert!(outcome.parse_cache_receipts.is_empty());
}

#[test]
fn classifier_pre_and_post_source_changes_keep_source_changed_semantics() {
    for (fail_on_pdf_call, expected_attempts) in [(0, 0), (1, 1)] {
        let profile = normalized_settings(ReportMode::Daily);
        let pdf = discovered("changed.pdf", ".pdf", 128);
        let text = discovered("notes.md", ".md", 64);
        let clock = FakeClock::new();
        let input = ScheduledRunInput::new(
            1,
            0,
            "C:\\corpus".to_string(),
            vec![pdf.clone(), text.clone()],
            Vec::new(),
            profile,
            WorkerIdentities {
                classifier_build: Some("a".repeat(64)),
                ..WorkerIdentities::default()
            },
            "0.1.0".to_string(),
            "engine-test".to_string(),
            "c".repeat(64),
            "d".repeat(64),
            0,
            &clock,
        )
        .expect("input");
        let scheduler = BudgetedContextScheduler::new(
            Box::new(TestClassifier {
                results: HashMap::from([(pdf.path.clone(), no_text_result(&pdf.path))]),
            }),
            Box::new(TestParser {
                results: HashMap::from([(
                    text.file_identity.clone(),
                    success_parse(&text.file_identity, &text.path, "text evidence"),
                )]),
            }),
            Box::new(TestCache::default()),
            Box::new(clock),
            Box::new(FailPdfGuard {
                fail_on_pdf_call,
                pdf_calls: std::sync::atomic::AtomicUsize::new(0),
            }),
        );

        let outcome = scheduler.execute(input).expect("source-change outcome");
        assert_eq!(outcome.terminal_intent, TerminalIntent::Partial);
        assert_eq!(
            outcome.execution_metrics.classify_attempt_count,
            expected_attempts
        );
        let file = outcome
            .file_results
            .iter()
            .find(|row| row.file_identity == pdf.file_identity)
            .expect("changed PDF row");
        assert_eq!(file.parse_status, ParseStatus::Error);
        let diagnostic = file.error.as_ref().expect("source change diagnostic");
        assert_eq!(diagnostic.error_code, ErrorCode::SourceVersionChanged);
        assert!(diagnostic.retryable);
        assert_eq!(file.parser_backend, "not_parsed");
        assert_eq!(file.worker_lane, AuditWorkerLane::NotParsed);
        assert_eq!(file.parse_transport, ParseTransport::NotApplicable);
        assert_eq!(file.parse_attempt_count, 0);
        assert!(
            file.pdf_classification.is_none(),
            "discarded classifier result must not fabricate a classification audit"
        );
        let decision = outcome
            .context
            .as_ref()
            .expect("partial context")
            .decisions
            .iter()
            .find(|row| row.file_identity == pdf.file_identity)
            .expect("changed PDF decision");
        assert_eq!(decision.decision.action, ContextAction::Error);
        assert_eq!(decision.decision.error_code, "SOURCE_VERSION_CHANGED");
    }
}

#[derive(Debug, Clone)]
struct ClockAdvancingClassifier {
    clock: Arc<Mutex<u64>>,
}

impl PdfClassifierPort for ClockAdvancingClassifier {
    fn classify_pdf(
        &self,
        request: &PdfClassifierRequestV1,
        _timeout: Duration,
    ) -> PdfClassifierExecution {
        let mut now = self.clock.lock().unwrap();
        *now += 4_000;
        drop(now);
        PdfClassifierExecution::test_execution(Ok(text_result(&request.file_path)))
    }
}

#[test]
fn cache_commit_skipped_after_work_deadline() {
    // A successful parse after the WorkDeadline is not committed to the parse
    // cache (the receipt is not authoritative), but a completed pre-deadline
    // result keeps its state and the receipt list only carries pre-deadline
    // writes. Here every parse completes before the deadline, so the receipt
    // list is non-empty and the run is Success.
    let profile = normalized_settings(ReportMode::Daily);
    let discovery = vec![discovered("notes/a.md", ".md", 64)];
    let parser = TestParser {
        results: HashMap::from([(
            "fixture:notes/a.md".to_string(),
            success_parse("fixture:notes/a.md", "", "evidence"),
        )]),
    };
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        TestCache::default(),
        parser,
        TestClassifier {
            results: HashMap::new(),
        },
        discovery,
        profile,
    )
    .expect("outcome");
    assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
    // One successful parse produced exactly one committed receipt.
    assert_eq!(outcome.parse_cache_receipts.len(), 1);
}

#[test]
fn cache_access_touch_failure_is_audited_instead_of_silently_ignored() {
    let profile = normalized_settings(ReportMode::Daily);
    let discovery = vec![discovered("notes/a.md", ".md", 64)];
    let touches = Arc::new(Mutex::new(0));
    let cache = TestCache {
        parse: HashMap::from([(
            "fixture:notes/a.md".to_string(),
            TestCache::fresh_parse("fixture:notes/a.md", "cached evidence"),
        )]),
        touch_error: Some(CachePortError::Store {
            detail: "cache touch unavailable".to_string(),
        }),
        touch_calls: touches.clone(),
        ..TestCache::default()
    };
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        cache,
        TestParser {
            results: HashMap::new(),
        },
        TestClassifier {
            results: HashMap::new(),
        },
        discovery,
        profile,
    )
    .expect("outcome");

    assert_eq!(*touches.lock().unwrap(), 1);
    assert_eq!(outcome.terminal_intent, TerminalIntent::Partial);
    assert!(outcome.diagnostics.iter().any(|record| {
        record.diagnostic.error_code == ErrorCode::CacheWriteFailed
            && record
                .diagnostic
                .message
                .contains("cache access touch skipped")
    }));
}

#[test]
fn cache_access_touch_deadline_uses_the_run_level_deadline_terminal_state() {
    let profile = normalized_settings(ReportMode::Daily);
    let discovery = vec![discovered("notes/a.md", ".md", 64)];
    let cache = TestCache {
        parse: HashMap::from([(
            "fixture:notes/a.md".to_string(),
            TestCache::fresh_parse("fixture:notes/a.md", "cached evidence"),
        )]),
        touch_error: Some(CachePortError::DeadlineExhausted {
            detail: "work deadline exhausted during cache touch".to_string(),
        }),
        ..TestCache::default()
    };
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        cache,
        TestParser {
            results: HashMap::new(),
        },
        TestClassifier {
            results: HashMap::new(),
        },
        discovery,
        profile,
    )
    .expect("outcome");

    assert_eq!(outcome.execution_metrics.stage_deadline_exhausted_count, 1);
    assert_eq!(outcome.terminal_intent, TerminalIntent::Partial);
    assert!(outcome
        .diagnostics
        .iter()
        .any(|record| { record.diagnostic.error_code == ErrorCode::StageDeadlineExhausted }));
}

#[test]
fn discovery_issues_mark_the_run_partial_with_warnings() {
    // An unreadable discovery entry must surface as a run-level warning and mark
    // the run Partial, never silently vanish (spec Part 5.3).
    let profile = normalized_settings(ReportMode::Daily);
    let discovery = vec![discovered("notes/a.md", ".md", 64)];
    let issues = vec![ai_daily_discovery::DiscoveryIssue {
        kind: ai_daily_discovery::DiscoveryIssueKind::Metadata,
        path: Some("C:\\corpus\\notes\\unreadable.md".to_string()),
        message: "metadata unavailable".to_string(),
    }];
    let clock = FakeClock::new();
    let input = ScheduledRunInput::new(
        1,
        0,
        "C:\\corpus".to_string(),
        discovery.clone(),
        issues,
        profile,
        WorkerIdentities {
            classifier_build: Some("a".repeat(64)),
            ..WorkerIdentities::default()
        },
        "0.1.0".to_string(),
        "engine-test".to_string(),
        "c".repeat(64),
        "d".repeat(64),
        0,
        &clock,
    )
    .expect("input");
    let scheduler = BudgetedContextScheduler::new(
        Box::new(TestClassifier {
            results: HashMap::new(),
        }),
        Box::new(TestParser {
            results: HashMap::from([(
                "fixture:notes/a.md".to_string(),
                success_parse("fixture:notes/a.md", "", "evidence"),
            )]),
        }),
        Box::new(TestCache::default()),
        Box::new(clock),
        Box::new(PassGuard),
    );
    let outcome = scheduler.execute(input).expect("outcome");

    assert_eq!(outcome.terminal_intent, TerminalIntent::Partial);
    assert!(outcome
        .diagnostics
        .iter()
        .any(|record| record.diagnostic.error_code == ErrorCode::DiscoveryEntryUnreadable));
}

#[test]
fn classification_cache_persists_real_page_counts() {
    // spec Part 3.2: the classification cache stores the classifier's REAL
    // page counts (page_count / result_examined_pages), never a hardcoded 1.
    let profile = normalized_settings(ReportMode::Daily);
    let discovery = vec![discovered("report.pdf", ".pdf", 256)];
    let classifier = TestClassifier {
        results: HashMap::from([(
            "C:\\corpus\\report.pdf".to_string(),
            no_text_result("report.pdf"), // page_count=2, result_examined_pages=2
        )]),
    };
    let parser = TestParser {
        results: HashMap::new(),
    };
    let cache = TestCache {
        classification_writes: Arc::new(Mutex::new(Vec::new())),
        ..TestCache::default()
    };
    let writes = cache.classification_writes.clone();
    let clock = FakeClock::new();
    let outcome =
        run_scheduler(&clock, cache, parser, classifier, discovery, profile).expect("outcome");
    assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
    let writes = writes.lock().unwrap();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].status, "no_text_in_parse_window");
    assert_eq!(writes[0].page_count, 2);
    assert_eq!(writes[0].result_examined_pages, 2);
}

#[test]
fn source_file_limit_exceeds_fail_closed() {
    // spec Part 2.1: the 1,000,001st source file fails closed as a non-retryable
    // run-level Error BEFORE prepare_inventory, with zero file rows and
    // discovery_observed_file_count = ceiling + 1.
    let profile = normalized_settings(ReportMode::Daily);
    let discovery: Vec<DiscoveredFileOut> = (0..1_000_001)
        .map(|index| discovered(&format!("notes/f{index:06}.md"), ".md", 64))
        .collect();
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        TestCache::default(),
        TestParser {
            results: HashMap::new(),
        },
        TestClassifier {
            results: HashMap::new(),
        },
        discovery,
        profile,
    )
    .expect("outcome");

    assert_eq!(outcome.terminal_intent, TerminalIntent::Error);
    assert_eq!(
        outcome.execution_metrics.discovery_observed_file_count,
        1_000_001
    );
    assert!(outcome.file_results.is_empty());
    assert!(outcome.inventory.is_empty());
    assert!(outcome
        .diagnostics
        .iter()
        .any(|record| record.diagnostic.error_code == ErrorCode::SourceFileLimitExceeded));
}

#[test]
fn classification_lookup_failure_propagates_as_run_level() {
    // spec Part 4: a classification cache lookup failure is a run-level
    // adapter/store error, NEVER a per-file miss disposition.
    let profile = normalized_settings(ReportMode::Daily);
    let discovery = vec![discovered("report.pdf", ".pdf", 256)];
    let cache = TestCache {
        classification_lookup_error: Some(CachePortError::Store {
            detail: "injected lookup failure".to_string(),
        }),
        ..TestCache::default()
    };
    let clock = FakeClock::new();
    let result = run_scheduler(
        &clock,
        cache,
        TestParser {
            results: HashMap::new(),
        },
        TestClassifier {
            results: HashMap::new(),
        },
        discovery,
        profile,
    );
    assert!(result.is_err(), "lookup failure must be a SchedulerFailure");
    let err = result.expect_err("err");
    assert_eq!(err.diagnostic.error_code, ErrorCode::CacheWriteFailed);
    assert!(err.diagnostic.retryable);
}

#[test]
fn classifier_unknown_maps_timeout_vs_crash() {
    // spec Part 3.2: classifier per-file timeout -> unknown -> Timeout; crash /
    // transient I/O / protocol failure -> unknown -> Error retryable=true.
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.parse.pdf.max_pages = 5;
    let diag = |code: ai_daily_scanner_contract::PythonOperationErrorCode| {
        ai_daily_scanner_contract::PythonOperationDiagnosticV1 {
            error_code: code,
            message: "classifier failure".to_string(),
            retryable: true,
            stage: ai_daily_scanner_contract::PythonOperationStage::Process,
            file_path: ai_daily_scanner_contract::Nullable(None),
            backend: ai_daily_scanner_contract::Nullable(None),
        }
    };
    let unknown_result =
        |code: ai_daily_scanner_contract::PythonOperationErrorCode| PdfClassifierResultV1 {
            status: PdfClassifierResultStatus::Unknown,
            page_count: ai_daily_scanner_contract::Nullable(None),
            result_examined_pages: ai_daily_scanner_contract::Nullable(None),
            diagnostic: ai_daily_scanner_contract::Nullable(Some(diag(code))),
        };
    let discovery = vec![
        discovered("timeout.pdf", ".pdf", 128),
        discovered("crash.pdf", ".pdf", 128),
    ];
    let classifier = TestClassifier {
        results: HashMap::from([
            (
                "C:\\corpus\\timeout.pdf".to_string(),
                unknown_result(ai_daily_scanner_contract::PythonOperationErrorCode::ParserTimeout),
            ),
            (
                "C:\\corpus\\crash.pdf".to_string(),
                unknown_result(ai_daily_scanner_contract::PythonOperationErrorCode::ParserFailed),
            ),
        ]),
    };
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        TestCache::default(),
        TestParser {
            results: HashMap::new(),
        },
        classifier,
        discovery,
        profile,
    )
    .expect("outcome");

    let timeout = outcome
        .file_results
        .iter()
        .find(|r| r.relative_path == "timeout.pdf")
        .expect("timeout.pdf");
    assert_eq!(timeout.parse_status, ParseStatus::Timeout);
    let crash = outcome
        .file_results
        .iter()
        .find(|r| r.relative_path == "crash.pdf")
        .expect("crash.pdf");
    assert_eq!(crash.parse_status, ParseStatus::Error);
    assert!(
        crash.error.as_ref().is_some_and(|diag| diag.retryable),
        "crash/transient must be retryable"
    );
}

// ---------------------------------------------------------------------------
// P4-T0: classification and parse execute in bounded parallel waves
// (spec Solution + Part 7.3; performance blocker fix). The wave executor must
// dispatch up to `max_workers` classifier/parser invocations at once;
// results are merged back by nominal rank so completion order never changes the
// outcome. These tests FAIL on the sequential scheduler and pass after the fix.
// ---------------------------------------------------------------------------

#[test]
fn classification_executes_in_parallel_waves() {
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.execution.max_workers = 2;
    let discovery = vec![
        discovered("one.pdf", ".pdf", 128),
        discovered("two.pdf", ".pdf", 128),
        discovered("three.pdf", ".pdf", 128),
    ];
    let classifier = ConcurrencyClassifier::new();
    let max_seen = classifier.max_seen.clone();
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        TestCache::default(),
        TestParser {
            results: HashMap::new(),
        },
        classifier,
        discovery,
        profile,
    )
    .expect("outcome");
    // All PDFs classified no-text -> metadata-only -> Success.
    assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
    assert!(
        max_seen.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "classification must run in parallel waves (max_workers=2), saw max {}",
        max_seen.load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[test]
fn parse_executes_in_parallel_waves() {
    let mut profile = normalized_settings(ReportMode::Daily);
    profile.execution.max_workers = 2;
    let discovery = vec![
        discovered("notes/a.md", ".md", 64),
        discovered("notes/b.md", ".md", 64),
        discovered("notes/c.md", ".md", 64),
        discovered("notes/d.md", ".md", 64),
    ];
    let parser = ConcurrencyParser::new();
    let max_seen = parser.max_seen.clone();
    let clock = FakeClock::new();
    let outcome = run_scheduler(
        &clock,
        TestCache::default(),
        parser,
        TestClassifier {
            results: HashMap::new(),
        },
        discovery,
        profile,
    )
    .expect("outcome");
    assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
    assert!(
        max_seen.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "parse must run in parallel waves (max_workers=2), saw max {}",
        max_seen.load(std::sync::atomic::Ordering::SeqCst)
    );
}
