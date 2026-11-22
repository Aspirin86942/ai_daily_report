pub(crate) use super::allocation::{range_spans, ranges_for, AllocatedExcerpt, ExcerptBlock};
use super::{
    allocation::range_gaps,
    parse::{self, Candidate, Completed, Outcome},
    sha256_hex,
    source_guard::file_source_id,
    PdfRuntime, PARSER_CONTENT_IDENTITY,
};
use chrono::{DateTime, Utc};
use daily_parsers::{
    contract::{
        ContentUnit, ParsedContentAudit, SourceGuardKind, SourceRange, SourceRangeUnit, WorkerLane,
    },
    pdf_wire::PdfNegativeKind,
    settings::{
        Diagnostic, ErrorCode, NormalizedScannerSettings, ParseStatus, PdfPageAudit, OCR_BACKEND,
    },
};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize)]
pub struct ScanSource {
    pub source_id: String,
    pub relative_path: String,
    pub extension: String,
    pub size_bytes: u64,
    pub modified_at_utc: String,
    pub observed_at_utc: String,
    pub content_sha256: Option<String>,
    pub excerpt_sha256: String,
    pub excerpt: String,
    pub parser_backend: String,
    pub worker_lane: String,
    pub parse_status: ParseStatus,
    pub parse_truncated: bool,
    pub excerpt_truncated: bool,
    pub coverage_limits: Value,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SourceProfile {
    pub observed_at_utc: String,
    limits: Value,
}

impl SourceProfile {
    pub(super) fn new(profile: &NormalizedScannerSettings, observed: DateTime<Utc>) -> Self {
        let mut limits = serde_json::to_value(&profile.parse).expect("serializable parse profile");
        limits["scan_admission"] = json!({
            "candidate_limit": profile.max_candidate_files,
            "file_char_limit": parse::MAX_FILE_CHARS,
            "total_char_limit": super::evidence::char_budget(profile),
            "pdf_inspection_page_limit": profile.max_total_pdf_classification_pages,
            "pdf_extraction_limit": profile.max_pdf_text_extractions,
        });
        limits["evidence_audit"] = json!({
            "version": 1,
            "algorithm_versions": {
                "cache_payload": "4",
                "excerpt": profile.parse.excerpt_policy_version,
                "admission": profile.admission_policy_version,
                "pdf_selection": profile.parse.pdf.selection_policy_version,
                "parser_content_identity": PARSER_CONTENT_IDENTITY,
            },
        });
        Self {
            observed_at_utc: observed.to_rfc3339(),
            limits,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub relative_path: String,
    pub parse_status: ParseStatus,
    pub parser_backend: &'static str,
    pub worker_lane: &'static str,
    pub parse_truncated: bool,
    pub pdf_charged_pages: Option<u64>,
    pdf_page_audit: Option<PdfPageAudit>,
    pub pdf_error_code: Option<ErrorCode>,
    pub(crate) excerpt: AllocatedExcerpt,
    pub source_id: String,
    pub extension: String,
    pub size_bytes: u64,
    pub modified_at_utc: String,
    pub raw_sha256: Option<String>,
    pub excerpt_sha256: String,
    pub reason: Option<&'static str>,
    pub max_file_size_bytes: Option<u64>,
    pub ocr_tag: Option<String>,
    pub unverified: bool,
    pub priority: u64,
    pub pdf_extraction_slot: Option<bool>,
    pdf_ocr_reserved_pages: Option<u64>,
}

pub struct ScanExcerpt<'a> {
    pub text: &'a str,
    pub chars: usize,
    pub truncated: bool,
    pub audit: Option<&'a ParsedContentAudit>,
    pub units: Option<&'a [ContentUnit]>,
    pub read_ranges: Option<&'a [SourceRange]>,
    pub omitted_ranges: &'a [SourceRange],
    pub ranges: Option<&'a [SourceRange]>,
}

#[derive(Debug, Clone)]
pub struct ScanBenchFile {
    pub input_chars: u64,
    pub content_sha256: String,
    pub parse_duration_ms: u64,
    pub diagnostic_code: Option<String>,
    pub final_diagnostic: Option<Diagnostic>,
    pub source_guard_kind: Option<SourceGuardKind>,
    pub parse_attempted: bool,
    pub pdf_runtime: Option<PdfRuntime>,
}

impl ScannedFile {
    pub(super) fn new(
        file: &Candidate,
        native: &Completed,
        mut excerpt: AllocatedExcerpt,
        parse_attempted: bool,
        error_code: Option<ErrorCode>,
        profile: &NormalizedScannerSettings,
    ) -> Self {
        let completed = file.result();
        let parsed = completed.obtained();
        let parse_status = completed.outcome.status();
        let failed = matches!(parse_status, ParseStatus::Error | ParseStatus::Timeout);
        let no_text = matches!(
            completed.outcome,
            Outcome::Negative(PdfNegativeKind::NoText)
        );
        let pdf = native.pdf.as_ref();
        let classification = pdf.map(|pdf| &pdf.classification);
        let pdf_page_audit = classification.and_then(|result| result.page_audit.as_ref());
        if let Some(page_count) = classification.and_then(|result| result.page_count) {
            let mut read = range_spans(
                excerpt.read_ranges.as_deref().unwrap_or_default(),
                SourceRangeUnit::PdfPage,
            );
            if let Some(pages) = pdf_page_audit.filter(|_| no_text) {
                read.extend(pages.checked_pages.iter().map(|&page| (page, page + 1)));
            }
            read.sort_unstable();
            range_gaps(
                1,
                page_count + 1,
                SourceRangeUnit::PdfPage,
                "unread",
                read,
                &mut excerpt.omitted_ranges,
            );
        }
        let parse_truncated = parsed.is_some_and(|(result, _)| result.truncated)
            || (no_text
                && classification
                    .is_some_and(|result| result.page_count > result.result_examined_pages));
        let (parser_backend, worker_lane) =
            if file.ocr.is_some() && (parsed.is_some() || parse_attempted) {
                (OCR_BACKEND, WorkerLane::RustPdfProcessV3.as_str())
            } else if no_text {
                ("pdf_metadata_v2", WorkerLane::RustCore.as_str())
            } else if parsed.is_some() || (parse_attempted && failed) {
                let backend = file.backend.expect("parsed file has a backend");
                (backend.as_str(), backend.lane().as_str())
            } else {
                ("not_parsed", "not_parsed")
            };
        let reason = completed
            .outcome
            .reason()
            .or_else(|| excerpt.truncated.then_some("excerpt_character_budget"))
            .or_else(|| {
                (parse_status == ParseStatus::Success && excerpt.text.is_empty()).then_some(
                    if parser_backend == OCR_BACKEND {
                        "pdf_image_ocr_empty"
                    } else {
                        "empty_document"
                    },
                )
            });
        // Discovery's display timestamp has microsecond precision. Its version retains nanoseconds.
        let mtime_ns: u128 = file
            .file
            .source_version
            .strip_prefix("mtime_ns=")
            .and_then(|version| version.split_once(':'))
            .and_then(|(ns, _)| ns.parse().ok())
            .expect("discovery produced a valid modification timestamp");
        let modified = DateTime::<Utc>::from_timestamp(
            (mtime_ns / 1_000_000_000) as i64,
            (mtime_ns % 1_000_000_000) as u32,
        )
        .expect("filesystem modification timestamp fits chrono");
        Self {
            relative_path: file.relative_path.clone(),
            parse_status,
            parser_backend,
            worker_lane,
            parse_truncated,
            pdf_charged_pages: pdf.map(|pdf| pdf.charged_pages),
            pdf_page_audit: pdf_page_audit.cloned(),
            pdf_error_code: if file.file.extension == ".pdf" {
                error_code
            } else {
                None
            },
            source_id: file_source_id(&file.relative_path),
            extension: file.file.extension.clone(),
            size_bytes: file.file.size_bytes,
            modified_at_utc: modified.to_rfc3339(),
            raw_sha256: parsed
                .filter(|_| !excerpt.text.is_empty())
                .map(|(_, hash)| hash.clone()),
            excerpt_sha256: sha256_hex(excerpt.text.as_bytes()),
            reason,
            max_file_size_bytes: (reason == Some("file_size_policy"))
                .then(|| parse::file_size_limit(profile, file.backend)),
            ocr_tag: completed
                .diagnostic()
                .filter(|d| d.message.starts_with("PDF_OCR_"))
                .map(|d| d.message.clone()),
            unverified: parser_backend == OCR_BACKEND && parsed.is_some(),
            priority: file.priority,
            pdf_extraction_slot: pdf.map(|pdf| pdf.extraction_slot),
            pdf_ocr_reserved_pages: file
                .ocr
                .as_ref()
                .and_then(|result| result.query.as_ref())
                .and_then(|query| query.pdf_window.as_ref())
                .map(|window| window.extract_max_pages),
            excerpt,
        }
    }

