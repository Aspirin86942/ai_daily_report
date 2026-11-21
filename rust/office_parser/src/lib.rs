use ai_daily_discovery::build_source_version;
use ai_daily_worker_contract::archive::{ArchiveArtifact, ArchiveRequest};
use ai_daily_worker_contract::{
    AuditTextBuilder, ParseRequest, ParseResult, ParsedContentAudit, ParserBackend, ParserLimits,
    WorkerDiagnostic, WorkerHello, WorkerKind, WorkerLane, WorkerOperation,
    CONTRACT as WORKER_CONTRACT, CONTRACT_VERSION as WORKER_CONTRACT_VERSION,
    PROTOCOL_VERSION as WORKER_PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};
use zip::read::ZipArchive;

mod captions;
mod charts;
mod documents;
mod legacy;
mod parts;
mod spreadsheet;
mod xlsx;

#[cfg(test)]
mod tests;

pub const RUST_OFFICE_BACKEND: &str = "rust_office_oxide_v2";
pub const RUST_XLSX_BOUNDED_BACKEND: &str = "rust_xlsx_bounded_v2";
pub fn worker_hello() -> WorkerHello {
    WorkerHello {
        contract: WORKER_CONTRACT.to_string(),
        protocol_version: WORKER_PROTOCOL_VERSION,
        frame: "hello".to_string(),
        worker_contract_version: WORKER_CONTRACT_VERSION.to_string(),
        worker_kind: WorkerKind::Office,
        worker_version: env!("CARGO_PKG_VERSION").to_string(),
        worker_build: env!("AI_DAILY_OFFICE_WORKER_BUILD").to_string(),
        supported_operations: vec![
            WorkerOperation::OfficeParse,
            WorkerOperation::ArchiveOfficeParse,
            WorkerOperation::ArchiveTextParse,
        ],
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_audit: Option<ParsedContentAudit>,
    pub error: Option<String>,
    pub parser_backend: String,
    pub truncated: bool,
}

pub fn normalize_file_type(file_type: &str) -> String {
    file_type.trim().to_ascii_lowercase()
}

pub fn is_supported_office_type(file_type: &str) -> bool {
    compatibility_backend(&normalize_file_type(file_type)).is_some()
}

fn compatibility_backend(format: &str) -> Option<&'static str> {
    match format {
        ".xlsx" => Some(RUST_XLSX_BOUNDED_BACKEND),
        ".xls" => Some(ParserBackend::RustXlsCalamineV3.as_str()),
        ".docx" | ".pptx" | ".doc" | ".ppt" => Some(RUST_OFFICE_BACKEND),
        _ => None,
    }
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
    match content.char_indices().nth(max_chars.max(1)) {
        Some((end, _)) => (content[..end].to_owned(), true),
        None => (content.to_owned(), false),
    }
}

pub fn unsupported_context(request: &OfficeParseRequest) -> FileContextOut {
    let file_type = normalize_file_type(&request.file_type);
    FileContextOut {
        evidence_audit: None,
        file_path: request.file_path.to_string_lossy().to_string(),
        file_type: file_type.clone(),
        content: String::new(),
        error: Some(format!("RUST_OFFICE_UNSUPPORTED_EXTENSION: {file_type}")),
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
        truncated: false,
    }
}

type OfficeContent = (String, ParsedContentAudit, bool);
type OfficeResult = Result<OfficeContent, (String, bool)>;

struct OfficeInput<'a> {
    path: &'a Path,
    format: &'a str,
    limits: &'a ParserLimits,
    timeout_ms: u64,
    max_chars: usize,
    archival: bool,
    output: Option<&'a ArchiveRequest>,
}

impl OfficeInput<'_> {
    fn builder(&self) -> AuditTextBuilder {
        let format = self.format.trim_start_matches('.');
        match self.output {
            Some(output) => {
                AuditTextBuilder::for_archive(&output.output_dir, output.max_output_bytes, format)
                    .unwrap_or_else(|_| AuditTextBuilder::new(0, format))
            }
            None => AuditTextBuilder::new(self.max_chars as u64, format),
        }
    }

    fn parse(&self) -> OfficeResult {
        let parts = parts::PartBudget::new(self.timeout_ms);
        if matches!(self.format, ".xlsx" | ".xls") {
            let ParserLimits::Office {
                excel_max_sheets,
                excel_max_rows,
                excel_max_columns,
                ..
            } = *self.limits
            else {
                unreachable!("validated Office limits")
            };
            let xls = self.format == ".xls";
            let budget = spreadsheet::XlsxBudget {
                deadline: parts.deadline,
                max_sheets: excel_max_sheets as usize,
                // XLS counts its header in addition to the configured data rows.
                max_rows: (excel_max_rows as usize).saturating_add(usize::from(xls)),
                max_columns: excel_max_columns as usize,
                max_chars: self.max_chars,
            };
            return if xls {
                legacy::parse_xls(self, parts, &budget)
                    .map_err(|_| ("RUST_XLS_PARSE_FAILED".into(), false))
            } else {
                xlsx::parse(self, parts, &budget)
            };
        }
        documents::parse(self, parts).map_err(|error| {
            (
                format!("RUST_OFFICE_PARSE_FAILED: {error}"),
                matches!(self.format, ".docx" | ".pptx"),
            )
        })
    }
}

