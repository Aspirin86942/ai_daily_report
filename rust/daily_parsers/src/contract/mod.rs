//! Typed parser input, output and source ranges.
pub use crate::settings::{
    pdf_sample_pages, PdfPageAudit, PdfPageObservation, OCR_BACKEND, OCR_RANGE_LABEL,
};
use crate::settings::{
    require_range as validate_range, require_source_version, validate_text_window,
};
use crate::settings::{OfficeParseProfile, Validate};
use serde::{Deserialize, Serialize};

mod content_audit;
pub use content_audit::{
    AuditTextBuilder, ContentUnit, ContentUnitKind, ParsedContentAudit, SourceRange,
    SourceRangeUnit,
};

pub const OCR_PAGES_PER_FILE: u64 = 3;
pub const OCR_CHARS_PER_FILE: u64 = 8192;
pub const OCR_TIMEOUT_MS: u64 = 15_000;
pub const OFFICE_CONTENT_MISSING_MESSAGE: &str =
    "Office package contains only metadata and has no document content";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDiagnostic {
    pub error_code: String,
    pub message: String,
    pub retryable: bool,
    pub stage: String,
    pub file_path: Option<String>,
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParserBackend {
    #[serde(rename = "light_text_v2")]
    LightTextV2,
    #[serde(rename = "rust_office_oxide_v2")]
    RustOfficeOxideV2,
    #[serde(rename = "rust_xlsx_bounded_v2")]
    RustXlsxBoundedV2,
    #[serde(rename = "rust_xls_calamine_v3")]
    RustXlsCalamineV3,
    #[serde(rename = "rust_pdfium_v3")]
    RustPdfiumV3,
    #[serde(rename = "rust_legacy_office_v3")]
    RustLegacyOfficeV3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLane {
    RustCore,
    RustOfficeProcessV3,
    RustOfficeInProcess,
    RustPdfProcessV3,
}

impl ParserBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LightTextV2 => "light_text_v2",
            Self::RustOfficeOxideV2 => "rust_office_oxide_v2",
            Self::RustXlsxBoundedV2 => "rust_xlsx_bounded_v2",
            Self::RustXlsCalamineV3 => "rust_xls_calamine_v3",
            Self::RustPdfiumV3 => "rust_pdfium_v3",
            Self::RustLegacyOfficeV3 => "rust_legacy_office_v3",
        }
    }

    pub fn supports(self, extension: &str) -> bool {
        match self {
            Self::LightTextV2 => matches!(extension, ".txt" | ".md" | ".csv" | ".json" | ".log"),
            Self::RustOfficeOxideV2 => matches!(extension, ".docx" | ".pptx"),
            Self::RustXlsxBoundedV2 => extension == ".xlsx",
            Self::RustXlsCalamineV3 => extension == ".xls",
            Self::RustPdfiumV3 => extension == ".pdf",
            Self::RustLegacyOfficeV3 => matches!(extension, ".doc" | ".ppt"),
        }
    }

    pub const fn lane(self) -> WorkerLane {
        match self {
            Self::LightTextV2 => WorkerLane::RustCore,
            Self::RustPdfiumV3 => WorkerLane::RustPdfProcessV3,
            _ => WorkerLane::RustOfficeInProcess,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ParserLimits {
    #[serde(rename = "pdf")]
    Pdf {
        max_pages: u64,
        excerpt_max_chars: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_pages: Option<Vec<u64>>,
    },
    #[serde(rename = "pdf_image_ocr")]
    PdfImageOcr {
        max_pages: u64,
        excerpt_max_chars: u64,
        selected_pages: Vec<u64>,
    },
}

impl ParserLimits {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Pdf {
                max_pages,
                excerpt_max_chars,
                selected_pages,
            } => {
                validate_range(*max_pages, 1, 10_000, "max_pages")?;
                if let Some(pages) = selected_pages {
                    validate_selected_pages(pages, *max_pages)?;
                }
                validate_range(*excerpt_max_chars, 1, 10_000_000, "excerpt_max_chars")
            }
            Self::PdfImageOcr {
                max_pages,
                excerpt_max_chars,
                selected_pages,
            } => {
                validate_range(*max_pages, 1, OCR_PAGES_PER_FILE, "max_pages")?;
                validate_range(
                    *excerpt_max_chars,
                    1,
                    OCR_CHARS_PER_FILE,
                    "excerpt_max_chars",
                )?;
                validate_selected_pages(selected_pages, *max_pages)
            }
        }
    }
}

