use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryProfile {
    pub allowed_extensions: Vec<String>,
    pub ignored_patterns: Vec<String>,
    pub excluded_dirs: Vec<String>,
    pub follow_symlinks: bool,
    pub cloud_files_policy: String,
}

impl Validate for DiscoveryProfile {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.cloud_files_policy, "local_only", "cloud_files_policy")?;
        validate_values(
            &self.allowed_extensions,
            "allowed_extensions",
            true,
            require_extension,
        )?;
        validate_values(
            &self.ignored_patterns,
            "ignored_patterns",
            true,
            validate_label,
        )?;
        validate_values(&self.excluded_dirs, "excluded_dirs", true, validate_label)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfile {
    pub max_workers: u64,
    pub max_file_size_bytes: u64,
    pub discovery_timeout_ms: u64,
    pub file_timeout_ms: u64,
    pub file_timeout_by_extension_ms: BTreeMap<String, u64>,
}

impl Validate for ExecutionProfile {
    fn validate(&self) -> Result<(), String> {
        require_range(self.max_workers, 1, 64, "max_workers")?;
        require_range(
            self.max_file_size_bytes,
            1,
            4_294_967_296,
            "max_file_size_bytes",
        )?;
        require_range(
            self.discovery_timeout_ms,
            1000,
            3_600_000,
            "discovery_timeout_ms",
        )?;
        require_range(self.file_timeout_ms, 1000, 3_600_000, "file_timeout_ms")?;
        validate_extension_timeouts(
            &self.file_timeout_by_extension_ms,
            "file_timeout_by_extension_ms",
            1_000,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextParseProfile {
    #[serde(default = "default_csv_encodings")]
    pub csv_encodings: Vec<String>,
    pub backend: String,
    pub read_head_bytes: u64,
    pub read_tail_bytes: u64,
    pub max_chars: u64,
    pub excerpt_max_chars: u64,
}

impl Validate for TextParseProfile {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.backend, "light_text_v2", "text.backend")?;
        validate_csv_encodings(&self.csv_encodings)?;
        validate_positive_limits(&[
            ("read_head_bytes", self.read_head_bytes, 67_108_864),
            ("read_tail_bytes", self.read_tail_bytes, 67_108_864),
            ("text.max_chars", self.max_chars, 10_000_000),
            ("text.excerpt_max_chars", self.excerpt_max_chars, 10_000_000),
        ])
    }
}

fn validate_positive_limits(limits: &[(&str, u64, u64)]) -> Result<(), String> {
    for &(name, value, maximum) in limits {
        require_range(value, 1, maximum, name)?;
    }
    Ok(())
}

pub fn default_csv_encodings() -> Vec<String> {
    vec!["utf-8".into(), "utf-8-sig".into()]
}

pub(super) fn validate_csv_encodings(encodings: &[String]) -> Result<(), String> {
    let unique: std::collections::BTreeSet<_> = encodings.iter().collect();
    if encodings.is_empty()
        || encodings.len() > 3
        || unique.len() != encodings.len()
        || encodings
            .iter()
            .any(|value| !matches!(value.as_str(), "utf-8" | "utf-8-sig" | "gb18030"))
    {
        return Err(
            "csv_encodings must be a unique explicit allowlist of utf-8, utf-8-sig, gb18030".into(),
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfficeParseProfile {
    pub primary_backend: String,
    pub legacy_extensions_enabled: bool,
    pub excel_max_sheets: u64,
    pub excel_max_rows: u64,
    pub excel_max_columns: u64,
    pub docx_max_paragraphs: u64,
    pub docx_max_tables: u64,
    pub docx_table_max_rows: u64,
    pub docx_table_max_cols: u64,
    pub pptx_max_slides: u64,
    pub pptx_include_notes: bool,
    pub document_excerpt_max_chars: u64,
}

impl Validate for OfficeParseProfile {
    fn validate(&self) -> Result<(), String> {
        require_non_empty(&self.primary_backend, 1024, "primary_backend")?;
        validate_positive_limits(&[
            ("excel_max_sheets", self.excel_max_sheets, 1024),
            ("excel_max_rows", self.excel_max_rows, 1_048_576),
            ("excel_max_columns", self.excel_max_columns, 16_384),
            ("docx_max_paragraphs", self.docx_max_paragraphs, 1_000_000),
            ("docx_max_tables", self.docx_max_tables, 100_000),
            ("docx_table_max_rows", self.docx_table_max_rows, 1_048_576),
            ("docx_table_max_cols", self.docx_table_max_cols, 16_384),
            ("pptx_max_slides", self.pptx_max_slides, 100_000),
            (
                "document_excerpt_max_chars",
                self.document_excerpt_max_chars,
                10_000_000,
            ),
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfParseProfile {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub image_ocr: Option<PdfImageOcrSettings>,
    #[serde(default = "legacy_pdf_selection_policy")]
    pub selection_policy_version: String,
    pub backend: String,
    pub max_pages: u64,
    pub excerpt_max_chars: u64,
}

fn legacy_pdf_selection_policy() -> String {
    "pdf_text_presence_v1".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PdfImageOcrSettings {
    pub enabled: bool,
    /// Scan-wide quota, shared by admission and refill waves.
    pub max_files: u64,
    pub max_pages: u64,
}

impl Default for PdfImageOcrSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            max_files: 1,
            max_pages: 3,
        }
    }
}

impl Validate for PdfImageOcrSettings {
    fn validate(&self) -> Result<(), String> {
        require_range(self.max_files, 1, 32, "pdf_image_ocr.max_files")?;
        require_range(self.max_pages, 1, 96, "pdf_image_ocr.max_pages")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseProfile {
    #[serde(default = "default_cache_payload_version")]
    pub cache_payload_version: u64,
    #[serde(default = "default_excerpt_policy_version")]
    pub excerpt_policy_version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excerpt_priority_paths: Vec<String>,
    pub aggregate_max_chars: u64,
    pub text: TextParseProfile,
    pub office: OfficeParseProfile,
    pub pdf: PdfParseProfile,
}

fn default_cache_payload_version() -> u64 {
    1
}
fn default_excerpt_policy_version() -> String {
    "fair_semantic_v1".into()
}

pub(super) fn validate_excerpt_priority_paths(paths: &[String]) -> Result<(), String> {
    if paths.len() > 4096 {
        return Err("excerpt_priority_paths contains too many items".into());
    }
    for path in paths {
        require_relative_path(path, "excerpt_priority_paths")?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextProfile {
    pub global_max_chars: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedScannerSettings {
    pub report_mode: ReportMode,
    pub discovery: DiscoveryProfile,
    pub execution: ExecutionProfile,
    pub parse: ParseProfile,
    pub context: ContextProfile,
    // v2-only leaves（spec Part 8.1 表），归一化后全部必填
    pub admission_policy_version: String,
    pub classifier_policy_version: String,
    pub max_candidate_files: u64,
    pub max_pdf_text_extractions: u64,
    pub max_total_pdf_classification_pages: u64,
    pub pdf_classification_timeout_ms: u64,
    pub total_deadline_ms: u64,
    pub worker_max_requests: u64,
    pub worker_idle_ttl_ms: u64,
    pub worker_rss_limit_bytes: u64,
}
