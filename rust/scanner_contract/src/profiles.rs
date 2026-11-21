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
        validate_extensions(&self.allowed_extensions, "allowed_extensions", true)?;
        validate_strings(&self.ignored_patterns, "ignored_patterns", true)?;
        validate_strings(&self.excluded_dirs, "excluded_dirs", true)
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
        if self.file_timeout_by_extension_ms.len() > 256 {
            return Err("file_timeout_by_extension_ms has too many entries".to_string());
        }
        for (extension, timeout) in &self.file_timeout_by_extension_ms {
            require_extension(extension, "file_timeout_by_extension_ms key")?;
            require_range(
                *timeout,
                1000,
                3_600_000,
                "file_timeout_by_extension_ms value",
            )?;
        }
        Ok(())
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
        require_range(self.read_head_bytes, 1, 67_108_864, "read_head_bytes")?;
        require_range(self.read_tail_bytes, 1, 67_108_864, "read_tail_bytes")?;
        require_range(self.max_chars, 1, 10_000_000, "text.max_chars")?;
        require_range(
            self.excerpt_max_chars,
            1,
            10_000_000,
            "text.excerpt_max_chars",
        )
    }
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
        require_range(self.excel_max_sheets, 1, 1024, "excel_max_sheets")?;
        require_range(self.excel_max_rows, 1, 1_048_576, "excel_max_rows")?;
        require_range(self.excel_max_columns, 1, 16_384, "excel_max_columns")?;
        require_range(
            self.docx_max_paragraphs,
            1,
            1_000_000,
            "docx_max_paragraphs",
        )?;
        require_range(self.docx_max_tables, 1, 100_000, "docx_max_tables")?;
        require_range(
            self.docx_table_max_rows,
            1,
            1_048_576,
            "docx_table_max_rows",
        )?;
        require_range(self.docx_table_max_cols, 1, 16_384, "docx_table_max_cols")?;
        require_range(self.pptx_max_slides, 1, 100_000, "pptx_max_slides")?;
        require_range(
            self.document_excerpt_max_chars,
            1,
            10_000_000,
            "document_excerpt_max_chars",
        )
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

impl Validate for PdfParseProfile {
    fn validate(&self) -> Result<(), String> {
        if let Some(ocr) = &self.image_ocr {
            if !ocr.enabled {
                return Err("disabled OCR must be omitted from a normalized PDF profile".into());
            }
            ocr.validate()?;
        }
        if !matches!(
            self.selection_policy_version.as_str(),
            "pdf_text_presence_v1" | "pdf_text_presence_v2"
        ) {
            return Err("unsupported PDF selection policy".into());
        }
        require_non_empty(&self.backend, 1024, "pdf.backend")?;
        require_range(self.max_pages, 1, 10_000, "pdf.max_pages")?;
        require_range(
            self.excerpt_max_chars,
            1,
            10_000_000,
            "pdf.excerpt_max_chars",
        )
    }
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

impl Validate for ParseProfile {
    fn validate(&self) -> Result<(), String> {
        if self.cache_payload_version != 1
            || !matches!(
                self.excerpt_policy_version.as_str(),
                "fair_semantic_v1"
                    | "fair_semantic_v2"
                    | "fair_semantic_v3"
                    | "fair_semantic_v4"
                    | "fair_semantic_v5"
                    | "fair_semantic_v6"
                    | "fair_semantic_v7"
                    | "fair_semantic_v8"
                    | "fair_semantic_v9"
                    | "fair_semantic_v10"
            )
        {
            return Err("parse evidence policies are unsupported".into());
        }
        validate_excerpt_priority_paths(&self.excerpt_priority_paths)?;
        require_sorted_unique(&self.excerpt_priority_paths, "excerpt_priority_paths")?;
        require_range(
            self.aggregate_max_chars,
            1,
            10_000_000,
            "aggregate_max_chars",
        )?;
        self.text.validate()?;
        self.office.validate()?;
        self.pdf.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextProfile {
    pub profile_name: String,
    pub global_max_chars: u64,
    pub per_file_max_chars: u64,
    pub small_file_max_bytes: u64,
    pub medium_file_max_bytes: u64,
    pub large_file_max_bytes: u64,
    pub priority_policy_version: String,
    pub compression_policy_version: String,
}

impl Validate for ContextProfile {
    fn validate(&self) -> Result<(), String> {
        require_range(self.global_max_chars, 1, 10_000_000, "global_max_chars")?;
        require_range(self.per_file_max_chars, 1, 10_000_000, "per_file_max_chars")?;
        if self.small_file_max_bytes != 65_536
            || self.medium_file_max_bytes != 1_048_576
            || self.large_file_max_bytes != 10_485_760
            || self.global_max_chars < self.per_file_max_chars
        {
            return Err("context thresholds or budgets violate v2".to_string());
        }
        require_const(
            &self.priority_policy_version,
            PRIORITY_POLICY_VERSION,
            "priority_policy_version",
        )?;
        require_const(
            &self.compression_policy_version,
            COMPRESSION_POLICY_VERSION,
            "compression_policy_version",
        )
    }
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

impl Validate for NormalizedScannerSettings {
    fn validate(&self) -> Result<(), String> {
        self.discovery.validate()?;
        self.execution.validate()?;
        self.parse.validate()?;
        self.context.validate()?;
        let expected = (
            self.report_mode.compression_profile().as_str(),
            CONTEXT_PER_FILE_MAX_CHARS,
        );
        let actual = (
            self.context.profile_name.as_str(),
            self.context.per_file_max_chars,
        );
        if actual != expected {
            return Err("report mode and normalized context profile do not match".to_string());
        }
        if !matches!(
            self.admission_policy_version.as_str(),
            "budget_admission_v2" | "budget_refill_v3"
        ) {
            return Err("unsupported admission_policy_version".to_string());
        }
        if !matches!(
            self.classifier_policy_version.as_str(),
            "pdf_text_presence_v1" | "pdf_text_presence_v2"
        ) {
            return Err("unsupported classifier policy".into());
        }
        macro_rules! range {
            ($field:ident, $min:expr, $max:expr) => {
                require_range(self.$field, $min, $max, stringify!($field))?;
            };
        }
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
