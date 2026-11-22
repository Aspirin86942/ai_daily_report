pub(crate) use super::allocation::{range_spans, ranges_for, AllocatedExcerpt, ExcerptBlock};
use super::{
    allocation::range_gaps,
    parse::{self, Candidate, Outcome},
    sha256_hex,
    source_guard::file_source_id,
    PARSER_CONTENT_IDENTITY,
};
use chrono::{DateTime, Utc};
use daily_parsers::{
    contract::SourceRangeUnit,
    pdf_wire::PdfNegativeKind,
    settings::{
        AuditWorkerLane, FileAuditV2, NormalizedScannerSettings, ParseStatus, PdfPageAudit,
        OCR_BACKEND,
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
    pub worker_lane: AuditWorkerLane,
    pub parse_status: ParseStatus,
    pub parse_truncated: bool,
    pub excerpt_truncated: bool,
    pub coverage_limits: Value,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SourceProfile {
    pub observed_at_utc: String,
    limits: Value,
    algorithms: Value,
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
        Self {
            observed_at_utc: observed.to_rfc3339(),
            limits,
            algorithms: json!({
                "cache_payload": "4",
                "excerpt": profile.parse.excerpt_policy_version,
                "admission": profile.admission_policy_version,
                "pdf_selection": profile.parse.pdf.selection_policy_version,
                "parser_content_identity": PARSER_CONTENT_IDENTITY,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScannedFile {
    pub audit: FileAuditV2,
    pub excerpt: AllocatedExcerpt,
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
    priority: u64,
    pdf_extraction_slot: Option<bool>,
    pdf_ocr_reserved_pages: Option<u64>,
}

impl ScannedFile {
    pub(super) fn new(
        file: &Candidate,
        mut excerpt: AllocatedExcerpt,
        audit: FileAuditV2,
        profile: &NormalizedScannerSettings,
    ) -> Self {
        let completed = file.result();
        let parsed = completed.obtained();
        let unverified = audit.parser_backend == OCR_BACKEND && parsed.is_some();
        let reason = completed
            .outcome
            .reason()
            .or_else(|| excerpt.truncated.then_some("excerpt_character_budget"))
            .or_else(|| {
                (audit.parse_status == ParseStatus::Success && excerpt.text.is_empty()).then_some(
                    if audit.parser_backend == OCR_BACKEND {
                        "pdf_image_ocr_empty"
                    } else {
                        "empty_document"
                    },
                )
            });
        if let Some((pdf, page_count)) = audit
            .pdf_classification
            .0
            .as_ref()
            .and_then(|pdf| Some((pdf, pdf.page_count.0?)))
        {
            let mut read = range_spans(
                excerpt.read_ranges.as_deref().unwrap_or_default(),
                SourceRangeUnit::PdfPage,
            );
            if matches!(
                completed.outcome,
                Outcome::Negative(PdfNegativeKind::NoText)
            ) {
                read.extend(checked_pages(pdf.page_audit.as_ref()));
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
        let modified_nanos: u128 = audit
            .source_version
            .strip_prefix("mtime_ns=")
            .and_then(|version| version.split_once(':'))
            .and_then(|(ns, _)| ns.parse().ok())
            .expect("discovery produced a valid modification timestamp");
        let modified = DateTime::<Utc>::from_timestamp(
            (modified_nanos / 1_000_000_000) as i64,
            (modified_nanos % 1_000_000_000) as u32,
        )
        .expect("filesystem modification timestamp fits chrono");
        Self {
            source_id: file_source_id(&audit.relative_path),
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
            unverified,
            priority: file.priority,
            pdf_extraction_slot: file
                .completed
                .as_ref()
                .and_then(|result| result.pdf.as_ref())
                .map(|pdf| pdf.extraction_slot),
            pdf_ocr_reserved_pages: file
                .ocr
                .as_ref()
                .and_then(|result| result.query.as_ref())
                .and_then(|query| query.pdf_window.as_ref())
                .map(|window| window.extract_max_pages),
            audit,
            excerpt,
        }
    }

    pub(crate) fn pdf_pages(&self) -> Option<&PdfPageAudit> {
        self.audit
            .pdf_classification
            .0
            .as_ref()?
            .page_audit
            .as_ref()
    }

    fn coverage_limits(&self, profile: &SourceProfile, for_report: bool) -> Value {
        let mut limits = profile.limits.clone();
        let admission = &mut limits["scan_admission"];
        admission["priority"] = self.priority.into();
        admission["pdf_reserved_pages"] = json!(self
            .pdf_extraction_slot
            .and(self.audit.pdf_classification.0.as_ref())
            .map(|pdf| pdf.nominal_charged_pages));
        admission["pdf_extraction_slot"] = json!(self.pdf_extraction_slot);
        if let Some(pages) = self.pdf_ocr_reserved_pages {
            admission["pdf_ocr_reserved_pages"] = pages.into();
        }
        let mut audit = self.excerpt.audit.as_ref().map_or_else(
            || json!({"version": 1}),
            |parser| serde_json::to_value(parser).expect("serializable parser audit"),
        );
        if for_report {
            let fields = audit.as_object_mut().unwrap();
            fields.remove("content_units");
            fields.remove("read_ranges");
        } else if let Some(units) = &self.excerpt.units {
            audit["content_units"] = json!(units);
        }
        audit["primary_status"] = match self.audit.parse_status {
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
        audit["parser_version"] = self.audit.parser_backend.as_str().into();
        audit["algorithm_versions"] = profile.algorithms.clone();
        limits["evidence_audit"] = audit;
        limits
    }

    pub(crate) fn report_limits(&self, profile: &SourceProfile, tags: impl Serialize) -> Value {
        let mut limits = self.coverage_limits(profile, true);
        limits["evidence_audit"]["secondary_tags"] = json!(tags);
        limits
    }

    pub(super) fn scan_view(&self, profile: &SourceProfile) -> ScanSource {
        let mut limits = self.coverage_limits(profile, false);
        let audit = &mut limits["evidence_audit"];
        self.excerpt.write_file_ranges(audit);
        if let Some(tag) = &self.ocr_tag {
            audit["secondary_tags"] = json!([tag]);
        }
        audit["excerpt_ranges"] = json!(self.excerpt.ranges);
        if !self.excerpt.blocks.is_empty() {
            let (ends, parents): (Vec<_>, Vec<_>) = self
                .excerpt
                .blocks
                .iter()
                .map(|block| (block.end, &block.inherited))
                .unzip();
            audit["excerpt_block_ends"] = json!(ends);
            audit["excerpt_block_context"] = json!(parents);
        }
        ScanSource {
            source_id: self.source_id.clone(),
            relative_path: self.audit.relative_path.clone(),
            extension: self.extension.clone(),
            size_bytes: self.size_bytes,
            modified_at_utc: self.modified_at_utc.clone(),
            observed_at_utc: profile.observed_at_utc.clone(),
            content_sha256: self.raw_sha256.clone(),
            excerpt_sha256: self.excerpt_sha256.clone(),
            excerpt: self.excerpt.text.clone(),
            parser_backend: self.audit.parser_backend.clone(),
            worker_lane: self.audit.worker_lane,
            parse_status: self.audit.parse_status,
            parse_truncated: self.audit.truncated,
            excerpt_truncated: self.excerpt.truncated,
            coverage_limits: limits,
            reason: self.reason.map(str::to_owned),
        }
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

fn checked_pages(pages: Option<&PdfPageAudit>) -> impl Iterator<Item = (u64, u64)> + '_ {
    pages
        .into_iter()
        .flat_map(|pages| &pages.checked_pages)
        .map(|page| (*page, page + 1))
}
