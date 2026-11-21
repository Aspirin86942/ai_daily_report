use super::profiles::{validate_csv_encodings, validate_excerpt_priority_paths};
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReportMode {
    #[serde(rename = "daily")]
    Daily,
    #[serde(rename = "weekly")]
    Weekly,
    #[serde(rename = "monthly")]
    Monthly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompressionProfile {
    #[serde(rename = "daily_balanced_v1")]
    DailyBalancedV1,
    #[serde(rename = "weekly_balanced_v1")]
    WeeklyBalancedV1,
    #[serde(rename = "monthly_balanced_v1")]
    MonthlyBalancedV1,
}

impl ReportMode {
    pub const fn compression_profile(self) -> CompressionProfile {
        match self {
            Self::Daily => CompressionProfile::DailyBalancedV1,
            Self::Weekly => CompressionProfile::WeeklyBalancedV1,
            Self::Monthly => CompressionProfile::MonthlyBalancedV1,
        }
    }
}

impl CompressionProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DailyBalancedV1 => "daily_balanced_v1",
            Self::WeeklyBalancedV1 => "weekly_balanced_v1",
            Self::MonthlyBalancedV1 => "monthly_balanced_v1",
        }
    }
}

pub const CONTEXT_PER_FILE_MAX_CHARS: u64 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterPaths {
    pub office_worker_path: String,
    pub pdf_worker_path: String,
}

impl Validate for AdapterPaths {
    fn validate(&self) -> Result<(), String> {
        require_absolute_path(&self.office_worker_path, "office_worker_path")?;
        require_absolute_path(&self.pdf_worker_path, "pdf_worker_path")
    }
}

// ---------------------------------------------------------------------------
// Scanner settings constants and the strict raw settings type. All leaves stay
// optional; Rust normalization owns every default and routing policy.
// ---------------------------------------------------------------------------

pub const ADMISSION_POLICY_VERSION: &str = "budget_refill_v3";
pub const CLASSIFIER_POLICY_VERSION: &str = "pdf_text_presence_v2";
pub const PRIORITY_POLICY_VERSION: &str = "budget_nominal_v2";
pub const COMPRESSION_POLICY_VERSION: &str = "markdown_context_v2";
pub const MAX_SOURCE_FILES_PER_RUN: u64 = 1_000_000;

/// report-mode 默认（spec Part 8.1 表）：
/// daily   => (96, 80, 8, 10_000ms)
/// weekly  => (192, 100, 12, 15_000ms)
/// monthly => (384, 370, 16, 25_000ms)
/// tuple order: (max_candidate_files, max_total_pdf_classification_pages,
/// max_pdf_text_extractions, total_deadline_ms).
pub fn quota_defaults(mode: ReportMode) -> (u64, u64, u64, u64) {
    match mode {
        ReportMode::Daily => (96, 80, 8, 10_000),
        ReportMode::Weekly => (192, 100, 12, 15_000),
        ReportMode::Monthly => (384, 370, 16, 25_000),
    }
}

// Every override has the same wire contract: missing uses the profile default,
// explicit null is invalid, and an absent setting is omitted when serializing.
macro_rules! scanner_settings {
    ($($field:ident: $value:ty),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct ScannerSettings {
            $(
                #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_non_null")]
                pub $field: Option<$value>,
            )*
        }
    };
}

scanner_settings! {
    pdf_image_ocr: PdfImageOcrSettings,
    allowed_extensions: Vec<String>,
    ignored_patterns: Vec<String>,
    excluded_dirs: Vec<String>,
    follow_symlinks: bool,
    cloud_files_policy: String,
    max_workers: u64,
    max_file_size_mb: u64,
    discovery_timeout_seconds: u64,
    file_timeout_seconds: u64,
    file_timeout_by_extension: BTreeMap<String, u64>,
    total_max_chars: u64,
    context_max_chars: u64,
    excerpt_priority_paths: Vec<String>,
    legacy_office_enabled: bool,
    pptx_include_notes: bool,
    direct_text_max_bytes: u64,
    direct_text_read_bytes: u64,
    log_tail_read_bytes: u64,
    text_excerpt_max_chars: u64,
    excel_max_rows: u64,
    pdf_max_pages: u64,
    text_max_chars: u64,
    csv_encodings: Vec<String>,
    excel_max_sheets: u64,
    excel_max_columns: u64,
    docx_max_paragraphs: u64,
    docx_max_tables: u64,
    docx_table_max_rows: u64,
    docx_table_max_cols: u64,
    pptx_max_slides: u64,
    document_excerpt_max_chars: u64,
    summary_excel_max_rows: u64,
    summary_pdf_max_pages: u64,
    summary_text_max_chars: u64,
    summary_excel_max_sheets: u64,
    summary_excel_max_columns: u64,
    summary_docx_max_paragraphs: u64,
    summary_docx_max_tables: u64,
    summary_docx_table_max_rows: u64,
    summary_docx_table_max_cols: u64,
    summary_pptx_max_slides: u64,
    summary_document_excerpt_max_chars: u64,
    max_candidate_files: u64,
    max_pdf_text_extractions: u64,
    max_total_pdf_classification_pages: u64,
    pdf_classification_timeout_ms: u64,
    total_deadline_ms: u64,
    worker_max_requests: u64,
    worker_idle_ttl_ms: u64,
    worker_rss_limit_bytes: u64,
}

