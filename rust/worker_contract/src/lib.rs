//! Shared v2 envelope for crash-isolated scanner workers.
//!
//! The interface is one hello frame followed by request/response NDJSON pairs.
//! Request ids, status, diagnostics, and worker identity live only in the
//! envelope; operation payloads contain domain data and never embed a second
//! transport protocol.

use serde::{Deserialize, Serialize};

pub const CONTRACT: &str = "ai_daily_worker";
pub const CONTRACT_VERSION: &str = "ai_daily_worker_v2";
pub const PROTOCOL_VERSION: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    Office,
    PythonDocument,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOperation {
    OfficeParse,
    PdfClassify,
    PdfParse,
    PythonOfficeParse,
    PythonSharepointParse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerHello {
    pub contract: String,
    pub protocol_version: u64,
    pub frame: String,
    pub worker_contract_version: String,
    pub worker_kind: WorkerKind,
    pub worker_version: String,
    pub worker_build: String,
    pub supported_operations: Vec<WorkerOperation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub operation: WorkerOperation,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerResponseStatus {
    Ok,
    Error,
}

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
    #[serde(rename = "rust_office_oxide_v2")]
    RustOfficeOxideV2,
    #[serde(rename = "rust_xlsx_bounded_v2")]
    RustXlsxBoundedV2,
    #[serde(rename = "python_office_v2")]
    PythonOfficeV2,
    #[serde(rename = "python_pdf_text_v2")]
    PythonPdfTextV2,
    #[serde(rename = "python_sharepoint_text_v2")]
    PythonSharepointTextV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLane {
    RustOfficeProcessV2,
    PythonDocumentProcessV2,
}

impl ParserBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RustOfficeOxideV2 => "rust_office_oxide_v2",
            Self::RustXlsxBoundedV2 => "rust_xlsx_bounded_v2",
            Self::PythonOfficeV2 => "python_office_v2",
            Self::PythonPdfTextV2 => "python_pdf_text_v2",
            Self::PythonSharepointTextV2 => "python_sharepoint_text_v2",
        }
    }

    pub fn supports(self, extension: &str) -> bool {
        match self {
            Self::RustOfficeOxideV2 => matches!(extension, ".docx" | ".pptx"),
            Self::RustXlsxBoundedV2 => extension == ".xlsx",
            Self::PythonOfficeV2 => matches!(extension, ".docx" | ".pptx" | ".xls" | ".xlsx"),
            Self::PythonPdfTextV2 => extension == ".pdf",
            Self::PythonSharepointTextV2 => matches!(extension, ".doc" | ".ppt"),
        }
    }

    pub const fn lane(self) -> WorkerLane {
        match self {
            Self::RustOfficeOxideV2 | Self::RustXlsxBoundedV2 => WorkerLane::RustOfficeProcessV2,
            _ => WorkerLane::PythonDocumentProcessV2,
        }
    }

    const fn limit_kind(self) -> &'static str {
        match self {
            Self::RustOfficeOxideV2 | Self::RustXlsxBoundedV2 | Self::PythonOfficeV2 => "office",
            Self::PythonPdfTextV2 => "pdf",
            Self::PythonSharepointTextV2 => "sharepoint_text",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum ParserLimits {
    #[serde(rename = "office")]
    Office {
        excel_max_sheets: u64,
        excel_max_rows: u64,
        excel_max_columns: u64,
        docx_max_paragraphs: u64,
        docx_max_tables: u64,
        docx_table_max_rows: u64,
        docx_table_max_cols: u64,
        pptx_max_slides: u64,
        pptx_include_notes: bool,
        document_excerpt_max_chars: u64,
    },
    #[serde(rename = "pdf")]
    Pdf {
        max_pages: u64,
        excerpt_max_chars: u64,
    },
    #[serde(rename = "sharepoint_text")]
    SharepointText { excerpt_max_chars: u64 },
}

impl ParserLimits {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Office { .. } => "office",
            Self::Pdf { .. } => "pdf",
            Self::SharepointText { .. } => "sharepoint_text",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Office {
                excel_max_sheets,
                excel_max_rows,
                excel_max_columns,
                docx_max_paragraphs,
                docx_max_tables,
                docx_table_max_rows,
                docx_table_max_cols,
                pptx_max_slides,
                document_excerpt_max_chars,
                ..
            } => {
                validate_range(*excel_max_sheets, 1, 1024, "excel_max_sheets")?;
                validate_range(*excel_max_rows, 1, 1_048_576, "excel_max_rows")?;
                validate_range(*excel_max_columns, 1, 16_384, "excel_max_columns")?;
                validate_range(*docx_max_paragraphs, 1, 1_000_000, "docx_max_paragraphs")?;
                validate_range(*docx_max_tables, 1, 100_000, "docx_max_tables")?;
                validate_range(*docx_table_max_rows, 1, 1_048_576, "docx_table_max_rows")?;
                validate_range(*docx_table_max_cols, 1, 16_384, "docx_table_max_cols")?;
                validate_range(*pptx_max_slides, 1, 100_000, "pptx_max_slides")?;
                validate_range(
                    *document_excerpt_max_chars,
                    1,
                    10_000_000,
                    "document_excerpt_max_chars",
                )
            }
            Self::Pdf {
                max_pages,
                excerpt_max_chars,
            } => {
                validate_range(*max_pages, 1, 10_000, "max_pages")?;
                validate_range(*excerpt_max_chars, 1, 10_000_000, "excerpt_max_chars")
            }
            Self::SharepointText { excerpt_max_chars } => {
                validate_range(*excerpt_max_chars, 1, 10_000_000, "excerpt_max_chars")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseRequest {
    pub file_path: String,
    pub file_type: String,
    pub backend: ParserBackend,
    pub remaining_timeout_ms: u64,
    pub max_file_size_bytes: u64,
    pub parser_limits: ParserLimits,
    pub expected_source_version: String,
}

impl ParseRequest {
    pub fn validate(&self) -> Result<(), String> {
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
        self.parser_limits.validate()?;
        validate_source_version(&self.expected_source_version)?;
        if self.backend.supports(&self.file_type)
            && self.backend.limit_kind() == self.parser_limits.kind()
        {
            Ok(())
        } else {
            Err("parse request route is inconsistent".to_string())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseResult {
    pub file_path: String,
    pub file_type: String,
    pub content: String,
    pub parser_backend: ParserBackend,
    pub worker_lane: WorkerLane,
    pub truncated: bool,
    pub duration_ms: u64,
    pub observed_source_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifyRequest {
    pub file_path: String,
    pub source_version: String,
    pub max_pages: u64,
    pub policy_version: String,
}

impl ClassifyRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_absolute_path(&self.file_path, "file_path")?;
        validate_source_version(&self.source_version)?;
        validate_range(self.max_pages, 1, 10_000, "max_pages")?;
        if self.policy_version == "pdf_text_presence_v1" {
            Ok(())
        } else {
            Err("classifier policy version mismatch".to_string())
        }
    }
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
    pub status: ClassifyStatus,
    pub page_count: Option<u64>,
    pub result_examined_pages: Option<u64>,
    pub diagnostic: Option<WorkerDiagnostic>,
}

impl ClassifyResult {
    pub fn validate(&self) -> Result<(), String> {
        match self.status {
            ClassifyStatus::TextInParseWindow | ClassifyStatus::NoTextInParseWindow => {
                if self.diagnostic.is_some() {
                    return Err("text/no-text result must not carry a diagnostic".to_string());
                }
                let Some(page_count) = self.page_count else {
                    return Err("text/no-text result requires page counts".to_string());
                };
                let Some(result_pages) = self.result_examined_pages else {
                    return Err("text/no-text result requires page counts".to_string());
                };
                if page_count == 0 || result_pages == 0 || result_pages > page_count {
                    return Err("text/no-text result page counts are inconsistent".to_string());
                }
            }
            ClassifyStatus::Unknown | ClassifyStatus::Error => {
                let diagnostic = self
                    .diagnostic
                    .as_ref()
                    .ok_or_else(|| "unknown/error result requires a diagnostic".to_string())?;
                if diagnostic.retryable != (self.status == ClassifyStatus::Unknown) {
                    return Err("unknown must be retryable and error must not be".to_string());
                }
            }
        }
        Ok(())
    }

    pub fn validate_for_max_pages(&self, max_pages: u64) -> Result<(), String> {
        self.validate()?;
        validate_range(max_pages, 1, 10_000, "max_pages")?;
        match self.status {
            ClassifyStatus::TextInParseWindow | ClassifyStatus::NoTextInParseWindow => {
                let page_count = self.page_count.expect("validated above");
                let result_pages = self.result_examined_pages.expect("validated above");
                let window_pages = page_count.min(max_pages);
                match self.status {
                    ClassifyStatus::TextInParseWindow
                        if !(1..=window_pages).contains(&result_pages) =>
                    {
                        Err("text result pages exceed the request parse window".to_string())
                    }
                    ClassifyStatus::NoTextInParseWindow if result_pages != window_pages => {
                        Err("no-text result did not inspect the complete request window"
                            .to_string())
                    }
                    _ => Ok(()),
                }
            }
            ClassifyStatus::Unknown | ClassifyStatus::Error => {
                if self.result_examined_pages.is_some_and(|result_pages| {
                    result_pages > max_pages
                        || self
                            .page_count
                            .is_some_and(|page_count| result_pages > page_count.min(max_pages))
                }) {
                    Err("typed failure result pages exceed the request parse window".to_string())
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl ParseResult {
    pub fn validate(&self) -> Result<(), String> {
        validate_absolute_path(&self.file_path, "file_path")?;
        validate_extension(&self.file_type, "file_type")?;
        validate_source_version(&self.observed_source_version)?;
        if self.parser_backend.supports(&self.file_type)
            && self.parser_backend.lane() == self.worker_lane
        {
            Ok(())
        } else {
            Err("parse result route is inconsistent".to_string())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResponse {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub operation: WorkerOperation,
    pub status: WorkerResponseStatus,
    pub result: Option<serde_json::Value>,
    pub error: Option<WorkerDiagnostic>,
}

impl WorkerHello {
    pub fn validate(&self) -> Result<(), String> {
        validate_common(&self.contract, self.protocol_version)?;
        if self.frame != "hello" {
            return Err("worker hello frame must be hello".to_string());
        }
        if self.worker_contract_version != CONTRACT_VERSION {
            return Err("worker contract version mismatch".to_string());
        }
        if self.worker_version.is_empty() || self.worker_version.len() > 1024 {
            return Err("worker version is invalid".to_string());
        }
        if self.worker_build.is_empty() || self.worker_build.len() > 1024 {
            return Err("worker build identity is invalid".to_string());
        }
        if self.supported_operations.is_empty() {
            return Err("worker must support at least one operation".to_string());
        }
        let mut operations = self.supported_operations.clone();
        operations.sort_by_key(|operation| *operation as u8);
        operations.dedup();
        if operations.len() != self.supported_operations.len() {
            return Err("worker operations must be unique".to_string());
        }
        Ok(())
    }
}

impl WorkerRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_common(&self.contract, self.protocol_version)?;
        validate_request_id(&self.request_id)
    }
}

impl WorkerResponse {
    pub fn validate(&self) -> Result<(), String> {
        validate_common(&self.contract, self.protocol_version)?;
        validate_request_id(&self.request_id)?;
        match self.status {
            WorkerResponseStatus::Ok if self.result.is_some() && self.error.is_none() => Ok(()),
            WorkerResponseStatus::Error if self.result.is_none() && self.error.is_some() => Ok(()),
            _ => Err("worker response status/result/error mismatch".to_string()),
        }
    }
}

fn validate_common(contract: &str, protocol_version: u64) -> Result<(), String> {
    if contract != CONTRACT {
        return Err("worker contract mismatch".to_string());
    }
    if protocol_version != PROTOCOL_VERSION {
        return Err("worker protocol version mismatch".to_string());
    }
    Ok(())
}

fn validate_request_id(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || ![8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| ![8, 13, 18, 23].contains(&index) && !byte.is_ascii_hexdigit())
    {
        return Err("request_id must be a canonical UUID".to_string());
    }
    Ok(())
}

fn validate_range(value: u64, min: u64, max: u64, field: &str) -> Result<(), String> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(format!("{field} is outside the allowed range"))
    }
}

fn validate_absolute_path(value: &str, field: &str) -> Result<(), String> {
    if value.len() <= 32_767
        && (value.starts_with("\\\\")
            || value
                .as_bytes()
                .get(1)
                .is_some_and(|separator| *separator == b':'))
    {
        Ok(())
    } else {
        Err(format!("{field} must be an absolute Windows path"))
    }
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

fn validate_source_version(value: &str) -> Result<(), String> {
    let Some((modified, size)) = value
        .strip_prefix("mtime_ns=")
        .and_then(|rest| rest.split_once(":size="))
    else {
        return Err("expected_source_version is invalid".to_string());
    };
    if !modified.is_empty()
        && !size.is_empty()
        && modified.bytes().all(|byte| byte.is_ascii_digit())
        && size.bytes().all(|byte| byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err("expected_source_version is invalid".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_requires_v2_identity_and_unique_operations() {
        let hello = WorkerHello {
            contract: CONTRACT.to_string(),
            protocol_version: PROTOCOL_VERSION,
            frame: "hello".to_string(),
            worker_contract_version: CONTRACT_VERSION.to_string(),
            worker_kind: WorkerKind::Office,
            worker_version: "0.1.0".to_string(),
            worker_build: "a".repeat(64),
            supported_operations: vec![WorkerOperation::OfficeParse],
        };
        assert_eq!(hello.validate(), Ok(()));
    }

    #[test]
    fn response_is_a_strict_ok_or_error_union() {
        let response = WorkerResponse {
            contract: CONTRACT.to_string(),
            protocol_version: PROTOCOL_VERSION,
            request_id: "61111111-6111-4111-8111-611111111111".to_string(),
            operation: WorkerOperation::PdfParse,
            status: WorkerResponseStatus::Ok,
            result: Some(serde_json::json!({"content": "ok"})),
            error: None,
        };
        assert_eq!(response.validate(), Ok(()));
    }
}
