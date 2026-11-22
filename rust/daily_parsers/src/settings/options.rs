use super::profiles::{validate_csv_encodings, validate_excerpt_priority_paths};
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportMode {
    Daily,
    Weekly,
    Monthly,
}

// ---------------------------------------------------------------------------
// Scanner settings constants and the strict raw settings type. All leaves stay
// optional; Rust normalization owns every default and routing policy.
// ---------------------------------------------------------------------------

pub const ADMISSION_POLICY_VERSION: &str = "budget_refill_v3";
pub const CLASSIFIER_POLICY_VERSION: &str = "pdf_text_presence_v2";

// Every override has the same wire contract: missing uses the profile default,
// explicit null is invalid, and an absent setting is omitted when serializing.
macro_rules! scanner_settings {
    ($($field:ident: $value:ty $(=> [$min:literal, $max:literal])?),* $(,)?) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct ScannerSettings {
            $(
                #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_non_null")]
                pub $field: Option<$value>,
            )*
        }

        impl ScannerSettings {
            pub(super) fn validate_limits(&self) -> Result<(), String> {
                $($(
                    if let Some(value) = self.$field {
                        require_range(value, $min, $max, stringify!($field))?;
                    }
                )?)*
                Ok(())
            }
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
    max_workers: u64 => [1, 64],
    max_file_size_mb: u64 => [1, 4096],
    discovery_timeout_seconds: u64 => [1, 3600],
    file_timeout_seconds: u64 => [1, 3600],
    file_timeout_by_extension: BTreeMap<String, u64>,
    total_max_chars: u64 => [1, 10_000_000],
    context_max_chars: u64 => [100_000, 10_000_000],
    excerpt_priority_paths: Vec<String>,
    legacy_office_enabled: bool,
    pptx_include_notes: bool,
    direct_text_max_bytes: u64 => [1, 67_108_864],
    direct_text_read_bytes: u64 => [1, 67_108_864],
    log_tail_read_bytes: u64 => [1, 67_108_864],
    text_excerpt_max_chars: u64 => [1, 10_000_000],
    excel_max_rows: u64 => [1, 1_048_576],
    pdf_max_pages: u64 => [1, 10_000],
    text_max_chars: u64 => [1, 10_000_000],
    csv_encodings: Vec<String>,
    excel_max_sheets: u64 => [1, 1024],
    excel_max_columns: u64 => [1, 16_384],
    docx_max_paragraphs: u64 => [1, 1_000_000],
    docx_max_tables: u64 => [1, 100_000],
    docx_table_max_rows: u64 => [1, 1_048_576],
    docx_table_max_cols: u64 => [1, 16_384],
    pptx_max_slides: u64 => [1, 100_000],
    document_excerpt_max_chars: u64 => [1, 10_000_000],
    summary_excel_max_rows: u64 => [1, 1_048_576],
    summary_pdf_max_pages: u64 => [1, 10_000],
    summary_text_max_chars: u64 => [1, 10_000_000],
    summary_excel_max_sheets: u64 => [1, 1024],
    summary_excel_max_columns: u64 => [1, 16_384],
    summary_docx_max_paragraphs: u64 => [1, 1_000_000],
    summary_docx_max_tables: u64 => [1, 100_000],
    summary_docx_table_max_rows: u64 => [1, 1_048_576],
    summary_docx_table_max_cols: u64 => [1, 16_384],
    summary_pptx_max_slides: u64 => [1, 100_000],
    summary_document_excerpt_max_chars: u64 => [1, 10_000_000],
    max_candidate_files: u64 => [1, 1_000_000],
    max_pdf_text_extractions: u64 => [0, 100_000],
    max_total_pdf_classification_pages: u64 => [0, 10_000_000],
    pdf_classification_timeout_ms: u64 => [100, 60_000],
    total_deadline_ms: u64 => [5_000, 3_600_000],
    worker_max_requests: u64 => [1, 10_000],
    worker_idle_ttl_ms: u64 => [1_000, 600_000],
    worker_rss_limit_bytes: u64 => [67_108_864, 8_589_934_592],
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
            validate_values(values, "allowed_extensions", false, require_extension)?;
        }
        if let Some(values) = &self.ignored_patterns {
            validate_values(values, "ignored_patterns", false, validate_label)?;
        }
        if let Some(values) = &self.excluded_dirs {
            validate_values(values, "excluded_dirs", false, validate_label)?;
        }
        if let Some(policy) = &self.cloud_files_policy {
            require_const(policy, "local_only", "cloud_files_policy")?;
        }
        if let Some(timeouts) = &self.file_timeout_by_extension {
            validate_extension_timeouts(timeouts, "file_timeout_by_extension", 1)?;
        }
        self.validate_limits()
    }
}