    pub fn pdf_pages(&self) -> Option<&PdfPageAudit> {
        self.pdf_page_audit.as_ref()
    }

    pub fn coverage_limits(&self, profile: &SourceProfile) -> Value {
        let mut limits = profile.limits.clone();
        let admission = &mut limits["scan_admission"];
        admission["priority"] = self.priority.into();
        admission["pdf_reserved_pages"] =
            json!(self.pdf_extraction_slot.and(self.pdf_charged_pages));
        admission["pdf_extraction_slot"] = json!(self.pdf_extraction_slot);
        if let Some(pages) = self.pdf_ocr_reserved_pages {
            admission["pdf_ocr_reserved_pages"] = pages.into();
        }
        let audit = &mut limits["evidence_audit"];
        if let Some(parser) = &self.excerpt.audit {
            audit["usable_chars"] = json!(parser.usable_chars);
            audit["encoding"] = json!(parser.encoding);
            audit["detected_format"] = json!(parser.detected_format);
        }
        audit["primary_status"] = match self.parse_status {
            ParseStatus::NotParsed => "not_parsed",
            ParseStatus::Timeout | ParseStatus::Error => "parse_failed",
            ParseStatus::Success if self.excerpt.text.is_empty() => "empty_excerpt",
            ParseStatus::Success => "usable_excerpt",
        }
        .into();
        if self.unverified {
            audit["text_unverified"] = true.into();
        }
        if let Some(reason) = self.reason {
            audit["primary_reason"] = reason.into();
        }
        if let Some(limit) = self.max_file_size_bytes {
            audit["max_file_size_bytes"] = limit.into();
        }
        if let Some(pages) = self.pdf_pages() {
            audit["pdf_page_audit"] = json!(pages);
        }
        audit["parser_version"] = self.parser_backend.into();
        limits
    }

