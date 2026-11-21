use super::*;

pub(super) fn source_version_changed_classification(identity: &str) -> PdfClassificationResult {
    PdfClassificationResult {
        file_identity: identity.to_string(),
        status: PdfClassificationStatus::Error,
        page_count: None,
        result_examined_pages: None,
        error_code: Some("SOURCE_VERSION_CHANGED".to_string()),
    }
}

pub(super) fn classifier_failure_semantics(
    status: &PdfClassificationStatus,
    classification: Option<&PdfClassificationResult>,
) -> (ParseStatus, ErrorCode, bool) {
    let marker = classification.and_then(|result| result.error_code.as_deref());
    match (status, marker) {
        (_, Some("SOURCE_VERSION_CHANGED")) => {
            (ParseStatus::Error, ErrorCode::SourceVersionChanged, true)
        }
        (PdfClassificationStatus::Error, Some("PDF_PROTECTED")) => {
            (ParseStatus::Error, ErrorCode::PdfProtected, false)
        }
        (PdfClassificationStatus::Unknown, Some("PARSER_TIMEOUT")) => {
            (ParseStatus::Timeout, ErrorCode::ParserTimeout, true)
        }
        _ => (
            ParseStatus::Error,
            ErrorCode::ParserFailed,
            *status == PdfClassificationStatus::Unknown,
        ),
    }
}

pub(super) fn source_version_changed_parse(file: &DiscoveredFileOut) -> ParseResult {
    ParseResult {
        evidence_audit: None,
        file_identity: file.file_identity.clone(),
        content: String::new(),
        parser_backend: "not_parsed".to_string(),
        worker_lane: "not_parsed".to_string(),
        truncated: false,
        content_sha256: crate::store::sha256_hex(b""),
        parse_status: ParseStatus::Error,
        error: Some(Diagnostic {
            error_code: ErrorCode::SourceVersionChanged,
            message: "file source version changed before or during parsing".to_string(),
            retryable: true,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some(file.path.clone())),
            backend: Nullable(Some(file.extension.clone())),
        }),
        warnings: Vec::new(),
        failure_class: "deterministic".to_string(),
        fallback_backend: String::new(),
        fallback_reason_code: String::new(),
        parse_transport: ParseTransport::NotApplicable,
        parse_attempt_count: 0,
        primary_duration_ms: 0,
        fallback_duration_ms: 0,
        parse_duration_ms: 0,
    }
}

pub(super) fn source_version_changed_after_parse(
    file: &DiscoveredFileOut,
    executed: ParseResult,
) -> ParseResult {
    debug_assert!(executed.parse_attempt_count > 0);
    ParseResult {
        parser_backend: executed.parser_backend,
        worker_lane: executed.worker_lane,
        warnings: executed.warnings,
        fallback_backend: executed.fallback_backend,
        fallback_reason_code: executed.fallback_reason_code,
        parse_transport: executed.parse_transport,
        parse_attempt_count: executed.parse_attempt_count,
        primary_duration_ms: executed.primary_duration_ms,
        fallback_duration_ms: executed.fallback_duration_ms,
        parse_duration_ms: executed.parse_duration_ms,
        ..source_version_changed_parse(file)
    }
}

pub(super) fn is_source_version_changed_result(result: &ParseResult) -> bool {
    result
        .error
        .as_ref()
        .is_some_and(|diagnostic| diagnostic.error_code == ErrorCode::SourceVersionChanged)
}
