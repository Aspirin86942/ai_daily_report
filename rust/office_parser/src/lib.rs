use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const RUST_OFFICE_BACKEND: &str = "rust_office_oxide_v1";

#[derive(Debug, Deserialize)]
pub struct OfficeParseRequest {
    pub file_path: PathBuf,
    pub file_type: String,
    pub limits: BTreeMap<String, serde_json::Value>,
    pub parser_backend: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileContextOut {
    pub file_path: String,
    pub file_type: String,
    pub content: String,
    pub error: Option<String>,
    pub parser_backend: String,
    pub truncated: bool,
}

pub fn normalize_file_type(file_type: &str) -> String {
    file_type.trim().to_ascii_lowercase()
}

pub fn is_supported_office_type(file_type: &str) -> bool {
    matches!(
        normalize_file_type(file_type).as_str(),
        ".docx" | ".xlsx" | ".pptx" | ".doc" | ".xls" | ".ppt"
    )
}

pub fn positive_limit(
    limits: &BTreeMap<String, serde_json::Value>,
    key: &str,
    default_value: usize,
) -> usize {
    limits
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_value)
}

pub fn truncate_content(content: &str, max_chars: usize) -> (String, bool) {
    let max_chars = max_chars.max(1);
    let mut output = String::new();
    let mut truncated = false;
    for (index, character) in content.chars().enumerate() {
        if index >= max_chars {
            truncated = true;
            break;
        }
        output.push(character);
    }
    (output, truncated)
}

pub fn unsupported_context(request: &OfficeParseRequest) -> FileContextOut {
    let file_type = normalize_file_type(&request.file_type);
    FileContextOut {
        file_path: request.file_path.to_string_lossy().to_string(),
        file_type: file_type.clone(),
        content: String::new(),
        error: Some(format!("RUST_OFFICE_UNSUPPORTED_EXTENSION: {file_type}")),
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
        truncated: false,
    }
}

pub fn parse_office_file(request: &OfficeParseRequest) -> FileContextOut {
    let file_type = normalize_file_type(&request.file_type);
    if !is_supported_office_type(&file_type) {
        return unsupported_context(request);
    }

    let max_chars = positive_limit(&request.limits, "document_excerpt_max_chars", 6000);

    match office_oxide::Document::open(&request.file_path) {
        Ok(document) => {
            let markdown = document.to_markdown();
            let content = if markdown.trim().is_empty() {
                "No Office text extracted".to_string()
            } else {
                markdown
            };
            let (content, truncated) = truncate_content(&content, max_chars);
            FileContextOut {
                file_path: request.file_path.to_string_lossy().to_string(),
                file_type,
                content,
                error: None,
                parser_backend: RUST_OFFICE_BACKEND.to_string(),
                truncated,
            }
        }
        Err(error) => FileContextOut {
            file_path: request.file_path.to_string_lossy().to_string(),
            file_type,
            content: String::new(),
            error: Some(format!("RUST_OFFICE_PARSE_FAILED: {error}")),
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
            truncated: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_office_type_is_case_insensitive() {
        assert!(is_supported_office_type(".DOCX"));
        assert!(is_supported_office_type(".xlsx"));
        assert!(is_supported_office_type(".PPT"));
        assert!(!is_supported_office_type(".pdf"));
    }

    #[test]
    fn positive_limit_uses_default_for_missing_invalid_or_zero_values() {
        let mut limits = BTreeMap::new();
        limits.insert("good".to_string(), serde_json::json!(12));
        limits.insert("zero".to_string(), serde_json::json!(0));
        limits.insert("text".to_string(), serde_json::json!("bad"));

        assert_eq!(positive_limit(&limits, "good", 6), 12);
        assert_eq!(positive_limit(&limits, "zero", 6), 6);
        assert_eq!(positive_limit(&limits, "text", 6), 6);
        assert_eq!(positive_limit(&limits, "missing", 6), 6);
    }

    #[test]
    fn truncate_content_preserves_utf8_boundaries() {
        let (content, truncated) = truncate_content("甲乙丙丁", 3);

        assert_eq!(content, "甲乙丙");
        assert!(truncated);
    }

    #[test]
    fn unsupported_context_is_file_context_compatible() {
        let request = OfficeParseRequest {
            file_path: PathBuf::from("/tmp/report.pdf"),
            file_type: ".PDF".to_string(),
            limits: BTreeMap::new(),
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
        };

        let context = unsupported_context(&request);

        assert_eq!(context.file_path, "/tmp/report.pdf");
        assert_eq!(context.file_type, ".pdf");
        assert_eq!(
            context.error,
            Some("RUST_OFFICE_UNSUPPORTED_EXTENSION: .pdf".to_string())
        );
        assert_eq!(context.parser_backend, RUST_OFFICE_BACKEND);
        assert!(!context.truncated);
    }
}
