use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceGuardKind {
    WindowsFileIdChangeTimeV1,
    UnixInodeCtimeV1,
    ContentSha256V1,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseCacheStatus {
    Fresh,
    Miss,
    Snapshot,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseTransport {
    Session,
    OneShot,
    RustInProcess,
    Snapshot,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfClassificationStatus {
    TextInParseWindow,
    NoTextInParseWindow,
    NotClassifiedByBudget,
    Unknown,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationCacheStatus {
    Fresh,
    Miss,
    Snapshot,
    NotEligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationTransport {
    Session,
    OneShot,
    Snapshot,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfClassificationAuditV1 {
    /// Exact input, native classifier and normalized settings for an identified open failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_open_failure_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_audit: Option<PdfPageAudit>,
    pub status: PdfClassificationStatus,
    pub page_count: Nullable<u64>,
    pub classification_cache_status: ClassificationCacheStatus,
    pub classification_cache_miss_reason: String,
    pub result_examined_pages: Nullable<u64>,
    pub run_inspected_pages: Nullable<u64>,
    pub nominal_charged_pages: u64,
    pub duration_ms: u64,
    pub transport: ClassificationTransport,
    pub attempt_count: u64,
    pub classifier_build: String,
    pub classifier_profile_hash: String,
}

impl PdfClassificationAuditV1 {
    fn is_image_only(&self) -> bool {
        let (Some(count), Some(audit)) = (self.page_count.0, self.page_audit.as_ref()) else {
            return false;
        };
        self.status == PdfClassificationStatus::NoTextInParseWindow
            && self.result_examined_pages.0 == Some(count)
            && audit.all_pages_are_images(count)
    }
}

fn validate_cache_miss_reason(
    cache: &str,
    is_miss: bool,
    reason: &str,
    identity_change: &str,
) -> Result<(), String> {
    if is_miss {
        if reason != identity_change
            && !matches!(
                reason,
                "new_file" | "source_version_changed" | "entry_absent_or_evicted"
            )
        {
            return Err(format!(
                "{cache} cache miss reason is not in the v2 allowlist"
            ));
        }
    } else if !reason.is_empty() {
        return Err(format!(
            "non-miss {cache} cache must have an empty miss reason"
        ));
    }
    Ok(())
}

impl Validate for PdfClassificationAuditV1 {
    fn validate(&self) -> Result<(), String> {
        if let Some(key) = &self.pdf_open_failure_key {
            require_sha256_hex(key, "pdf_open_failure_key")?;
            if self.status != PdfClassificationStatus::Error
                || self.page_audit.is_some()
                || self.page_count.0.is_some()
                || self.result_examined_pages.0.is_some()
                || self.nominal_charged_pages == 0
            {
                return Err(
                    "PDFium open failure must be a charged error without page counts".into(),
                );
            }
        }
        if let Some(audit) = &self.page_audit {
            audit.validate_counts(self.page_count.0, self.result_examined_pages.0)?;
            require_range(
                self.nominal_charged_pages,
                0,
                10_000,
                "PDF inspection window",
            )?;
            let expected = pdf_sample_pages(
                self.page_count.0.unwrap_or(0),
                self.nominal_charged_pages,
                &audit.policy_version,
            );
            if expected.get(..audit.checked_pages.len()) != Some(audit.checked_pages.as_slice()) {
                return Err("PDF observed pages violate the selected policy".into());
            }
            if let Some(observations) = &audit.observations {
                let text = self.status == PdfClassificationStatus::TextInParseWindow;
                if observations
                    .iter()
                    .enumerate()
                    .any(|(index, page)| page.has_text != (text && index + 1 == observations.len()))
                {
                    return Err("PDF text observations violate its status".into());
                }
            }
        }
        require_sha256_hex(&self.classifier_build, "classifier_build")?;
        require_sha256_hex(&self.classifier_profile_hash, "classifier_profile_hash")?;
        require_range(self.attempt_count, 0, 3, "attempt_count")?;
        let cache = self.classification_cache_status;
        validate_cache_miss_reason(
            "classification",
            cache == ClassificationCacheStatus::Miss,
            &self.classification_cache_miss_reason,
            "classifier_identity_changed",
        )?;
        let started = self.attempt_count > 0
            && matches!(
                self.transport,
                ClassificationTransport::Session | ClassificationTransport::OneShot
            );
        match self.status {
            PdfClassificationStatus::TextInParseWindow
            | PdfClassificationStatus::NoTextInParseWindow => {
                validate_text_window(
                    self.page_count.0,
                    self.result_examined_pages.0,
                    self.nominal_charged_pages,
                    self.status == PdfClassificationStatus::TextInParseWindow,
                )?;
                if cache == ClassificationCacheStatus::NotEligible {
                    return Err("text/no-text classification cannot be not_eligible".into());
                }
                if cache == ClassificationCacheStatus::Miss && !started {
                    return Err(
                        "executed classification miss requires session/one_shot and 1..3 attempts"
                            .into(),
                    );
                }
            }
            PdfClassificationStatus::Error
                if self.pdf_open_failure_key.is_some()
                    && cache == ClassificationCacheStatus::Fresh => {}
            PdfClassificationStatus::Unknown | PdfClassificationStatus::Error => {
                if cache != ClassificationCacheStatus::Miss
                    || !started
                    || self.nominal_charged_pages == 0
                {
                    return Err(
                        "unknown/error classification requires an executed miss with a nominal charge"
                            .to_string(),
                    );
                }
                let limit = self
                    .nominal_charged_pages
                    .min(self.page_count.0.unwrap_or(u64::MAX));
                if self
                    .result_examined_pages
                    .0
                    .is_some_and(|pages| pages > limit)
                {
                    return Err("typed failure exceeds the charged page window".into());
                }
            }
            PdfClassificationStatus::NotClassifiedByBudget => {
                if self.page_count.0.is_some()
                    || self.result_examined_pages.0 != Some(0)
                    || self.nominal_charged_pages != 0
                    || cache != ClassificationCacheStatus::NotEligible
                {
                    return Err(
                        "not_classified_by_budget must carry the frozen not_eligible zero-page shape"
                            .to_string(),
                    );
                }
            }
        }
        let idle_transport = match cache {
            ClassificationCacheStatus::Fresh | ClassificationCacheStatus::NotEligible => {
                Some(ClassificationTransport::NotApplicable)
            }
            ClassificationCacheStatus::Snapshot => Some(ClassificationTransport::Snapshot),
            ClassificationCacheStatus::Miss => None,
        };
        if let Some(transport) = idle_transport {
            if self.run_inspected_pages.0 != Some(0)
                || self.duration_ms != 0
                || self.transport != transport
                || self.attempt_count != 0
            {
                return Err(
                    "non-executing classification provenance has nonzero execution fields".into(),
                );
            }
        }
        if let Some(run_pages) = self.run_inspected_pages.0 {
            let max_run_pages = self
                .attempt_count
                .checked_mul(self.nominal_charged_pages)
                .ok_or_else(|| "classification attempt page budget overflows u64".to_string())?;
            if run_pages > max_run_pages {
                return Err(
                    "run inspected pages exceed the started attempt page budget".to_string()
                );
            }
            if self.classification_cache_status == ClassificationCacheStatus::Miss
                && self
                    .result_examined_pages
                    .0
                    .is_some_and(|result_pages| run_pages < result_pages)
            {
                return Err("observable run pages cannot be smaller than result pages".to_string());
            }
        }
        Ok(())
    }
}

/// Validate the file route and its physical-page evidence.
pub(super) fn validate_file_parse_provenance(
    status: ParseStatus,
    backend: &str,
    lane: AuditWorkerLane,
    cache: ParseCacheStatus,
    transport: ParseTransport,
    attempts: u64,
    classification: Option<&PdfClassificationAuditV1>,
) -> Result<(), String> {
    let image_only = || classification.is_some_and(PdfClassificationAuditV1::is_image_only);
    if backend == OCR_BACKEND && (!image_only() || transport == ParseTransport::RustInProcess) {
        return Err("image OCR requires fully observed image-only pages and a PDF worker".into());
    }
    // The shared worker slot can expire before a request is sent. Keep the previous
    // classifier evidence and the diagnostic, without inventing a parser invocation.
    if matches!(status, ParseStatus::Error | ParseStatus::Timeout)
        && backend == "not_parsed"
        && lane == AuditWorkerLane::NotParsed
        && cache == ParseCacheStatus::NotApplicable
        && transport == ParseTransport::NotApplicable
        && attempts == 0
        && image_only()
    {
        return Ok(());
    }
    validate_parse_route(
        status,
        backend,
        lane,
        cache,
        classification.map(|audit| audit.status),
    )
}

pub(super) fn validate_parse_route(
    parse_status: ParseStatus,
    parser_backend: &str,
    worker_lane: AuditWorkerLane,
    parse_cache_status: ParseCacheStatus,
    classification_status: Option<PdfClassificationStatus>,
) -> Result<(), String> {
    let metadata = parser_backend == "pdf_metadata_v2" && worker_lane == AuditWorkerLane::RustCore;
    let not_parsed = parser_backend == "not_parsed" && worker_lane == AuditWorkerLane::NotParsed;
    let ocr = parser_backend == OCR_BACKEND
        && worker_lane == AuditWorkerLane::RustPdfProcessV3
        && classification_status == Some(PdfClassificationStatus::NoTextInParseWindow);
    if ocr
        && !matches!(
            (parse_status, parse_cache_status),
            (
                ParseStatus::Success,
                ParseCacheStatus::Fresh | ParseCacheStatus::Miss | ParseCacheStatus::Snapshot
            ) | (
                ParseStatus::Error | ParseStatus::Timeout,
                ParseCacheStatus::Miss
            )
        )
    {
        return Err("image OCR has inconsistent parse or cache status".into());
    }
    let body_parser = ocr
        || matches!(
            (parser_backend, worker_lane),
            ("light_text_v2", AuditWorkerLane::RustCore)
                | ("rust_pdfium_v3", AuditWorkerLane::RustPdfProcessV3)
                | (
                    "rust_office_oxide_v2"
                        | "rust_xlsx_bounded_v2"
                        | "rust_xls_calamine_v3"
                        | "rust_legacy_office_v3",
                    AuditWorkerLane::RustOfficeProcessV3 | AuditWorkerLane::RustOfficeInProcess
                )
        );
    if !(metadata || not_parsed || body_parser) {
        return Err("parser backend and worker lane are not a frozen v2 route".to_string());
    }

    match classification_status {
        Some(PdfClassificationStatus::NoTextInParseWindow)
            if !(ocr
                || (parse_status == ParseStatus::Success && metadata)
                || (parse_status == ParseStatus::NotParsed && not_parsed)) =>
        {
            return Err(
                "no-text classification requires metadata, image OCR or budget-omitted provenance"
                    .to_string(),
            );
        }
        Some(status @ (PdfClassificationStatus::Unknown | PdfClassificationStatus::Error)) => {
            let status_matches = parse_status == ParseStatus::Error
                || (status == PdfClassificationStatus::Unknown
                    && parse_status == ParseStatus::Timeout);
            if !status_matches
                || !not_parsed
                || parse_cache_status != ParseCacheStatus::NotApplicable
            {
                return Err(
                    "classifier failure must map to not_parsed provenance and Error/Timeout"
                        .to_string(),
                );
            }
        }
        Some(PdfClassificationStatus::NotClassifiedByBudget)
            if parse_status != ParseStatus::NotParsed || !not_parsed =>
        {
            return Err(
                "classification budget exclusion must map to not_parsed provenance".to_string(),
            );
        }
        _ => {}
    }
    if metadata && classification_status != Some(PdfClassificationStatus::NoTextInParseWindow) {
        return Err(
            "pdf_metadata_v2 is only valid for no-text classification provenance".to_string(),
        );
    }

    match parse_cache_status {
        ParseCacheStatus::Fresh | ParseCacheStatus::Miss if !body_parser => {
            Err("fresh/miss parse provenance requires a body parser".to_string())
        }
        ParseCacheStatus::Snapshot => match parse_status {
            ParseStatus::Success if body_parser || metadata => Ok(()),
            ParseStatus::NotParsed if not_parsed => Ok(()),
            _ => Err("snapshot parse status does not match its semantic provenance".to_string()),
        },
        ParseCacheStatus::NotApplicable if parse_status == ParseStatus::Success && !metadata => {
            Err("not_applicable Success requires pdf_metadata_v2/rust_core".to_string())
        }
        ParseCacheStatus::NotApplicable if parse_status != ParseStatus::Success && !not_parsed => {
            Err("not_applicable NotParsed/Error/Timeout requires not_parsed provenance".to_string())
        }
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAuditV2 {
    pub relative_path: String,
    pub file_identity: String,
    pub source_version: String,
    pub source_guard_kind: SourceGuardKind,
    pub source_guard_sha256: Nullable<String>,
    pub parse_status: ParseStatus,
    pub parser_backend: String,
    pub worker_lane: AuditWorkerLane,
    pub parse_cache_status: ParseCacheStatus,
    pub cache_miss_reason: String,
    pub truncated: bool,
    pub content_sha256: String,
    pub parse_duration_ms: u64,
    pub failure_class: String,
    pub fallback_backend: String,
    pub fallback_reason_code: String,
    pub parse_transport: ParseTransport,
    pub parse_attempt_count: u64,
    pub final_diagnostic: Nullable<Diagnostic>,
    pub pdf_classification: Nullable<PdfClassificationAuditV1>,
}

impl Validate for FileAuditV2 {
    fn validate(&self) -> Result<(), String> {
        require_relative_path(&self.relative_path, "file relative_path")?;
        require_non_empty(&self.file_identity, 4096, "file_identity")?;
        require_source_version(&self.source_version, "source_version")?;
        require_non_empty(&self.parser_backend, 4096, "parser_backend")?;
        require_sha256_hex(&self.content_sha256, "content_sha256")?;
        match (
            self.source_guard_kind,
            self.source_guard_sha256.0.as_deref(),
        ) {
            (SourceGuardKind::Unavailable, None) => {}
            (SourceGuardKind::Unavailable, Some(_)) => {
                return Err("unavailable source guard must have a null hash".into());
            }
            (_, Some(hash)) => require_sha256_hex(hash, "source_guard_sha256")?,
            (_, None) => return Err("source guard kind requires a sha256".into()),
        }
        validate_cache_miss_reason(
            "parse",
            self.parse_cache_status == ParseCacheStatus::Miss,
            &self.cache_miss_reason,
            "parser_identity_changed",
        )?;
        require_range(self.parse_attempt_count, 0, 3, "parse_attempt_count")?;
        let idle = |transport| {
            self.parse_transport == transport
                && self.parse_attempt_count == 0
                && self.parse_duration_ms == 0
        };
        let failed = matches!(self.parse_status, ParseStatus::Error | ParseStatus::Timeout);
        match self.parse_cache_status {
            ParseCacheStatus::Fresh
                if self.parse_status != ParseStatus::Success
                    || !idle(ParseTransport::NotApplicable) =>
            {
                return Err(
                    "fresh parse-cache provenance must be Success with zero execution".into(),
                );
            }
            ParseCacheStatus::Snapshot if failed || !idle(ParseTransport::Snapshot) => {
                return Err(
                    "snapshot parse provenance must have zero execution and no Error/Timeout"
                        .into(),
                );
            }
            ParseCacheStatus::Miss
                if self.parse_status == ParseStatus::NotParsed
                    || self.parse_attempt_count == 0
                    || !matches!(
                        self.parse_transport,
                        ParseTransport::Session
                            | ParseTransport::OneShot
                            | ParseTransport::RustInProcess
                    ) =>
            {
                return Err(
                    "parse miss requires a started parser transport and 1..3 attempts".into(),
                );
            }
            ParseCacheStatus::NotApplicable if !idle(ParseTransport::NotApplicable) => {
                return Err("not_applicable parse provenance must have zero execution".into());
            }
            _ => {}
        }
        let diagnostic = self.final_diagnostic.0.as_ref();
        match (failed, diagnostic) {
            (true, Some(diagnostic)) => diagnostic.validate()?,
            (true, None) => {
                return Err("error/timeout file audit requires a final diagnostic".into())
            }
            (false, Some(_)) => {
                return Err(
                    "success/not_parsed file audit must not carry a final diagnostic".into(),
                );
            }
            (false, None) => {}
        }
        if let Some(classification) = &self.pdf_classification.0 {
            classification.validate()?;
            if classification.pdf_open_failure_key.is_some()
                && (self.source_guard_kind == SourceGuardKind::Unavailable
                    || !diagnostic.is_some_and(|diagnostic| {
                        matches!(
                            diagnostic.error_code,
                            ErrorCode::ParserFailed | ErrorCode::PdfProtected
                        ) && !diagnostic.retryable
                            && diagnostic.stage == DiagnosticStage::Parse
                            && diagnostic.backend.0.as_deref() == Some("pdf_classifier")
                    }))
            {
                return Err(
                    "PDFium open failure requires a guarded nonretryable classifier diagnostic"
                        .into(),
                );
            }
        }
        validate_file_parse_provenance(
            self.parse_status,
            &self.parser_backend,
            self.worker_lane,
            self.parse_cache_status,
            self.parse_transport,
            self.parse_attempt_count,
            self.pdf_classification.0.as_ref(),
        )
    }
}