impl Validate for ScannerSettings {
    fn validate(&self) -> Result<(), String> {
        if let Some(ocr) = &self.pdf_image_ocr {
            ocr.validate()?;
        }
        if let Some(paths) = &self.excerpt_priority_paths {
            validate_excerpt_priority_paths(paths)?;
        }
        if let Some(encodings) = &self.csv_encodings {
            validate_csv_encodings(encodings)?;
        }
        if let Some(values) = &self.allowed_extensions {
            validate_extensions(values, "allowed_extensions", false)?;
        }
        if let Some(values) = &self.ignored_patterns {
            validate_strings(values, "ignored_patterns", false)?;
        }
        if let Some(values) = &self.excluded_dirs {
            validate_strings(values, "excluded_dirs", false)?;
        }
        if let Some(policy) = &self.cloud_files_policy {
            require_const(policy, "local_only", "cloud_files_policy")?;
        }
        macro_rules! range {
            ($field:ident, $min:expr, $max:expr) => {
                if let Some(value) = self.$field {
                    require_range(value, $min, $max, stringify!($field))?;
                }
            };
        }
        range!(max_workers, 1, 64);
        range!(max_file_size_mb, 1, 4096);
        range!(discovery_timeout_seconds, 1, 3600);
        range!(file_timeout_seconds, 1, 3600);
        if let Some(timeouts) = &self.file_timeout_by_extension {
            if timeouts.len() > 256 {
                return Err("file_timeout_by_extension has too many entries".to_string());
            }
            for (extension, timeout) in timeouts {
                require_extension(extension, "file_timeout_by_extension key")?;
                require_range(*timeout, 1, 3600, "file_timeout_by_extension value")?;
            }
        }
        range!(total_max_chars, 1, 10_000_000);
        range!(context_max_chars, 100_000, 10_000_000);
        range!(direct_text_max_bytes, 1, 67_108_864);
        range!(direct_text_read_bytes, 1, 67_108_864);
        range!(log_tail_read_bytes, 1, 67_108_864);
        range!(text_excerpt_max_chars, 1, 10_000_000);
        range!(text_max_chars, 1, 10_000_000);
        range!(document_excerpt_max_chars, 1, 10_000_000);
        range!(summary_text_max_chars, 1, 10_000_000);
        range!(summary_document_excerpt_max_chars, 1, 10_000_000);
        range!(pdf_max_pages, 1, 10_000);
        range!(summary_pdf_max_pages, 1, 10_000);
        range!(excel_max_sheets, 1, 1024);
        range!(summary_excel_max_sheets, 1, 1024);
        range!(excel_max_rows, 1, 1_048_576);
        range!(docx_table_max_rows, 1, 1_048_576);
        range!(summary_excel_max_rows, 1, 1_048_576);
        range!(summary_docx_table_max_rows, 1, 1_048_576);
        range!(excel_max_columns, 1, 16_384);
        range!(docx_table_max_cols, 1, 16_384);
        range!(summary_excel_max_columns, 1, 16_384);
        range!(summary_docx_table_max_cols, 1, 16_384);
        range!(docx_max_paragraphs, 1, 1_000_000);
        range!(summary_docx_max_paragraphs, 1, 1_000_000);
        range!(docx_max_tables, 1, 100_000);
        range!(pptx_max_slides, 1, 100_000);
        range!(summary_docx_max_tables, 1, 100_000);
        range!(summary_pptx_max_slides, 1, 100_000);
        range!(max_candidate_files, 1, 1_000_000);
        range!(max_pdf_text_extractions, 0, 100_000);
        range!(max_total_pdf_classification_pages, 0, 10_000_000);
        range!(pdf_classification_timeout_ms, 100, 60_000);
        range!(total_deadline_ms, 5_000, 3_600_000);
        range!(worker_max_requests, 1, 10_000);
        range!(worker_idle_ttl_ms, 1_000, 600_000);
        range!(worker_rss_limit_bytes, 67_108_864, 8_589_934_592);
        Ok(())
    }
}
