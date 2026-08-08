use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheStatus, ContextAction, ContextProfile, Diagnostic, DiagnosticStage,
    ErrorCode, Nullable, ParseStatus, ReportMode,
};
use ai_daily_scanner_core::compressor::build_context;
use ai_daily_scanner_core::decision::ContextFileEvidence;

fn profile(global_max_chars: u64, per_file_max_chars: u64) -> ContextProfile {
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

fn evidence(path: &str, extension: &str, content: &str) -> ContextFileEvidence {
    ContextFileEvidence {
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
        reason: None,
    }
}

fn not_parsed(path: &str, extension: &str, reason: &str) -> ContextFileEvidence {
    let mut item = evidence(path, extension, "");
    item.parse_status = ParseStatus::NotParsed;
    item.reason = Some(reason.to_string());
    item
}

fn parse_error(path: &str) -> ContextFileEvidence {
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
fn golden_keep_compress_metadata_only_and_error_actions_are_frozen() {
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
        &profile(4_000, 80),
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
    // spec Part 1.1: an Error file keeps its nominal priority (30 for .md), so
    // broken.md sorts with the other priority-30 text files (broken < notes).
    assert_eq!(
        actions,
        vec![
            ("book.xlsx", ContextAction::MetadataOnly),
            ("broken.md", ContextAction::Error),
            ("notes\\large.md", ContextAction::Compress),
            ("notes\\small.md", ContextAction::Keep),
        ]
    );
    assert!(!result.content.contains("sensitive body"));
    assert!(result.content.contains("daily evidence"));
    // spec Part 1.3: file_context renders only the bounded error code, never
    // the arbitrary Diagnostic message.
    assert!(result.content.contains("PARSER_FAILED"));
    assert!(!result.content.contains("synthetic parser failure"));
    assert_eq!(result.included_file_count, 3);
    assert_eq!(result.omitted_file_count, 0);
    assert_eq!(result.error_file_count, 1);
}

#[test]
fn golden_large_log_uses_the_recent_tail_once() {
    let content = format!("{}RECENT_TAIL", "old-".repeat(80));
    let result = build_context(
        vec![evidence("logs/app.log", ".log", &content)],
        &profile(2_000, 48),
        ReportMode::Daily,
    )
    .expect("golden context");

    assert!(result.content.contains("RECENT_TAIL"));
    assert!(!result.content.contains(&"old-".repeat(20)));
    assert_eq!(result.decisions[0].decision.action, ContextAction::Compress);
    assert_eq!(result.decisions[0].decision.reason, "large_log_tail");
    assert_eq!(result.decisions[0].decision.output_chars, 48);
}

#[test]
fn cache_state_does_not_change_context_bytes() {
    let cold = evidence("notes/cache-stable.md", ".md", "stable evidence");
    let mut warm = cold.clone();
    warm.cache_status = CacheStatus::Fresh;

    let cold_context =
        build_context(vec![cold], &profile(2_000, 100), ReportMode::Daily).expect("cold context");
    let warm_context =
        build_context(vec![warm], &profile(2_000, 100), ReportMode::Daily).expect("warm context");

    assert_eq!(cold_context.content, warm_context.content);
}

#[test]
fn golden_office_pdf_error_priority_and_path_tie_break_are_frozen() {
    let mut office = evidence("zeta/report.docx", ".docx", "office");
    office.parser_backend = "rust_office_oxide_v1".to_string();
    office.worker_lane = AuditWorkerLane::RustOfficeProcess;
    let mut pdf = evidence("alpha/report.pdf", ".pdf", "pdf");
    pdf.parser_backend = "pdf_text_v1".to_string();
    pdf.worker_lane = AuditWorkerLane::PythonDocumentProcess;

    let result = build_context(
        vec![
            evidence("b.md", ".md", "b"),
            parse_error("error.md"),
            office,
            evidence("A.md", ".md", "a"),
            pdf,
        ],
        &profile(4_000, 200),
        ReportMode::Daily,
    )
    .expect("golden context");

    let paths: Vec<_> = result
        .decisions
        .iter()
        .map(|record| record.decision.relative_path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec![
            "alpha\\report.pdf",
            "zeta\\report.docx",
            "A.md",
            "b.md",
            "error.md",
        ]
    );
    // spec Part 1.1/2.2: parse status NEVER changes the position; the error file
    // keeps nominal priority 30 for .md (the legacy priority-80 jump is removed).
    let priorities: Vec<_> = result
        .decisions
        .iter()
        .map(|record| record.decision.priority)
        .collect();
    assert_eq!(priorities, vec![20, 20, 30, 30, 30]);
}

#[test]
fn golden_not_parsed_is_omit_with_reason_and_no_error() {
    // spec Part 2.2: NotParsed (semantic/policy) -> omit + budget reason,
    // no error Diagnostic, derived not_parsed count, nominal priority.
    let result = build_context(
        vec![
            evidence("a.md", ".md", "a"),
            not_parsed("b.md", ".md", "semantic_file_quota_exhausted"),
            not_parsed("c.pdf", ".pdf", "pdf_classification_page_quota_exhausted"),
        ],
        &profile(4_000, 200),
        ReportMode::Daily,
    )
    .expect("golden context");

    let by_path: Vec<_> = result
        .decisions
        .iter()
        .map(|record| {
            (
                record.decision.relative_path.as_str(),
                record.decision.action,
                record.decision.reason.as_str(),
                record.decision.error_code.as_str(),
            )
        })
        .collect();
    assert_eq!(
        by_path,
        vec![
            (
                "c.pdf",
                ContextAction::Omit,
                "pdf_classification_page_quota_exhausted",
                ""
            ),
            ("a.md", ContextAction::Keep, "small_file_keep", ""),
            (
                "b.md",
                ContextAction::Omit,
                "semantic_file_quota_exhausted",
                ""
            ),
        ]
    );
    assert_eq!(result.success_count, 1);
    assert_eq!(result.included_file_count, 1);
    assert_eq!(result.omitted_file_count, 2);
    assert_eq!(result.error_file_count, 0);
    assert_eq!(result.timeout_count, 0);
    // omitted rows appear in the omitted summary
    assert!(result.content.contains("## 省略文件摘要"));
    assert!(result.content.contains("semantic_file_quota_exhausted"));
    assert!(result.content.contains("pdf_classification_page_quota_exhausted"));
}

#[test]
fn golden_global_budget_overflow_is_budget_model_mismatch_not_omit() {
    // spec Part 2.2: 成功后因全局预算改 Omit 的兼容分支已删除；一个已准入文件
    // 在渲染时超过全局预算即 BUDGET_MODEL_MISMATCH 内部错误，不静默 Omit。
    let result = build_context(
        vec![
            evidence("a.md", ".md", &"A".repeat(260)),
            evidence("b.md", ".md", &"B".repeat(260)),
            evidence("c.md", ".md", &"C".repeat(260)),
        ],
        &profile(1_250, 300),
        ReportMode::Daily,
    );
    let error = result.expect_err("budget overflow must fail closed");
    assert!(
        error.contains("BUDGET_MODEL_MISMATCH"),
        "unexpected error: {error}"
    );
}

#[test]
fn golden_truncated_source_and_unreadable_size_remain_auditable() {
    let mut truncated = evidence("truncated.txt", ".txt", "bounded preview");
    truncated.truncated = true;
    let mut unknown_size = evidence("unknown.txt", ".txt", "known content");
    unknown_size.size_bytes = None;

    let result = build_context(
        vec![unknown_size, truncated],
        &profile(2_000, 100),
        ReportMode::Daily,
    )
    .expect("golden context");

    let truncated = result
        .decisions
        .iter()
        .find(|record| record.decision.relative_path == "truncated.txt")
        .expect("truncated decision");
    assert_eq!(truncated.decision.action, ContextAction::Compress);
    assert!(truncated.decision.truncated);

    let unknown = result
        .decisions
        .iter()
        .find(|record| record.decision.relative_path == "unknown.txt")
        .expect("unknown-size decision");
    assert_eq!(unknown.decision.action, ContextAction::Keep);
    assert_eq!(unknown.decision.reason, "small_file_keep");
}
