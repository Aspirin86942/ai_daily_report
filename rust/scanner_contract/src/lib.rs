//! Strict, versioned DTOs shared by the scanner core and its Python caller.

use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

pub trait Validate {
    fn validate(&self) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Nullable<T>(pub Option<T>);

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| de::Error::custom("explicit null is not allowed"))
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Nullable<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Nullable)
}

fn require_const(actual: &str, expected: &str, field: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{field} must equal {expected}"))
    }
}

fn require_non_empty(value: &str, max: usize, field: &str) -> Result<(), String> {
    let length = value.chars().count();
    if (1..=max).contains(&length) {
        Ok(())
    } else {
        Err(format!("{field} length must be 1..={max}"))
    }
}

fn require_range(value: u64, min: u64, max: u64, field: &str) -> Result<(), String> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(format!("{field} must be in {min}..={max}"))
    }
}

fn require_absolute_path(value: &str, field: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let drive_rooted = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let absolute = value.starts_with('/') || value.starts_with("\\\\") || drive_rooted;
    if absolute && !value.contains('\0') && value.chars().count() <= 32_767 {
        Ok(())
    } else {
        Err(format!("{field} must be an absolute path"))
    }
}

fn require_request_id(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && matches!(bytes[14], b'1'..=b'5')
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'A' | b'b' | b'B');
    if valid {
        Ok(())
    } else {
        Err("request_id must be an RFC 4122 UUID v1..v5".to_string())
    }
}

fn require_date(value: &str, field: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [4, 7].contains(&index) || byte.is_ascii_digit())
    {
        return Err(format!("{field} must use YYYY-MM-DD"));
    }
    let year: u32 = value[0..4]
        .parse()
        .map_err(|_| format!("invalid {field}"))?;
    let month: u32 = value[5..7]
        .parse()
        .map_err(|_| format!("invalid {field}"))?;
    let day: u32 = value[8..10]
        .parse()
        .map_err(|_| format!("invalid {field}"))?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if day >= 1 && day <= max_day {
        Ok(())
    } else {
        Err(format!("{field} must be a valid calendar date"))
    }
}

fn require_extension(value: &str, field: &str) -> Result<(), String> {
    let length = value.chars().count();
    let valid = (2..=32).contains(&length)
        && value.starts_with('.')
        && value.chars().skip(1).all(|character| {
            !character.is_ascii_uppercase() && !matches!(character, '\\' | '/' | ':' | '\0')
        });
    if valid {
        Ok(())
    } else {
        Err(format!("{field} must be a lowercase extension"))
    }
}

fn require_source_version(value: &str, field: &str) -> Result<(), String> {
    let Some((mtime, size)) = value
        .strip_prefix("mtime_ns=")
        .and_then(|rest| rest.split_once(":size="))
    else {
        return Err(format!("invalid {field}"));
    };
    if !mtime.is_empty()
        && !size.is_empty()
        && mtime.bytes().all(|byte| byte.is_ascii_digit())
        && size.bytes().all(|byte| byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(format!("invalid {field}"))
    }
}

fn require_relative_path(value: &str, field: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let drive_prefixed = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let rooted = value.starts_with('/') || value.starts_with('\\') || drive_prefixed;
    let escapes = value.split(['/', '\\']).any(|component| component == "..");
    if !value.is_empty() && value.chars().count() <= 32_767 && !rooted && !escapes {
        Ok(())
    } else {
        Err(format!("{field} must be a safe relative path"))
    }
}

fn require_sorted_unique(values: &[String], field: &str) -> Result<(), String> {
    if values.windows(2).all(|pair| pair[0] < pair[1]) {
        Ok(())
    } else {
        Err(format!("{field} must be sorted and unique"))
    }
}

fn require_sha256_hex(value: &str, field: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(format!("{field} must be a lowercase SHA-256"))
    }
}

fn validate_extensions(values: &[String], field: &str, require_unique: bool) -> Result<(), String> {
    if values.len() > 256 {
        return Err(format!("{field} contains too many items"));
    }
    for value in values {
        require_extension(value, field)?;
    }
    if require_unique {
        require_sorted_unique(values, field)?;
    }
    Ok(())
}

