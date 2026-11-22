use super::{
    cache::{CacheKey, CacheQuery, PdfNegative, PdfObservation, PdfWindow},
    pdf::{PdfClient, PdfClientError},
    sha256_hex,
    source_guard::{observe, source_changed, source_sha256, SourceGuard},
    PARSER_CONTENT_IDENTITY,
};
use daily_parsers::{
    contract::{
        ClassifyResult, ClassifyStatus, ParseRequest, ParseResult, ParserBackend, ParserLimits,
        WorkerDiagnostic, OCR_CHARS_PER_FILE, OCR_TIMEOUT_MS, OFFICE_CONTENT_MISSING_MESSAGE,
    },
    discovery::DiscoveredFileOut,
    office,
    pdf_wire::{InspectAndExtract, PdfNegativeKind, PdfSourceGuard},
    settings::{NormalizedScannerSettings, OfficeParseProfile, ParseStatus},
    text::{self, LightTextError},
};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

pub(super) const MAX_FILE_CHARS: u64 = 60_000;
pub(super) const MAX_TOTAL_CHARS: u64 = 180_000;
const PARSE_POLICY: &str = "scan_v4";
pub(super) const PDF: ParserBackend = ParserBackend::RustPdfiumV3;

pub(super) struct Candidate {
    pub file: DiscoveredFileOut,
    pub relative_path: String,
    pub priority: u64,
    pub backend: Option<ParserBackend>,
    pub guard: Option<SourceGuard>,
    pub completed: Option<Completed>,
    pub ocr: Option<Completed>,
}

impl Candidate {
    pub fn result(&self) -> &Completed {
        self.ocr
            .as_ref()
            .or(self.completed.as_ref())
            .expect("completed scan")
    }

    pub fn results(&self) -> impl Iterator<Item = &Completed> {
        self.completed.iter().chain(self.ocr.iter())
    }
}

pub(super) struct Completed {
    pub outcome: Outcome,
    pub query: Option<CacheQuery>,
    pub reused: bool,
    pub attempted: bool,
    pub duration_ms: u64,
    pub pdf: Option<PdfScan>,
}

pub(super) enum Outcome {
    Parsed {
        result: Box<ParseResult>,
        source_sha256: String,
        inspection_only: bool,
    },
    Negative(PdfNegativeKind),
    Omitted(&'static str),
    Failed {
        reason: &'static str,
        status: ParseStatus,
        diagnostic: WorkerDiagnostic,
    },
}

impl Completed {
    pub fn new(outcome: Outcome) -> Self {
        Self {
            outcome,
            query: None,
            reused: false,
            attempted: false,
            duration_ms: 0,
            pdf: None,
        }
    }

    pub fn diagnostic(&self) -> Option<&WorkerDiagnostic> {
        match &self.outcome {
            Outcome::Failed { diagnostic, .. } => Some(diagnostic),
            Outcome::Negative(_) => self.pdf.as_ref()?.classification.diagnostic.as_ref(),
            _ => None,
        }
    }

    pub fn obtained(&self) -> Option<(&ParseResult, &String)> {
        match &self.outcome {
            Outcome::Parsed {
                result,
                source_sha256,
                inspection_only: false,
            } => Some((result, source_sha256)),
            _ => None,
        }
    }
}

impl Outcome {
    fn content(result: ParseResult, inspection_only: bool) -> Self {
        Self::Parsed {
            result: Box::new(result),
            source_sha256: String::new(),
            inspection_only,
        }
    }

    pub fn status(&self) -> ParseStatus {
        match self {
            Self::Parsed {
                inspection_only: false,
                ..
            }
            | Self::Negative(PdfNegativeKind::NoText) => ParseStatus::Success,
            Self::Omitted(_)
            | Self::Parsed {
                inspection_only: true,
                ..
            } => ParseStatus::NotParsed,
            Self::Failed { status, .. } => *status,
            Self::Negative(_) => ParseStatus::Error,
        }
    }

    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Parsed {
                inspection_only, ..
            } => inspection_only.then_some("pdf_text_extraction_quota_exhausted"),
            Self::Negative(PdfNegativeKind::NoText) => Some("pdf_window_no_text"),
            Self::Negative(PdfNegativeKind::InvalidPdf) => Some("pdf_damaged"),
            Self::Negative(PdfNegativeKind::Protected) => Some("pdf_protected"),
            Self::Omitted(reason) | Self::Failed { reason, .. } => Some(reason),
        }
    }
}