pub fn parse_office_file(request: &OfficeParseRequest) -> FileContextOut {
    let format = normalize_file_type(&request.file_type);
    let Some(backend) = compatibility_backend(&format) else {
        return unsupported_context(request);
    };
    let limit = |key, default| positive_limit(&request.limits, key, default) as u64;
    let xls = format == ".xls";
    let max_chars = limit("document_excerpt_max_chars", 6000);
    let limits = ParserLimits::Office {
        legacy_office_enabled: true,
        excel_max_sheets: limit("excel_max_sheets", if xls { 5 } else { 2 }),
        excel_max_rows: limit("excel_max_rows", if xls { 50 } else { 10 }),
        excel_max_columns: limit("excel_max_columns", 12),
        docx_max_paragraphs: limit("docx_max_paragraphs", 200),
        docx_max_tables: limit("docx_max_tables", 20),
        docx_table_max_rows: limit("docx_table_max_rows", 50),
        docx_table_max_cols: limit("docx_table_max_cols", 12),
        pptx_max_slides: limit("pptx_max_slides", 50),
        pptx_include_notes: request
            .limits
            .get("pptx_include_notes")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        document_excerpt_max_chars: max_chars,
    };
    let output = request.limits.get("archive_output");
    // A present but malformed legacy value still selects archive traversal with a bounded excerpt.
    let archive = output.and_then(|value| serde_json::from_value(value.clone()).ok());
    let parsed = OfficeInput {
        path: &request.file_path,
        format: &format,
        limits: &limits,
        timeout_ms: limit("remaining_timeout_ms", 30_000),
        max_chars: max_chars as usize,
        archival: output.is_some(),
        output: archive.as_ref(),
    }
    .parse();
    let (content, evidence_audit, truncated, error) = match parsed {
        Ok((text, audit, truncated)) => (text, Some(audit), truncated, None),
        Err((message, _)) => (String::new(), None, false, Some(message)),
    };
    FileContextOut {
        file_path: request.file_path.to_string_lossy().into_owned(),
        file_type: if xls {
            request.file_type.clone()
        } else {
            format
        },
        content,
        evidence_audit,
        truncated,
        error,
        parser_backend: backend.into(),
    }
}

pub fn parse_worker_request(request: &ParseRequest) -> Result<ParseResult, Box<WorkerDiagnostic>> {
    parse_request_inner(request, None)
}

pub fn parse_archive_request(
    request: &ArchiveRequest,
) -> Result<ArchiveArtifact, Box<WorkerDiagnostic>> {
    request.validate().map_err(|message| {
        worker_error(&request.source, "PARSER_INVALID_PAYLOAD", message, false)
    })?;
    let parsed = parse_request_inner(&request.source, Some(request))?;
    let mut artifact = parsed
        .evidence_audit
        .and_then(|audit| audit.archive)
        .ok_or_else(|| {
            worker_error(
                &request.source,
                "ARCHIVE_OUTPUT_FAILED",
                "archive output could not be created",
                true,
            )
        })?;
    if artifact.error.is_some() {
        return Err(worker_error(
            &request.source,
            "ARCHIVE_OUTPUT_FAILED",
            "archive output could not be persisted",
            true,
        ));
    }
    artifact.truncated |= parsed.truncated;
    if artifact.truncated && artifact.gaps.is_empty() {
        artifact.gaps.push("PARSER_LIMIT".into());
    }
    if artifact.usable_chars == 0 {
        artifact.gaps.push("NO_USABLE_TEXT".into());
    }
    artifact.parser_backend = parsed.parser_backend.as_str().into();
    artifact.worker_lane = "rust_office_archive_process_v4".into();
    artifact.raw_sha256 = request.raw_sha256.clone();
    artifact.duration_ms = parsed.duration_ms;
    if let Ok(file) = std::fs::File::open(&request.source.file_path) {
        if let Ok(package) = zip::ZipArchive::new(file) {
            for name in package.file_names() {
                let gap = if name.contains("/media/") {
                    Some("OFFICE_IMAGE_CONTENT_NOT_EXTRACTED")
                } else if name.contains("/embeddings/") {
                    Some("OFFICE_EMBEDDED_CONTENT_NOT_EXTRACTED")
                } else if matches!(
                    name,
                    "word/footnotes.xml" | "word/endnotes.xml" | "word/comments.xml"
                ) {
                    Some("DOCX_ANNOTATIONS_NOT_EXTRACTED")
                } else {
                    None
                };
                if let Some(gap) = gap {
                    if !artifact.gaps.iter().any(|v| v == gap) {
                        artifact.gaps.push(gap.into());
                    }
                }
            }
        }
    }
    Ok(artifact)
}