    pub(crate) fn report_limits(&self, profile: &SourceProfile, tags: impl Serialize) -> Value {
        let mut limits = self.coverage_limits(profile);
        limits["evidence_audit"]["secondary_tags"] = json!(tags);
        limits
    }

    pub fn benchmark_excerpt(&self) -> ScanExcerpt<'_> {
        ScanExcerpt {
            text: &self.excerpt.text,
            chars: self.excerpt.chars,
            truncated: self.excerpt.truncated,
            audit: self.excerpt.audit.as_ref(),
            units: self.excerpt.units.as_deref(),
            read_ranges: self.excerpt.read_ranges.as_deref(),
            omitted_ranges: &self.excerpt.omitted_ranges,
            ranges: self.excerpt.ranges.as_deref(),
        }
    }

    pub fn benchmark_blocks(&self) -> impl Iterator<Item = (usize, &[usize])> {
        self.excerpt
            .blocks
            .iter()
            .map(|block| (block.end, block.inherited.as_slice()))
    }
}

impl AllocatedExcerpt {
    pub(crate) fn write_file_ranges(&self, audit: &mut Value) {
        if self.audit.is_some() {
            audit["read_ranges"] = json!(self.read_ranges);
        }
        if !self.omitted_ranges.is_empty() {
            audit["omitted_ranges"] = json!(self.omitted_ranges);
        }
    }
}