pub(super) struct PdfScan {
    pub classification: ClassifyResult,
    pub charged_pages: u64,
    pub extraction_slot: bool,
    pub requests: u64,
    pub document_opens: u64,
    pub page_reads: u64,
}

impl PdfScan {
    pub fn into_observation(self) -> Option<PdfObservation> {
        Some(PdfObservation {
            page_count: self.classification.page_count?,
            pages: self.classification.page_audit?,
        })
    }

    pub fn into_negative(self, kind: PdfNegativeKind) -> Option<PdfNegative> {
        Some(match kind {
            PdfNegativeKind::NoText => PdfNegative::NoText(self.into_observation()?),
            PdfNegativeKind::InvalidPdf => PdfNegative::Damaged(self.classification.diagnostic?),
            PdfNegativeKind::Protected => {
                PdfNegative::PasswordProtected(self.classification.diagnostic?)
            }
        })
    }
}

pub(super) fn backend(
    extension: &str,
    profile: &NormalizedScannerSettings,
) -> Option<ParserBackend> {
    Some(match extension {
        ".txt" | ".md" | ".log" | ".csv" | ".json" => ParserBackend::LightTextV2,
        ".docx" | ".pptx" => ParserBackend::RustOfficeOxideV2,
        ".xlsx" => ParserBackend::RustXlsxBoundedV2,
        ".xls" => ParserBackend::RustXlsCalamineV3,
        ".pdf" => PDF,
        ".doc" | ".ppt" if profile.parse.office.legacy_extensions_enabled => {
            ParserBackend::RustLegacyOfficeV3
        }
        _ => return None,
    })
}

pub(super) fn query(
    file: &Candidate,
    backend: ParserBackend,
    profile: &NormalizedScannerSettings,
    pdf_pages: u64,
    pdf_identity: Option<&str>,
) -> CacheQuery {
    let pdf_window = (backend == PDF).then(|| PdfWindow {
        max_pages: pdf_pages,
        policy_version: profile.parse.pdf.selection_policy_version.clone(),
        extract_max_pages: pdf_pages,
        selected_pages: None,
    });
    query_for_window(file, backend, profile, pdf_window, pdf_identity)
}

pub(super) fn query_for_window(
    file: &Candidate,
    backend: ParserBackend,
    profile: &NormalizedScannerSettings,
    pdf_window: Option<PdfWindow>,
    pdf_identity: Option<&str>,
) -> CacheQuery {
    let route = (
        PARSE_POLICY,
        PARSER_CONTENT_IDENTITY,
        &file.file.extension,
        file_size_limit(profile, Some(backend)),
        backend,
    );
    let fingerprint = if backend == ParserBackend::LightTextV2 {
        serde_json::to_vec(&(route, text_profile(profile)))
    } else if let Some(window) = &pdf_window {
        serde_json::to_vec(&(
            route,
            pdf_limits(profile, window),
            window,
            profile.pdf_classification_timeout_ms,
            pdf_identity,
        ))
    } else {
        serde_json::to_vec(&(route, office_profile(profile)))
    }
    .expect("parse profile is serializable");
    CacheQuery {
        key: CacheKey {
            file_id: file.file.file_identity.clone(),
            source_guard: file
                .guard
                .as_ref()
                .expect("admitted source has a guard")
                .sha256
                .clone(),
            profile_hash: sha256_hex(&fingerprint),
        },
        pdf_window,
    }
}

pub(super) fn inspection_query(query: &CacheQuery) -> CacheQuery {
    let mut query = query.clone();
    query.key.profile_hash =
        sha256_hex(format!("inspection/{}", query.key.profile_hash).as_bytes());
    query
}

fn text_profile(profile: &NormalizedScannerSettings) -> daily_parsers::settings::TextParseProfile {
    let mut text = profile.parse.text.clone();
    text.max_chars = text.max_chars.min(MAX_FILE_CHARS);
    text.excerpt_max_chars = text.excerpt_max_chars.min(MAX_FILE_CHARS);
    text
}