fn validate_selected_pages(pages: &[u64], max_pages: u64) -> Result<(), String> {
    if pages.is_empty()
        || pages.len() as u64 > max_pages
        || pages.contains(&0)
        || pages.iter().collect::<std::collections::HashSet<_>>().len() != pages.len()
    {
        return Err("invalid PDF selected pages".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseRequest<L = ParserLimits> {
    pub file_path: String,
    pub file_type: String,
    pub backend: ParserBackend,
    pub remaining_timeout_ms: u64,
    pub max_file_size_bytes: u64,
    pub parser_limits: L,
    pub expected_source_version: String,
}

impl<L> ParseRequest<L> {
    fn validate_route(&self, lane: WorkerLane) -> Result<(), String> {
        validate_absolute_path(&self.file_path, "file_path")?;
        validate_extension(&self.file_type, "file_type")?;
        validate_range(
            self.remaining_timeout_ms,
            1,
            3_600_000,
            "remaining_timeout_ms",
        )?;
        validate_range(
            self.max_file_size_bytes,
            1,
            4_294_967_296,
            "max_file_size_bytes",
        )?;
        require_source_version(&self.expected_source_version, "expected_source_version")?;
        if self.backend.supports(&self.file_type) && self.backend.lane() == lane {
            Ok(())
        } else {
            Err("parse request route is inconsistent".to_string())
        }
    }
}

impl ParseRequest<ParserLimits> {
    pub fn validate(&self) -> Result<(), String> {
        self.validate_route(WorkerLane::RustPdfProcessV3)?;
        self.parser_limits.validate()
    }
}

impl ParseRequest<OfficeParseProfile> {
    pub fn validate(&self) -> Result<(), String> {
        self.validate_route(WorkerLane::RustOfficeInProcess)?;
        self.parser_limits.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseResult {
    pub file_path: String,
    pub file_type: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_audit: Option<ParsedContentAudit>,
    pub parser_backend: ParserBackend,
    pub worker_lane: WorkerLane,
    pub truncated: bool,
    pub duration_ms: u64,
    pub observed_source_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifyStatus {
    TextInParseWindow,
    NoTextInParseWindow,
    Unknown,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifyResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_audit: Option<PdfPageAudit>,
    pub status: ClassifyStatus,
    pub page_count: Option<u64>,
    pub result_examined_pages: Option<u64>,
    pub diagnostic: Option<WorkerDiagnostic>,
}

impl ClassifyResult {
    pub fn validate_for_max_pages(&self, max_pages: u64) -> Result<(), String> {
        validate_range(max_pages, 1, 10_000, "max_pages")?;
        if let Some(audit) = &self.page_audit {
            audit.validate_counts(self.page_count, self.result_examined_pages)?;
        }
        match self.status {
            ClassifyStatus::TextInParseWindow | ClassifyStatus::NoTextInParseWindow => {
                if self.diagnostic.is_some() {
                    return Err("text/no-text result must not carry a diagnostic".into());
                }
                validate_text_window(
                    self.page_count,
                    self.result_examined_pages,
                    max_pages,
                    self.status == ClassifyStatus::TextInParseWindow,
                )?;
            }
            ClassifyStatus::Unknown | ClassifyStatus::Error => {
                let limit = self.page_count.unwrap_or(max_pages).min(max_pages);
                if self
                    .result_examined_pages
                    .is_some_and(|pages| pages > limit)
                {
                    return Err("typed failure exceeds the request page window".into());
                }
                let retryable = self.status == ClassifyStatus::Unknown;
                if self.diagnostic.as_ref().map(|error| error.retryable) != Some(retryable) {
                    return Err(
                        "PDF failure requires a diagnostic with matching retryability".into(),
                    );
                }
            }
        }
        Ok(())
    }
}

impl ParseResult {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(audit) = &self.evidence_audit {
            audit.validate(&self.content)?;
        }
        validate_absolute_path(&self.file_path, "file_path")?;
        validate_extension(&self.file_type, "file_type")?;
        require_source_version(&self.observed_source_version, "observed_source_version")?;
        let detected = self
            .evidence_audit
            .as_ref()
            .and_then(|audit| audit.detected_format.as_ref())
            .map(|format| format!(".{format}"));
        if self
            .parser_backend
            .supports(detected.as_deref().unwrap_or(&self.file_type))
            && self.parser_backend.lane() == self.worker_lane
        {
            Ok(())
        } else {
            Err("parse result route is inconsistent".to_string())
        }
    }
}
fn validate_absolute_path(value: &str, field: &str) -> Result<(), String> {
    if value.len() > 32_767 || (cfg!(windows) && !std::path::Path::new(value).is_absolute()) {
        return Err(format!("{field} must be an absolute path"));
    }
    crate::settings::require_absolute_path(value, field)
}

fn validate_extension(value: &str, field: &str) -> Result<(), String> {
    if value.len() >= 2
        && value.len() <= 32
        && value.starts_with('.')
        && value == value.to_ascii_lowercase()
        && value[1..].bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        Ok(())
    } else {
        Err(format!("{field} is not a normalized extension"))
    }
}