fn parse_request_inner(
    request: &ParseRequest,
    archive: Option<&ArchiveRequest>,
) -> Result<ParseResult, Box<WorkerDiagnostic>> {
    let started_at = Instant::now();
    request
        .validate()
        .map_err(|message| worker_error(request, "PARSER_INVALID_PAYLOAD", message, false))?;
    let ParserLimits::Office {
        document_excerpt_max_chars,
        legacy_office_enabled,
        ..
    } = request.parser_limits
    else {
        return Err(worker_error(
            request,
            "PARSER_INVALID_PAYLOAD",
            "backend is not supported by the Office worker",
            false,
        ));
    };
    let path = Path::new(&request.file_path);
    let metadata = std::fs::metadata(path).map_err(|e| {
        worker_error(
            request,
            "PARSER_FAILED",
            format!("file metadata is unavailable: {e}"),
            false,
        )
    })?;
    let observed_before = metadata_source_version(&metadata).map_err(|e| {
        worker_error(
            request,
            "PARSER_FAILED",
            format!("file source version is unavailable: {e}"),
            false,
        )
    })?;
    if metadata.len() > request.max_file_size_bytes {
        return Err(worker_error(
            request,
            "FILE_TOO_LARGE",
            "file exceeds the configured size limit",
            false,
        ));
    }
    if observed_before != request.expected_source_version {
        return Err(worker_error(
            request,
            "SOURCE_VERSION_CHANGED",
            "file source version changed before parsing",
            true,
        ));
    }
    let (format, backend) = legacy::detect_office_format(path).map_err(|reason| {
        let message = match reason {
            legacy::OfficeFormatError::MissingContent => {
                ai_daily_worker_contract::OFFICE_CONTENT_MISSING_MESSAGE
            }
            legacy::OfficeFormatError::Invalid => "Office format is invalid",
        };
        worker_error(request, "PARSER_FAILED", message, false)
    })?;
    if !legacy_office_enabled && matches!(format, ".doc" | ".ppt") {
        return Err(worker_error(
            request,
            "UNSUPPORTED_EXTENSION",
            "Legacy Office parsing is disabled",
            false,
        ));
    }
    let timeout_ms = if archive.is_some() {
        request.remaining_timeout_ms
    } else {
        request
            .remaining_timeout_ms
            .saturating_sub(elapsed_ms(started_at))
            .max(1)
    };
    let input = OfficeInput {
        path,
        format,
        limits: &request.parser_limits,
        timeout_ms,
        max_chars: archive.map_or(document_excerpt_max_chars, |a| a.max_output_bytes) as usize,
        archival: archive.is_some(),
        output: archive,
    };
    let parsed = if backend == ParserBackend::RustOfficeOxideV2 {
        validate_ooxml_zip(path).and_then(|()| input.parse())
    } else {
        input.parse()
    };
    let observed_after = std::fs::metadata(path)
        .and_then(|m| metadata_source_version(&m))
        .map_err(|_| {
            worker_error(
                request,
                "SOURCE_VERSION_CHANGED",
                "file source version became unavailable during parsing",
                true,
            )
        })?;
    if observed_after != observed_before {
        return Err(worker_error(
            request,
            "SOURCE_VERSION_CHANGED",
            "file source version changed during parsing",
            true,
        ));
    }
    let (content, audit, truncated) = parsed.map_err(|(_, retryable)| {
        worker_error(
            request,
            "PARSER_FAILED",
            "Office parser reported an error",
            retryable,
        )
    })?;
    Ok(ParseResult {
        file_path: request.file_path.clone(),
        file_type: request.file_type.clone(),
        content,
        evidence_audit: Some(audit),
        parser_backend: backend,
        worker_lane: WorkerLane::RustOfficeProcessV3,
        truncated,
        duration_ms: elapsed_ms(started_at),
        observed_source_version: observed_after,
    })
}

fn validate_ooxml_zip(file_path: &Path) -> Result<(), (String, bool)> {
    let file = File::open(file_path)
        .map_err(|error| (format!("RUST_OFFICE_OPEN_FAILED: {error}"), true))?;
    ZipArchive::new(file).map(|_| ()).map_err(|error| {
        let retryable = matches!(error, zip::result::ZipError::Io(_));
        (format!("RUST_OFFICE_CORRUPT_ZIP: {error}"), retryable)
    })
}

fn worker_error(
    request: &ParseRequest,
    error_code: &str,
    message: impl AsRef<str>,
    retryable: bool,
) -> Box<WorkerDiagnostic> {
    let message: String = message.as_ref().chars().take(4096).collect();
    Box::new(WorkerDiagnostic {
        error_code: error_code.to_string(),
        message: if message.is_empty() {
            "Office worker parse failed".to_string()
        } else {
            message
        },
        retryable,
        stage: "parse".to_string(),
        file_path: Some(request.file_path.clone()),
        backend: Some(request.backend.as_str().to_string()),
    })
}

fn metadata_source_version(metadata: &std::fs::Metadata) -> std::io::Result<String> {
    let modified = metadata.modified()?;
    let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("modified time is before unix epoch: {error}"),
        )
    })?;
    Ok(build_source_version(duration.as_nanos(), metadata.len()))
}

fn elapsed_ms(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn escape_markdown_cell(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .replace('|', "\\|")
        .trim()
        .to_string()
}