pub(super) fn file_size_limit(
    profile: &NormalizedScannerSettings,
    backend: Option<ParserBackend>,
) -> u64 {
    let configured = profile.execution.max_file_size_bytes;
    if backend == Some(ParserBackend::RustXlsxBoundedV2) {
        configured.min(office::MAX_XLSX_FILE_BYTES)
    } else {
        configured
    }
}

pub(super) fn ocr_char_limit(profile: &NormalizedScannerSettings) -> u64 {
    profile.parse.pdf.excerpt_max_chars.min(OCR_CHARS_PER_FILE)
}

fn pdf_limits(profile: &NormalizedScannerSettings, window: &PdfWindow) -> ParserLimits {
    if let Some(selected_pages) = &window.selected_pages {
        ParserLimits::PdfImageOcr {
            max_pages: window.extract_max_pages,
            excerpt_max_chars: ocr_char_limit(profile),
            selected_pages: selected_pages.clone(),
        }
    } else {
        ParserLimits::Pdf {
            max_pages: window.extract_max_pages,
            excerpt_max_chars: profile.parse.pdf.excerpt_max_chars.min(MAX_FILE_CHARS),
            selected_pages: None,
        }
    }
}

fn office_profile(profile: &NormalizedScannerSettings) -> OfficeParseProfile {
    let mut office = profile.parse.office.clone();
    office.document_excerpt_max_chars = office.document_excerpt_max_chars.min(MAX_FILE_CHARS);
    office
}

pub(super) fn run(
    file: &Candidate,
    query: &CacheQuery,
    root: &Path,
    profile: &NormalizedScannerSettings,
    dispatch_deadline: Instant,
    pdf: Option<(&mut PdfClient, bool)>,
) -> Completed {
    let started = Instant::now();
    let backend = file.backend.expect("admitted file has a parser");
    let mut timeout_ms = profile
        .execution
        .file_timeout_by_extension_ms
        .get(&file.file.extension)
        .copied()
        .unwrap_or(profile.execution.file_timeout_ms);
    let ocr = query
        .pdf_window
        .as_ref()
        .is_some_and(|window| window.selected_pages.is_some());
    if ocr {
        timeout_ms = timeout_ms.min(OCR_TIMEOUT_MS);
    }
    let deadline = dispatch_deadline.min(started + Duration::from_millis(timeout_ms));
    let guard = file.guard.as_ref().expect("admitted source has a guard");
    let verify = || verify_source(file, root, deadline);
    if let Err(error) = verify() {
        return Completed::new(source_error(file, &error));
    }
    let mut pdf_scan = None;
    let mut outcome = if let Some((client, extract)) = pdf {
        let window = query.pdf_window.as_ref().expect("PDF has a page window");
        let parse_request = request(
            file,
            backend,
            profile,
            pdf_limits(profile, window),
            deadline,
        );
        let (result, evidence) = parse_pdf(
            file,
            client,
            window,
            parse_request,
            extract,
            deadline,
            profile.pdf_classification_timeout_ms,
        );
        pdf_scan = Some(evidence);
        result
    } else if backend == ParserBackend::LightTextV2 {
        let profile_text = text_profile(profile);
        match text::parse_light_text_until(
            Path::new(&file.file.path),
            &file.file.extension,
            &profile_text,
            profile.execution.max_file_size_bytes,
            deadline,
        ) {
            Ok(parsed) => Outcome::content(
                ParseResult {
                    file_path: file.file.path.clone(),
                    file_type: file.file.extension.clone(),
                    content: parsed.content,
                    evidence_audit: Some(parsed.evidence_audit),
                    parser_backend: backend,
                    worker_lane: backend.lane(),
                    truncated: parsed.truncated,
                    duration_ms: elapsed_ms(started),
                    observed_source_version: file.file.source_version.clone(),
                },
                false,
            ),
            Err(error) => failed(
                file,
                match error {
                    LightTextError::FileTooLarge => "FILE_TOO_LARGE",
                    LightTextError::TimedOut => "PARSER_TIMEOUT",
                    _ => "PARSER_FAILED",
                },
                error.to_string(),
                true,
            ),
        }
    } else {
        let request = request(file, backend, profile, office_profile(profile), deadline);
        match office::parse(&request) {
            Ok(result) => Outcome::content(result, false),
            Err(diagnostic) => classify_error(*diagnostic),
        }
    };
    if let Outcome::Parsed {
        result,
        inspection_only,
        ..
    } = outcome
    {
        outcome = parsed_outcome(file, result, guard, root, deadline, inspection_only);
    }
    if let Some(error) = verify().err().filter(|error| {
        error.kind() != io::ErrorKind::TimedOut || outcome.status() != ParseStatus::Timeout
    }) {
        outcome = source_error(file, &error);
        if let Some(pdf) = pdf_scan.as_mut() {
            // A verified preflight does not attest to observations from a changed source.
            pdf.classification = unknown();
        }
    }
    Completed {
        outcome,
        query: Some(query.clone()),
        reused: false,
        attempted: true,
        duration_ms: elapsed_ms(started),
        pdf: pdf_scan,
    }
}