fn validate_strings(values: &[String], field: &str, require_unique: bool) -> Result<(), String> {
    if values.len() > 256 {
        return Err(format!("{field} contains too many items"));
    }
    for value in values {
        require_non_empty(value, 1024, field)?;
    }
    if require_unique {
        require_sorted_unique(values, field)?;
    }
    Ok(())
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FallbackBackend {
    #[serde(rename = "python_office_v2")]
    PythonOfficeV2,
    #[serde(rename = "python_sharepoint_text_v2")]
    PythonSharepointTextV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterPaths {
    pub office_worker_path: String,
    pub python_executable: String,
    pub python_module_root: String,
    pub python_document_worker_module: String,
}

impl Validate for AdapterPaths {
    fn validate(&self) -> Result<(), String> {
        require_absolute_path(&self.office_worker_path, "office_worker_path")?;
        require_absolute_path(&self.python_executable, "python_executable")?;
        require_absolute_path(&self.python_module_root, "python_module_root")?;
        require_non_empty(
            &self.python_document_worker_module,
            1024,
            "python_document_worker_module",
        )?;
        let mut parts = self.python_document_worker_module.split('.');
        if parts.all(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
                && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
        }) {
            Ok(())
        } else {
            Err("python_document_worker_module must be dotted identifiers".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Scanner settings constants and the strict raw settings type. All leaves stay
// optional; Rust normalization owns every default and routing policy.
// ---------------------------------------------------------------------------

pub const ADMISSION_POLICY_VERSION: &str = "budget_admission_v2";
pub const CLASSIFIER_POLICY_VERSION: &str = "pdf_text_presence_v1";
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScannerSettings {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub allowed_extensions: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub ignored_patterns: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub excluded_dirs: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub max_workers: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub max_file_size_mb: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub discovery_timeout_seconds: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub file_timeout_seconds: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub file_timeout_by_extension: Option<BTreeMap<String, u64>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub total_max_chars: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub fallback_after_timeout: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub legacy_office_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub pptx_include_notes: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub direct_text_max_bytes: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub direct_text_read_bytes: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub log_tail_read_bytes: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub text_excerpt_max_chars: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub excel_max_rows: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub pdf_max_pages: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub text_max_chars: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub excel_max_sheets: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub excel_max_columns: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub docx_max_paragraphs: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub docx_max_tables: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub docx_table_max_rows: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub docx_table_max_cols: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub pptx_max_slides: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub document_excerpt_max_chars: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_excel_max_rows: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_pdf_max_pages: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_text_max_chars: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_excel_max_sheets: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_excel_max_columns: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_docx_max_paragraphs: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_docx_max_tables: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_docx_table_max_rows: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_docx_table_max_cols: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_pptx_max_slides: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub summary_document_excerpt_max_chars: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub max_candidate_files: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub max_pdf_text_extractions: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub max_total_pdf_classification_pages: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub pdf_classification_timeout_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub total_deadline_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub worker_max_requests: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub worker_idle_ttl_ms: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub worker_rss_limit_bytes: Option<u64>,
}

impl Validate for ScannerSettings {
    fn validate(&self) -> Result<(), String> {
        if let Some(values) = &self.allowed_extensions {
            validate_extensions(values, "allowed_extensions", false)?;
        }
        if let Some(values) = &self.ignored_patterns {
            validate_strings(values, "ignored_patterns", false)?;
        }
        if let Some(values) = &self.excluded_dirs {
            validate_strings(values, "excluded_dirs", false)?;
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
        for (field, value) in [
            ("direct_text_max_bytes", self.direct_text_max_bytes),
            ("direct_text_read_bytes", self.direct_text_read_bytes),
            ("log_tail_read_bytes", self.log_tail_read_bytes),
        ] {
            if let Some(value) = value {
                require_range(value, 1, 67_108_864, field)?;
            }
        }
        for (field, value) in [
            ("text_excerpt_max_chars", self.text_excerpt_max_chars),
            ("text_max_chars", self.text_max_chars),
            (
                "document_excerpt_max_chars",
                self.document_excerpt_max_chars,
            ),
            ("summary_text_max_chars", self.summary_text_max_chars),
            (
                "summary_document_excerpt_max_chars",
                self.summary_document_excerpt_max_chars,
            ),
        ] {
            if let Some(value) = value {
                require_range(value, 1, 10_000_000, field)?;
            }
        }
        for (field, value) in [
            ("pdf_max_pages", self.pdf_max_pages),
            ("summary_pdf_max_pages", self.summary_pdf_max_pages),
        ] {
            if let Some(value) = value {
                require_range(value, 1, 10_000, field)?;
            }
        }
        for (field, value) in [
            ("excel_max_sheets", self.excel_max_sheets),
            ("summary_excel_max_sheets", self.summary_excel_max_sheets),
        ] {
            if let Some(value) = value {
                require_range(value, 1, 1024, field)?;
            }
        }
        for (field, value) in [
            ("excel_max_rows", self.excel_max_rows),
            ("docx_table_max_rows", self.docx_table_max_rows),
            ("summary_excel_max_rows", self.summary_excel_max_rows),
            (
                "summary_docx_table_max_rows",
                self.summary_docx_table_max_rows,
            ),
        ] {
            if let Some(value) = value {
                require_range(value, 1, 1_048_576, field)?;
            }
        }
        for (field, value) in [
            ("excel_max_columns", self.excel_max_columns),
            ("docx_table_max_cols", self.docx_table_max_cols),
            ("summary_excel_max_columns", self.summary_excel_max_columns),
            (
                "summary_docx_table_max_cols",
                self.summary_docx_table_max_cols,
            ),
        ] {
            if let Some(value) = value {
                require_range(value, 1, 16_384, field)?;
            }
        }
        for (field, value) in [
            ("docx_max_paragraphs", self.docx_max_paragraphs),
            (
                "summary_docx_max_paragraphs",
                self.summary_docx_max_paragraphs,
            ),
        ] {
            if let Some(value) = value {
                require_range(value, 1, 1_000_000, field)?;
            }
        }
        for (field, value) in [
            ("docx_max_tables", self.docx_max_tables),
            ("pptx_max_slides", self.pptx_max_slides),
            ("summary_docx_max_tables", self.summary_docx_max_tables),
            ("summary_pptx_max_slides", self.summary_pptx_max_slides),
        ] {
            if let Some(value) = value {
                require_range(value, 1, 100_000, field)?;
            }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryProfile {
    pub allowed_extensions: Vec<String>,
    pub ignored_patterns: Vec<String>,
    pub excluded_dirs: Vec<String>,
}

impl Validate for DiscoveryProfile {
    fn validate(&self) -> Result<(), String> {
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
    pub backend: String,
    pub read_head_bytes: u64,
    pub read_tail_bytes: u64,
    pub max_chars: u64,
    pub excerpt_max_chars: u64,
}

impl Validate for TextParseProfile {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.backend, "light_text_v2", "text.backend")?;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfficeParseProfile {
    pub primary_backend: String,
    pub fallback_enabled: bool,
    pub fallback_order: Vec<FallbackBackend>,
    pub fallback_after_timeout: bool,
    pub fallback_policy_version: String,
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
        require_non_empty(
            &self.fallback_policy_version,
            1024,
            "fallback_policy_version",
        )?;
        if self.fallback_order.len() > 2
            || self.fallback_order.iter().collect::<HashSet<_>>().len() != self.fallback_order.len()
        {
            return Err("fallback_order must be unique and contain at most two items".to_string());
        }
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
    pub backend: String,
    pub max_pages: u64,
    pub excerpt_max_chars: u64,
}

impl Validate for PdfParseProfile {
    fn validate(&self) -> Result<(), String> {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseProfile {
    pub aggregate_max_chars: u64,
    pub text: TextParseProfile,
    pub office: OfficeParseProfile,
    pub pdf: PdfParseProfile,
}

impl Validate for ParseProfile {
    fn validate(&self) -> Result<(), String> {
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
        let expected = match self.report_mode {
            ReportMode::Daily => ("daily_balanced_v1", 500_000, 100_000),
            ReportMode::Weekly => ("weekly_balanced_v1", 500_000, 100_000),
            ReportMode::Monthly => ("monthly_balanced_v1", 500_000, 100_000),
        };
        let actual = (
            self.context.profile_name.as_str(),
            self.context.global_max_chars,
            self.context.per_file_max_chars,
        );
        if actual != expected {
            return Err("report mode and normalized context profile do not match".to_string());
        }
        require_const(
            &self.admission_policy_version,
            ADMISSION_POLICY_VERSION,
            "admission_policy_version",
        )?;
        require_const(
            &self.classifier_policy_version,
            CLASSIFIER_POLICY_VERSION,
            "classifier_policy_version",
        )?;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildContextRequest {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub work_dir: String,
    pub start_date: String,
    pub end_date: String,
    pub report_mode: ReportMode,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub compression_profile: Nullable<CompressionProfile>,
    pub scan_db_path: String,
    pub scanner_settings: ScannerSettings,
    pub adapters: AdapterPaths,
}

impl Validate for BuildContextRequest {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_absolute_path(&self.work_dir, "work_dir")?;
        require_date(&self.start_date, "start_date")?;
        require_date(&self.end_date, "end_date")?;
        if self.start_date > self.end_date {
            return Err("start_date must not be after end_date".to_string());
        }
        let compression_matches = matches!(
            (self.report_mode, self.compression_profile.0),
            (_, None)
                | (ReportMode::Daily, Some(CompressionProfile::DailyBalancedV1))
                | (
                    ReportMode::Weekly,
                    Some(CompressionProfile::WeeklyBalancedV1)
                )
                | (
                    ReportMode::Monthly,
                    Some(CompressionProfile::MonthlyBalancedV1)
                )
        );
        if !compression_matches {
            return Err("compression profile does not match report mode".to_string());
        }
        require_absolute_path(&self.scan_db_path, "scan_db_path")?;
        self.scanner_settings.validate()?;
        self.adapters.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidRequest,
    ContractVersionMismatch,
    WorkDirNotFound,
    WorkDirNotDirectory,
    DiscoveryEntryUnreadable,
    FileTooLarge,
    ParserStartFailed,
    ParserTimeout,
    ParserInvalidPayload,
    ParserFailed,
    WorkerHandshakeFailed,
    WorkerVersionMismatch,
    WorkerBuildChanged,
    SourceVersionChanged,
    CacheOpenFailed,
    CacheWriteFailed,
    ScanAlreadyRunning,
    RequestInProgress,
    RequestIdConflict,
    RunNotFound,
    RunCorrupt,
    ContextBudgetInvalid,
    NotImplemented,
    RustCoreCrashed,
    StageDeadlineExhausted,
    BudgetModelMismatch,
    ContextFixedSectionsOverBudget,
    ProfileRouteInvariant,
    SourceFileLimitExceeded,
    SourceGuardUnavailable,
    ScannerDbSchemaMismatch,
    DiagnosticsAggregated,
    SnapshotReuseProjectedAsFresh,
    ParseCacheNotApplicableProjectedAsMiss,
    CacheMissReasonProjectedAsNewFile,
    SourceGuardNotProjected,
    WorkerRssUnavailable,
    InternalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStage {
    Request,
    Discovery,
    Cache,
    Parse,
    Context,
    Process,
    Doctor,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub error_code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub stage: DiagnosticStage,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub file_path: Nullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub backend: Nullable<String>,
}

impl Validate for Diagnostic {
    fn validate(&self) -> Result<(), String> {
        require_non_empty(&self.message, 4096, "diagnostic.message")?;
        if let Some(file_path) = &self.file_path.0 {
            require_absolute_path(file_path, "diagnostic.file_path")?;
        }
        if let Some(backend) = &self.backend.0 {
            require_non_empty(backend, 1024, "diagnostic.backend")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSummary {
    pub source_file_count: u64,
    pub success_count: u64,
    pub timeout_count: u64,
    pub included_file_count: u64,
    pub omitted_file_count: u64,
    pub error_file_count: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub total_duration_ms: u64,
    pub discovery_duration_ms: u64,
    pub parse_duration_ms: u64,
    pub compression_duration_ms: u64,
}

impl Validate for ContextSummary {
    fn validate(&self) -> Result<(), String> {
        let classified = self
            .success_count
            .checked_add(self.timeout_count)
            .and_then(|value| value.checked_add(self.error_file_count))
            .ok_or_else(|| "summary counts overflow".to_string())?;
        if classified <= self.source_file_count {
            Ok(())
        } else {
            Err("classified file counts exceed source_file_count".to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineStatus {
    Ok,
    Partial,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEnvelope {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub engine_version: String,
    pub engine_build: String,
    pub status: EngineStatus,
    pub file_context: String,
    pub summary: ContextSummary,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub scan_run_id: Nullable<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub context_run_id: Nullable<u64>,
    pub warnings: Vec<Diagnostic>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub error: Nullable<Diagnostic>,
}

impl Validate for ContextEnvelope {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_non_empty(&self.engine_version, 4096, "engine_version")?;
        require_non_empty(&self.engine_build, 4096, "engine_build")?;
        self.summary.validate()?;
        if self.warnings.len() > 100_000 {
            return Err("too many context warnings".to_string());
        }
        for warning in &self.warnings {
            warning.validate()?;
        }
        if let Some(error) = &self.error.0 {
            error.validate()?;
        }
        for (field, value) in [
            ("scan_run_id", self.scan_run_id.0),
            ("context_run_id", self.context_run_id.0),
        ] {
            if let Some(value) = value {
                require_range(value, 1, u64::MAX, field)?;
            }
        }
        match self.status {
            EngineStatus::Ok => {
                if self.file_context.is_empty()
                    || self.scan_run_id.0.is_none()
                    || self.context_run_id.0.is_none()
                    || self.error.0.is_some()
                {
                    Err("ok context violates status invariants".to_string())
                } else {
                    Ok(())
                }
            }
            EngineStatus::Partial => {
                if self.file_context.is_empty()
                    || self.scan_run_id.0.is_none()
                    || self.context_run_id.0.is_none()
                    || self.warnings.is_empty()
                    || self.error.0.is_some()
                {
                    Err("partial context violates status invariants".to_string())
                } else {
                    Ok(())
                }
            }
            EngineStatus::Error => {
                if !self.file_context.is_empty() || self.error.0.is_none() {
                    Err("error context violates status invariants".to_string())
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorRequest {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub scan_db_path: String,
    pub adapters: AdapterPaths,
}

impl Validate for DoctorRequest {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_absolute_path(&self.scan_db_path, "scan_db_path")?;
        self.adapters.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorCheckStatus,
    pub message: String,
}

impl Validate for DoctorCheck {
    fn validate(&self) -> Result<(), String> {
        require_non_empty(&self.name, 4096, "doctor check name")?;
        require_non_empty(&self.message, 4096, "doctor check message")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorResponse {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub status: EngineStatus,
    pub engine_version: String,
    pub engine_build: String,
    pub checks: Vec<DoctorCheck>,
    pub warnings: Vec<Diagnostic>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub error: Nullable<Diagnostic>,
}

impl Validate for DoctorResponse {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_non_empty(&self.engine_version, 4096, "engine_version")?;
        require_non_empty(&self.engine_build, 4096, "engine_build")?;
        if self.checks.len() > 256 || self.warnings.len() > 256 {
            return Err("doctor response contains too many items".to_string());
        }
        for check in &self.checks {
            check.validate()?;
        }
        for warning in &self.warnings {
            warning.validate()?;
        }
        if let Some(error) = &self.error.0 {
            error.validate()?;
        }
        match self.status {
            EngineStatus::Ok if self.error.0.is_none() => Ok(()),
            EngineStatus::Partial if self.error.0.is_none() && !self.warnings.is_empty() => Ok(()),
            EngineStatus::Error if self.error.0.is_some() => Ok(()),
            _ => Err("doctor response violates status invariants".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Success,
    Partial,
    Error,
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageName {
    Discovery,
    Cache,
    Parse,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageMetric {
    pub stage: StageName,
    pub item_count: u64,
    pub duration_ms: u64,
}

impl Validate for StageMetric {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionMetric {
    pub extension: String,
    pub file_count: u64,
    pub parse_duration_ms: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub timeout_count: u64,
}

impl Validate for ExtensionMetric {
    fn validate(&self) -> Result<(), String> {
        require_extension(&self.extension, "extension metric extension")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    Success,
    Error,
    Timeout,
    NotParsed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditWorkerLane {
    RustCore,
    RustOfficeProcessV2,
    PythonDocumentProcessV2,
    NotParsed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    Fresh,
    Miss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheMissReason {
    #[serde(rename = "")]
    None,
    #[serde(rename = "new_file")]
    NewFile,
    #[serde(rename = "error_cache")]
    ErrorCache,
    #[serde(rename = "source_version_changed")]
    SourceVersionChanged,
    #[serde(rename = "parser_profile_changed")]
    ParserProfileChanged,
    /// v2-only reason (spec Part 4): exact key absent but another identity row
    /// for the same source exists. Projected losslessly to `parser_profile_changed`
    /// in the v1 inspect (spec Part 5.3).
    #[serde(rename = "parser_identity_changed")]
    ParserIdentityChanged,
    /// v2-only reason (spec Part 4): inventory existed before this round but the
    /// exact cache entry is absent/evicted. Projected to `new_file` + warning in
    /// the v1 inspect (spec Part 5.3).
    #[serde(rename = "entry_absent_or_evicted")]
    EntryAbsentOrEvicted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAudit {
    pub relative_path: String,
    pub file_identity: String,
    pub source_version: String,
    pub parse_status: ParseStatus,
    pub parser_backend: String,
    pub worker_lane: AuditWorkerLane,
    pub cache_status: CacheStatus,
    pub cache_miss_reason: CacheMissReason,
    pub truncated: bool,
    pub content_sha256: String,
    pub parse_duration_ms: u64,
    pub failure_class: String,
    pub fallback_backend: String,
    pub fallback_reason_code: String,
}

impl Validate for FileAudit {
    fn validate(&self) -> Result<(), String> {
        require_relative_path(&self.relative_path, "file relative_path")?;
        require_non_empty(&self.file_identity, 4096, "file_identity")?;
        require_source_version(&self.source_version, "source_version")?;
        require_non_empty(&self.parser_backend, 4096, "parser_backend")?;
        if self.content_sha256.len() != 64
            || !self
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("content_sha256 must be lowercase SHA-256".to_string());
        }
        for (field, value) in [
            ("failure_class", self.failure_class.as_str()),
            ("fallback_backend", self.fallback_backend.as_str()),
            ("fallback_reason_code", self.fallback_reason_code.as_str()),
        ] {
            if value.chars().count() > 1024 {
                return Err(format!("{field} is too long"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAction {
    Keep,
    Compress,
    MetadataOnly,
    Omit,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDecision {
    pub relative_path: String,
    pub action: ContextAction,
    pub reason: String,
    pub priority: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub truncated: bool,
    pub error_code: String,
}

impl Validate for ContextDecision {
    fn validate(&self) -> Result<(), String> {
        require_relative_path(&self.relative_path, "decision relative_path")?;
        require_non_empty(&self.reason, 4096, "decision reason")?;
        if self.error_code.chars().count() <= 1024 {
            Ok(())
        } else {
            Err("decision error_code is too long".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Complete scanner evidence and worker contracts.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheRetentionPolicy {
    pub policy_version: String,
    pub parse_cache_max_bytes: u64,
    pub classification_cache_max_bytes: u64,
    pub context_artifacts_max_bytes: u64,
    pub terminal_audit_max_bytes: u64,
    pub terminal_run_max_count: u64,
    pub terminal_run_max_age_days: u64,
    pub opportunistic_gc_budget_ms: u64,
}

impl Validate for CacheRetentionPolicy {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.policy_version, "cache_retention_v1", "policy_version")?;
        require_range(
            self.parse_cache_max_bytes,
            1,
            u64::MAX,
            "parse_cache_max_bytes",
        )?;
        require_range(
            self.classification_cache_max_bytes,
            1,
            u64::MAX,
            "classification_cache_max_bytes",
        )?;
        require_range(
            self.context_artifacts_max_bytes,
            1,
            u64::MAX,
            "context_artifacts_max_bytes",
        )?;
        require_range(
            self.terminal_audit_max_bytes,
            1,
            u64::MAX,
            "terminal_audit_max_bytes",
        )?;
        require_range(
            self.terminal_run_max_count,
            1,
            u64::MAX,
            "terminal_run_max_count",
        )?;
        require_range(
            self.terminal_run_max_age_days,
            1,
            u64::MAX,
            "terminal_run_max_age_days",
        )?;
        require_range(
            self.opportunistic_gc_budget_ms,
            1,
            u64::MAX,
            "opportunistic_gc_budget_ms",
        )
    }
}

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
    pub status: PdfClassificationStatus,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub page_count: Nullable<u64>,
    pub classification_cache_status: ClassificationCacheStatus,
    pub classification_cache_miss_reason: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub result_examined_pages: Nullable<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub run_inspected_pages: Nullable<u64>,
    pub nominal_charged_pages: u64,
    pub duration_ms: u64,
    pub transport: ClassificationTransport,
    pub attempt_count: u64,
    pub classifier_build: String,
    pub classifier_profile_hash: String,
}

impl Validate for PdfClassificationAuditV1 {
    fn validate(&self) -> Result<(), String> {
        require_sha256_hex(&self.classifier_build, "classifier_build")?;
        require_sha256_hex(&self.classifier_profile_hash, "classifier_profile_hash")?;
        require_range(self.attempt_count, 0, 3, "attempt_count")?;
        if self.classification_cache_status == ClassificationCacheStatus::Miss {
            if !matches!(
                self.classification_cache_miss_reason.as_str(),
                "new_file"
                    | "source_version_changed"
                    | "classifier_identity_changed"
                    | "entry_absent_or_evicted"
            ) {
                return Err(
                    "classification cache miss reason is not in the v2 allowlist".to_string(),
                );
            }
        } else if !self.classification_cache_miss_reason.is_empty() {
            return Err("non-miss classification cache must have an empty miss reason".to_string());
        }
        let zero_execution = |audit: &Self, transport: ClassificationTransport| {
            if audit.run_inspected_pages.0 != Some(0)
                || audit.duration_ms != 0
                || audit.transport != transport
                || audit.attempt_count != 0
            {
                Err(
                    "non-executing classification provenance has nonzero execution fields"
                        .to_string(),
                )
            } else {
                Ok(())
            }
        };
        match self.status {
            PdfClassificationStatus::TextInParseWindow
            | PdfClassificationStatus::NoTextInParseWindow => {
                let page_count = self.page_count.0.ok_or_else(|| {
                    "text/no-text classification requires page provenance".to_string()
                })?;
                let result_pages = self.result_examined_pages.0.ok_or_else(|| {
                    "text/no-text classification requires examined-page provenance".to_string()
                })?;
                if page_count == 0 {
                    return Err(
                        "text/no-text classification page_count must be positive".to_string()
                    );
                }
                if self.nominal_charged_pages == 0 {
                    return Err(
                        "classified PDF requires a positive nominal page charge".to_string()
                    );
                }
                let window_pages = page_count.min(self.nominal_charged_pages);
                match self.status {
                    PdfClassificationStatus::TextInParseWindow
                        if !(1..=window_pages).contains(&result_pages) =>
                    {
                        return Err(
                            "text classification pages must fit the parse window".to_string()
                        );
                    }
                    PdfClassificationStatus::NoTextInParseWindow
                        if result_pages != window_pages =>
                    {
                        return Err(
                            "no-text classification must examine the complete window".to_string()
                        );
                    }
                    _ => {}
                }
                match self.classification_cache_status {
                    ClassificationCacheStatus::Fresh => {
                        zero_execution(self, ClassificationTransport::NotApplicable)?
                    }
                    ClassificationCacheStatus::Snapshot => {
                        zero_execution(self, ClassificationTransport::Snapshot)?
                    }
                    ClassificationCacheStatus::Miss => {
                        if !matches!(
                            self.transport,
                            ClassificationTransport::Session | ClassificationTransport::OneShot
                        ) || self.attempt_count == 0
                        {
                            return Err(
                                "executed classification miss requires session/one_shot and 1..3 attempts"
                                    .to_string(),
                            );
                        }
                    }
                    ClassificationCacheStatus::NotEligible => {
                        return Err(
                            "text/no-text classification cannot be not_eligible".to_string()
                        );
                    }
                }
            }
            PdfClassificationStatus::Unknown | PdfClassificationStatus::Error => {
                if self.classification_cache_status != ClassificationCacheStatus::Miss
                    || !matches!(
                        self.transport,
                        ClassificationTransport::Session | ClassificationTransport::OneShot
                    )
                    || self.attempt_count == 0
                    || self.nominal_charged_pages == 0
                {
                    return Err(
                        "unknown/error classification requires an executed miss with a nominal charge"
                            .to_string(),
                    );
                }
                if let Some(result_pages) = self.result_examined_pages.0 {
                    if result_pages > self.nominal_charged_pages {
                        return Err(
                            "typed failure result pages exceed the nominal page charge".to_string()
                        );
                    }
                    if self.page_count.0.is_some_and(|page_count| {
                        result_pages > page_count.min(self.nominal_charged_pages)
                    }) {
                        return Err(
                            "typed failure result pages exceed the parse window".to_string()
                        );
                    }
                }
            }
            PdfClassificationStatus::NotClassifiedByBudget => {
                if self.page_count.0.is_some()
                    || self.result_examined_pages.0 != Some(0)
                    || self.nominal_charged_pages != 0
                    || self.classification_cache_status != ClassificationCacheStatus::NotEligible
                {
                    return Err(
                        "not_classified_by_budget must carry the frozen not_eligible zero-page shape"
                            .to_string(),
                    );
                }
                zero_execution(self, ClassificationTransport::NotApplicable)?;
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

/// Shared full-v2 parser provenance matrix used by the wire validator and the
/// store's pre-persistence seam. Keeping it here prevents the two gates from
/// accepting different backend/lane combinations.
pub fn validate_v2_parse_provenance(
    parse_status: ParseStatus,
    parser_backend: &str,
    worker_lane: AuditWorkerLane,
    parse_cache_status: ParseCacheStatus,
    classification_status: Option<PdfClassificationStatus>,
) -> Result<(), String> {
    let metadata = parser_backend == "pdf_metadata_v2" && worker_lane == AuditWorkerLane::RustCore;
    let not_parsed = parser_backend == "not_parsed" && worker_lane == AuditWorkerLane::NotParsed;
    let body_parser = matches!(
        (parser_backend, worker_lane),
        ("light_text_v2", AuditWorkerLane::RustCore)
            | ("rust_office_oxide_v2", AuditWorkerLane::RustOfficeProcessV2)
            | ("rust_xlsx_bounded_v2", AuditWorkerLane::RustOfficeProcessV2)
            | (
                "python_pdf_text_v2",
                AuditWorkerLane::PythonDocumentProcessV2
            )
            | ("python_office_v2", AuditWorkerLane::PythonDocumentProcessV2)
            | (
                "python_sharepoint_text_v2",
                AuditWorkerLane::PythonDocumentProcessV2
            )
    );
    if !(metadata || not_parsed || body_parser) {
        return Err("parser backend and worker lane are not a frozen v2 route".to_string());
    }

    match classification_status {
        Some(PdfClassificationStatus::NoTextInParseWindow)
            if !((parse_status == ParseStatus::Success && metadata)
                || (parse_status == ParseStatus::NotParsed && not_parsed)) =>
        {
            return Err(
                "no-text classification requires metadata-only success or budget-omitted provenance"
                    .to_string(),
            );
        }
        Some(status @ (PdfClassificationStatus::Unknown | PdfClassificationStatus::Error)) => {
            let status_matches = if status == PdfClassificationStatus::Unknown {
                matches!(parse_status, ParseStatus::Error | ParseStatus::Timeout)
            } else {
                parse_status == ParseStatus::Error
            };
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub final_diagnostic: Nullable<Diagnostic>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pdf_classification: Nullable<PdfClassificationAuditV1>,
}

impl Validate for FileAuditV2 {
    fn validate(&self) -> Result<(), String> {
        require_relative_path(&self.relative_path, "file relative_path")?;
        require_non_empty(&self.file_identity, 4096, "file_identity")?;
        require_source_version(&self.source_version, "source_version")?;
        require_non_empty(&self.parser_backend, 4096, "parser_backend")?;
        require_sha256_hex(&self.content_sha256, "content_sha256")?;
        match self.source_guard_kind {
            SourceGuardKind::Unavailable => {
                if self.source_guard_sha256.0.is_some() {
                    return Err("unavailable source guard must have a null hash".to_string());
                }
            }
            _ => {
                let hash = self
                    .source_guard_sha256
                    .0
                    .as_deref()
                    .ok_or_else(|| "source guard kind requires a sha256".to_string())?;
                require_sha256_hex(hash, "source_guard_sha256")?;
            }
        }
        if self.parse_cache_status == ParseCacheStatus::Miss {
            if !matches!(
                self.cache_miss_reason.as_str(),
                "new_file"
                    | "source_version_changed"
                    | "parser_identity_changed"
                    | "entry_absent_or_evicted"
            ) {
                return Err("parse cache miss reason is not in the v2 allowlist".to_string());
            }
        } else if !self.cache_miss_reason.is_empty() {
            return Err("non-miss parse cache must have an empty miss reason".to_string());
        }
        require_range(self.parse_attempt_count, 0, 3, "parse_attempt_count")?;
        match self.parse_cache_status {
            ParseCacheStatus::Fresh => {
                if self.parse_status != ParseStatus::Success
                    || self.parse_transport != ParseTransport::NotApplicable
                    || self.parse_attempt_count != 0
                    || self.parse_duration_ms != 0
                {
                    return Err(
                        "fresh parse-cache provenance must be Success with zero execution"
                            .to_string(),
                    );
                }
            }
            ParseCacheStatus::Snapshot => {
                if matches!(self.parse_status, ParseStatus::Error | ParseStatus::Timeout)
                    || self.parse_transport != ParseTransport::Snapshot
                    || self.parse_attempt_count != 0
                    || self.parse_duration_ms != 0
                {
                    return Err(
                        "snapshot parse provenance must have zero execution and no Error/Timeout"
                            .to_string(),
                    );
                }
            }
            ParseCacheStatus::Miss => {
                if !matches!(
                    self.parse_status,
                    ParseStatus::Success | ParseStatus::Error | ParseStatus::Timeout
                ) || !matches!(
                    self.parse_transport,
                    ParseTransport::Session
                        | ParseTransport::OneShot
                        | ParseTransport::RustInProcess
                ) || self.parse_attempt_count == 0
                {
                    return Err(
                        "parse miss requires a started parser transport and 1..3 attempts"
                            .to_string(),
                    );
                }
            }
            ParseCacheStatus::NotApplicable => {
                if self.parse_transport != ParseTransport::NotApplicable
                    || self.parse_attempt_count != 0
                    || self.parse_duration_ms != 0
                {
                    return Err(
                        "not_applicable parse provenance must have zero execution".to_string()
                    );
                }
            }
        }
        match self.parse_status {
            ParseStatus::Error | ParseStatus::Timeout => {
                if self.final_diagnostic.0.is_none() {
                    return Err("error/timeout file audit requires a final diagnostic".to_string());
                }
            }
            ParseStatus::Success | ParseStatus::NotParsed => {
                if self.final_diagnostic.0.is_some() {
                    return Err(
                        "success/not_parsed file audit must not carry a final diagnostic"
                            .to_string(),
                    );
                }
            }
        }
        if let Some(diagnostic) = &self.final_diagnostic.0 {
            diagnostic.validate()?;
        }
        if let Some(classification) = &self.pdf_classification.0 {
            classification.validate()?;
        }
        validate_v2_parse_provenance(
            self.parse_status,
            &self.parser_backend,
            self.worker_lane,
            self.parse_cache_status,
            self.pdf_classification
                .0
                .as_ref()
                .map(|classification| classification.status),
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReuseKind {
    ContextSnapshot,
    ParseCache,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionMetricsV2 {
    pub discovery_observed_file_count: u64,
    pub source_guard_content_hash_file_count: u64,
    pub source_guard_unavailable_count: u64,
    pub source_guard_bytes_read: u64,
    pub candidate_file_count: u64,
    pub admitted_file_count: u64,
    pub classification_slot_count: u64,
    pub confirmed_run_inspected_pages_total: u64,
    pub unobserved_classification_attempt_count: u64,
    pub nominal_charged_pages_total: u64,
    pub extraction_slot_count: u64,
    pub pdfplumber_invocations: u64,
    pub snapshot_hit: bool,
    pub parse_cache_lookup_count: u64,
    pub classification_cache_lookup_count: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parse_cache_all_hit: Nullable<bool>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub classification_cache_all_hit: Nullable<bool>,
    pub stage_deadline_exhausted_count: u64,
    pub session_restart_count: u64,
    pub session_fallback_count: u64,
    pub classify_attempt_count: u64,
    pub parse_attempt_count: u64,
    pub reserved_chars: u64,
    pub rendered_chars: u64,
    pub worker_handshake_ms: u64,
    pub discovery_ms: u64,
    pub snapshot_lookup_ms: u64,
    pub current_run_audit_write_ms: u64,
    pub terminal_precommit_ms: u64,
    pub deadline_precommit_elapsed_ms: u64,
    pub envelope_rebuild_ms: u64,
    pub terminal_rows_written: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub peak_worker_rss_bytes: Nullable<u64>,
}

impl Validate for ExecutionMetricsV2 {
    fn validate(&self) -> Result<(), String> {
        require_range(
            self.stage_deadline_exhausted_count,
            0,
            1,
            "stage_deadline_exhausted_count",
        )?;
        if self.parse_cache_lookup_count == 0 && self.parse_cache_all_hit.0.is_some() {
            return Err(
                "parse_cache_all_hit must be null when no parse lookup occurred".to_string(),
            );
        }
        if self.parse_cache_lookup_count > 0 && self.parse_cache_all_hit.0.is_none() {
            return Err("parse_cache_all_hit is required after a parse lookup".to_string());
        }
        if self.classification_cache_lookup_count == 0
            && self.classification_cache_all_hit.0.is_some()
        {
            return Err(
                "classification_cache_all_hit must be null when no classification lookup occurred"
                    .to_string(),
            );
        }
        if self.classification_cache_lookup_count > 0
            && self.classification_cache_all_hit.0.is_none()
        {
            return Err(
                "classification_cache_all_hit is required after a classification lookup"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScannerEvidence {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub scan_run_id: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub context_run_id: Nullable<u64>,
    pub run_status: RunStatus,
    pub summary: ContextSummary,
    pub stage_metrics: Vec<StageMetric>,
    pub extension_metrics: Vec<ExtensionMetric>,
    pub files: Vec<FileAuditV2>,
    pub decisions: Vec<ContextDecision>,
    pub warnings: Vec<Diagnostic>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub artifact_id: Nullable<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reused_from_context_run_id: Nullable<u64>,
    pub reuse_kind: ReuseKind,
    pub execution_metrics: ExecutionMetricsV2,
}

impl Validate for ScannerEvidence {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_range(self.scan_run_id, 1, u64::MAX, "scan_run_id")?;
        if let Some(context_run_id) = self.context_run_id.0 {
            require_range(context_run_id, 1, u64::MAX, "context_run_id")?;
        }
        if let Some(artifact_id) = self.artifact_id.0 {
            require_range(artifact_id, 1, u64::MAX, "artifact_id")?;
        }
        if let Some(reused_from) = self.reused_from_context_run_id.0 {
            require_range(reused_from, 1, u64::MAX, "reused_from_context_run_id")?;
        }
        self.summary.validate()?;
        if self.stage_metrics.len() > 32
            || self.extension_metrics.len() > 256
            || self.files.len() > 1_000_000
            || self.decisions.len() > 1_000_000
            || self.warnings.len() > 100_000
        {
            return Err("scanner evidence contains too many items".to_string());
        }
        for metric in &self.stage_metrics {
            metric.validate()?;
        }
        for metric in &self.extension_metrics {
            metric.validate()?;
        }
        for file in &self.files {
            file.validate()?;
        }
        for decision in &self.decisions {
            decision.validate()?;
        }
        for warning in &self.warnings {
            warning.validate()?;
        }
        self.execution_metrics.validate()?;
        match self.run_status {
            RunStatus::Success | RunStatus::Partial if self.artifact_id.0.is_none() => {
                return Err("successful scanner evidence requires artifact_id".to_string());
            }
            RunStatus::Error if self.artifact_id.0.is_some() => {
                return Err("error scanner evidence must have a null artifact_id".to_string());
            }
            _ => {}
        }
        if self.reuse_kind == ReuseKind::ContextSnapshot
            && self.reused_from_context_run_id.0.is_none()
        {
            return Err("context_snapshot reuse requires reused_from_context_run_id".to_string());
        }
        if self.reuse_kind == ReuseKind::ContextSnapshot && !self.execution_metrics.snapshot_hit {
            return Err("context_snapshot reuse requires snapshot_hit".to_string());
        }
        if self.reuse_kind != ReuseKind::ContextSnapshot
            && self.reused_from_context_run_id.0.is_some()
        {
            return Err(
                "reused_from_context_run_id is only allowed for context_snapshot reuse".to_string(),
            );
        }
        if self.reuse_kind == ReuseKind::ParseCache
            && (self.execution_metrics.parse_cache_lookup_count == 0
                || self.execution_metrics.parse_cache_all_hit.0 != Some(true)
                || self.execution_metrics.snapshot_hit)
        {
            return Err(
                "parse_cache reuse requires a parse lookup all-hit and no snapshot".to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelatedPayload {
    pub role: String,
    pub schema: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
enum ContractPayload {
    BuildContextRequest(Box<BuildContextRequest>),
    ContextEnvelope(ContextEnvelope),
    Diagnostic(Diagnostic),
    DoctorRequest(DoctorRequest),
    DoctorResponse(DoctorResponse),
    NormalizedScannerSettings(Box<NormalizedScannerSettings>),
    RawScannerSettings(Box<ScannerSettings>),
}

fn parse_typed<T>(value: &Value) -> Result<T, String>
where
    T: DeserializeOwned + Validate,
{
    let parsed: T = serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    parsed.validate()?;
    Ok(parsed)
}

fn parse_contract_payload(schema: &str, value: &Value) -> Result<ContractPayload, String> {
    match schema {
        "build-context-request-v1.schema.json" => Ok(ContractPayload::BuildContextRequest(
            Box::new(parse_typed(value)?),
        )),
        "context-envelope-v1.schema.json" => {
            Ok(ContractPayload::ContextEnvelope(parse_typed(value)?))
        }
        "diagnostic-v1.schema.json" => Ok(ContractPayload::Diagnostic(parse_typed(value)?)),
        "doctor-request-v1.schema.json" => Ok(ContractPayload::DoctorRequest(parse_typed(value)?)),
        "doctor-response-v1.schema.json" => {
            Ok(ContractPayload::DoctorResponse(parse_typed(value)?))
        }
        "normalized-scanner-settings.schema.json" => Ok(
            ContractPayload::NormalizedScannerSettings(Box::new(parse_typed(value)?)),
        ),
        "scanner-settings.schema.json" => Ok(ContractPayload::RawScannerSettings(Box::new(
            parse_typed(value)?,
        ))),
        _ => Err(format!("unknown scanner contract schema: {schema}")),
    }
}

impl ContractPayload {
    fn to_value(&self) -> Result<Value, String> {
        macro_rules! serialize {
            ($payload:expr) => {
                serde_json::to_value($payload).map_err(|error| error.to_string())
            };
        }
        match self {
            Self::BuildContextRequest(payload) => serialize!(payload),
            Self::ContextEnvelope(payload) => serialize!(payload),
            Self::Diagnostic(payload) => serialize!(payload),
            Self::DoctorRequest(payload) => serialize!(payload),
            Self::DoctorResponse(payload) => serialize!(payload),
            Self::NormalizedScannerSettings(payload) => serialize!(payload),
            Self::RawScannerSettings(payload) => serialize!(payload),
        }
    }
}

/// Parse one strict DTO, apply relational checks, and return its typed round-trip JSON.
pub fn validate_contract_payload(
    schema: &str,
    value: &Value,
    related_payloads: &[RelatedPayload],
) -> Result<Value, String> {
    let parsed = parse_contract_payload(schema, value)?;
    let mut request = None;
    for related in related_payloads {
        let related_parsed = parse_contract_payload(&related.schema, &related.payload)?;
        match related.role.as_str() {
            "request" => request = Some(related_parsed),
            _ => return Err(format!("unknown related payload role: {}", related.role)),
        }
    }

    if let (
        ContractPayload::ContextEnvelope(response),
        Some(ContractPayload::BuildContextRequest(request)),
    ) = (&parsed, &request)
    {
        if response.request_id != request.request_id {
            return Err("context response request_id mismatch".to_string());
        }
    }

    parsed.to_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Deserialize)]
    struct FixtureManifest {
        valid_fixtures: Vec<FixtureEntry>,
    }

    #[derive(Debug, Deserialize)]
    struct FixtureEntry {
        file: String,
        schema: String,
    }

    #[derive(Debug, Deserialize)]
    struct InvalidCorpus {
        cases: Vec<InvalidCase>,
    }

    #[derive(Debug, Deserialize)]
    struct InvalidCase {
        name: String,
        schema: String,
        payload: Value,
        #[serde(default)]
        related_payloads: Vec<RelatedPayload>,
    }

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("scanner_contract")
            .join("v1")
    }

    fn read_json<T: DeserializeOwned>(path: &Path) -> T {
        let text = fs::read_to_string(path).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn every_golden_fixture_round_trips_through_typed_dtos() {
        let fixture_dir = fixture_dir();
        let manifest: FixtureManifest = read_json(&fixture_dir.join("fixture-manifest.json"));
        for entry in manifest.valid_fixtures {
            let value: Value = read_json(&fixture_dir.join(&entry.file));
            let round_trip = validate_contract_payload(&entry.schema, &value, &[])
                .unwrap_or_else(|error| panic!("{}: {error}", entry.file));
            assert_eq!(round_trip, value, "{}", entry.file);
        }
    }

    #[test]
    fn every_invalid_fixture_is_rejected() {
        let fixture_dir = fixture_dir();
        let corpus: InvalidCorpus = read_json(&fixture_dir.join("invalid-cases.json"));
        for case in corpus.cases {
            let result =
                validate_contract_payload(&case.schema, &case.payload, &case.related_payloads);
            assert!(result.is_err(), "{} unexpectedly passed", case.name);
        }
    }

    #[test]
    fn absolute_paths_reject_embedded_nul() {
        assert!(require_absolute_path("C:\\evidence\0hidden.xlsx", "file_path").is_err());
    }

    #[test]
    fn file_audit_v2_fail_closes_non_miss_row_with_non_empty_miss_reason() {
        // spec Part 5.3: cache_miss_reason is only valid for parse_cache_status=miss.
        // A not_applicable row with a non-empty reason (real-corpus RUN_CORRUPT
        // regression) must fail validation.
        fn audit(cache_miss_reason: &str) -> FileAuditV2 {
            FileAuditV2 {
                relative_path: "evidence.txt".to_string(),
                file_identity: "file-a".to_string(),
                source_version: "mtime_ns=100:size=5".to_string(),
                source_guard_kind: SourceGuardKind::Unavailable,
                source_guard_sha256: Nullable(None),
                parse_status: ParseStatus::NotParsed,
                parser_backend: "not_parsed".to_string(),
                worker_lane: AuditWorkerLane::NotParsed,
                parse_cache_status: ParseCacheStatus::NotApplicable,
                cache_miss_reason: cache_miss_reason.to_string(),
                truncated: false,
                content_sha256: "0".repeat(64),
                parse_duration_ms: 0,
                failure_class: String::new(),
                fallback_backend: String::new(),
                fallback_reason_code: String::new(),
                parse_transport: ParseTransport::NotApplicable,
                parse_attempt_count: 0,
                final_diagnostic: Nullable(None),
                pdf_classification: Nullable(None),
            }
        }
        assert!(
            audit("new_file").validate().is_err(),
            "not_applicable row with a non-empty miss reason must fail validation"
        );
        assert!(
            audit("").validate().is_ok(),
            "not_applicable row with an empty miss reason must validate"
        );
        // The same holds for fresh/snapshot rows.
        for status in [ParseCacheStatus::Fresh, ParseCacheStatus::Snapshot] {
            let mut row = audit("");
            row.parse_cache_status = status;
            row.cache_miss_reason = "new_file".to_string();
            assert!(row.validate().is_err(), "{status:?} with reason must fail");
        }
    }
}