fn request<L>(
    file: &Candidate,
    backend: ParserBackend,
    profile: &NormalizedScannerSettings,
    parser_limits: L,
    deadline: Instant,
) -> ParseRequest<L> {
    ParseRequest {
        file_path: file.file.path.clone(),
        file_type: file.file.extension.clone(),
        backend,
        remaining_timeout_ms: (deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as u64)
            .max(1),
        max_file_size_bytes: file_size_limit(profile, Some(backend)),
        parser_limits,
        expected_source_version: file.file.source_version.clone(),
    }
}

fn parse_pdf(
    file: &Candidate,
    client: &mut PdfClient,
    window: &PdfWindow,
    parse: ParseRequest,
    extract: bool,
    deadline: Instant,
    inspect_timeout_ms: u64,
) -> (Outcome, PdfScan) {
    let guard = file.guard.as_ref().expect("PDF source has a guard");
    let inspected_version = parse.expected_source_version.clone();
    let mut wire = InspectAndExtract::new(
        parse,
        window.max_pages,
        window.policy_version.clone(),
        extract,
    );
    wire.expected_source_guard = Some(PdfSourceGuard {
        kind: guard.kind,
        sha256: guard.sha256.clone(),
    });
    wire.inspect_timeout_ms = Some(inspect_timeout_ms);
    let before = client.business_requests_sent();
    let response = client.inspect_and_extract(&wire, deadline);
    let requests = client.business_requests_sent().saturating_sub(before);
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let code = match error {
                PdfClientError::Timeout => "PARSER_TIMEOUT",
                PdfClientError::Start(_) | PdfClientError::PdfiumUnavailable(_) => {
                    "PARSER_START_FAILED"
                }
                PdfClientError::Protocol(_) | PdfClientError::InvalidRequest(_) => {
                    "PARSER_INVALID_PAYLOAD"
                }
                PdfClientError::WorkerExited => "PARSER_FAILED",
            };
            let outcome = failed(file, code, error.to_string(), true);
            return (
                outcome,
                PdfScan {
                    classification: unknown(),
                    charged_pages: window.max_pages,
                    extraction_slot: false,
                    requests,
                    document_opens: 0,
                    page_reads: 0,
                },
            );
        }
    };
    let negative = response.negative_kind();
    let ocr = matches!(wire.parse.parser_limits, ParserLimits::PdfImageOcr { .. });
    let scan = PdfScan {
        extraction_slot: extract
            && response.classification.status == ClassifyStatus::TextInParseWindow,
        classification: response.classification,
        charged_pages: window.max_pages,
        requests,
        document_opens: u64::from(response.native_document_opens),
        page_reads: response.native_page_reads,
    };
    let diagnostic = || {
        scan.classification.diagnostic.clone().unwrap_or_else(|| {
            diagnostic(
                file,
                "PARSER_INVALID_PAYLOAD",
                "PDF response has no definitive result".into(),
                true,
            )
        })
    };
    let body = |result| Outcome::content(result, !extract);
    let outcome = if ocr {
        match response.extraction {
            Some(Ok(result)) => body(result),
            Some(Err(error)) => classify_error(error),
            None => classify_error(diagnostic()),
        }
    } else {
        match negative {
            Some(kind) => Outcome::Negative(kind),
            None if scan.classification.status != ClassifyStatus::TextInParseWindow => {
                classify_error(diagnostic())
            }
            None if !extract => body(ParseResult {
                file_path: file.file.path.clone(),
                file_type: file.file.extension.clone(),
                content: String::new(),
                evidence_audit: Some(Default::default()),
                parser_backend: PDF,
                worker_lane: PDF.lane(),
                truncated: false,
                duration_ms: 0,
                observed_source_version: inspected_version,
            }),
            None => match response.extraction {
                Some(Ok(result)) => body(result),
                Some(Err(error)) => classify_error(error),
                None => classify_error(diagnostic()),
            },
        }
    };
    (outcome, scan)
}

pub(super) fn verify_source(file: &Candidate, root: &Path, deadline: Instant) -> io::Result<()> {
    if file.guard.as_ref() == Some(&observe(&file.file, root, deadline)?) {
        Ok(())
    } else {
        Err(source_changed())
    }
}

fn parsed_outcome(
    file: &Candidate,
    mut result: Box<ParseResult>,
    guard: &SourceGuard,
    root: &Path,
    deadline: Instant,
    inspection_only: bool,
) -> Outcome {
    if result.parser_backend != file.backend.expect("admitted source has a parser")
        || result.worker_lane != result.parser_backend.lane()
        || result.file_type != file.file.extension
        || result.file_path != file.file.path
        || result.observed_source_version != file.file.source_version
        || result.content.chars().count() > MAX_FILE_CHARS as usize
        || result
            .evidence_audit
            .as_ref()
            .is_some_and(|audit| audit.validate(&result.content).is_err())
    {
        return failed(
            file,
            "PARSER_INVALID_PAYLOAD",
            "parser result does not match the source request".into(),
            true,
        );
    }
    match source_sha256(&file.file, guard, root, deadline) {
        Ok(source_sha256) => {
            result.observed_source_version.clone_from(&guard.sha256);
            Outcome::Parsed {
                result,
                source_sha256,
                inspection_only,
            }
        }
        Err(error) => source_error(file, &error),
    }
}

pub(super) fn source_error(file: &Candidate, error: &io::Error) -> Outcome {
    let code = match error.kind() {
        io::ErrorKind::TimedOut => "STAGE_DEADLINE_EXHAUSTED",
        io::ErrorKind::InvalidData => "SOURCE_VERSION_CHANGED",
        _ => "SOURCE_GUARD_UNAVAILABLE",
    };
    failed(file, code, error.to_string(), true)
}

pub(super) fn failed(file: &Candidate, code: &str, message: String, retryable: bool) -> Outcome {
    classify_error(diagnostic(file, code, message, retryable))
}

fn diagnostic(file: &Candidate, code: &str, message: String, retryable: bool) -> WorkerDiagnostic {
    WorkerDiagnostic {
        error_code: code.into(),
        message,
        retryable,
        stage: "parse".into(),
        file_path: Some(file.file.path.clone()),
        backend: file.backend.map(|backend| backend.as_str().into()),
    }
}

fn classify_error(diagnostic: WorkerDiagnostic) -> Outcome {
    let (reason, status) = match diagnostic.error_code.as_str() {
        "PARSER_TIMEOUT" => ("parser_timeout", ParseStatus::Timeout),
        "STAGE_DEADLINE_EXHAUSTED" => ("scan_deadline", ParseStatus::Timeout),
        "SOURCE_VERSION_CHANGED" => ("source_version_changed", ParseStatus::Error),
        "SOURCE_GUARD_UNAVAILABLE" => ("source_guard_unavailable", ParseStatus::Error),
        "PARSER_START_FAILED" => ("parser_start_failed", ParseStatus::Error),
        "PROFILE_ROUTE_INVARIANT" => ("profile_route_invariant", ParseStatus::Error),
        "PDF_PROTECTED" => ("pdf_protected", ParseStatus::Error),
        "FILE_TOO_LARGE" => ("file_size_policy", ParseStatus::NotParsed),
        _ if diagnostic.message == OFFICE_CONTENT_MISSING_MESSAGE => {
            ("office_content_missing", ParseStatus::Error)
        }
        _ => ("parse_failed", ParseStatus::Error),
    };
    Outcome::Failed {
        diagnostic,
        reason,
        status,
    }
}

fn unknown() -> ClassifyResult {
    ClassifyResult {
        status: ClassifyStatus::Unknown,
        page_count: None,
        result_examined_pages: None,
        page_audit: None,
        diagnostic: None,
    }
}

pub(super) fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}
