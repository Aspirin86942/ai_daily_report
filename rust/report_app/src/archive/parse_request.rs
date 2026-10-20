//! Output-affecting archive request construction is part of parser identity.
use super::error;
use crate::{config::AppConfig, ReportError};
use ai_daily_worker_contract::{
    archive::ArchiveTextRequest, ParseRequest, ParserBackend, ParserLimits,
};
use std::{
    fs,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

pub(super) fn text(
    config: &AppConfig,
    object: &Path,
    extension: &str,
    hash: &str,
    output_dir: &Path,
    timeout: Duration,
) -> ArchiveTextRequest {
    ArchiveTextRequest {
        file_path: object.to_owned(),
        file_type: extension.to_owned(),
        raw_sha256: hash.to_owned(),
        output_dir: output_dir.to_owned(),
        max_input_bytes: config.archive.max_input_mib * 1024 * 1024,
        max_output_bytes: config.archive.max_output_mib * 1024 * 1024,
        timeout_ms: timeout.as_millis() as u64,
        csv_encodings: config
            .scanner
            .csv_encodings
            .clone()
            .unwrap_or_else(|| vec!["utf-8".into(), "utf-8-sig".into(), "gb18030".into()]),
    }
}

pub(super) fn source(
    config: &AppConfig,
    object: &std::path::Path,
    extension: &str,
    timeout: Duration,
) -> Result<ParseRequest, ReportError> {
    let metadata = fs::metadata(object)?;
    let version = ai_daily_discovery::build_source_version(
        metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| error("ARCHIVE_MTIME_INVALID"))?
            .as_nanos(),
        metadata.len(),
    );
    let backend = match extension {
        ".pdf" => ParserBackend::RustPdfiumV3,
        ".xlsx" => ParserBackend::RustXlsxBoundedV2,
        ".xls" => ParserBackend::RustXlsCalamineV3,
        ".doc" | ".ppt" => ParserBackend::RustLegacyOfficeV3,
        _ => ParserBackend::RustOfficeOxideV2,
    };
    let limits = if extension == ".pdf" {
        ParserLimits::Pdf {
            max_pages: 10000,
            excerpt_max_chars: 10000000,
            selected_pages: None,
        }
    } else {
        ParserLimits::Office {
            legacy_office_enabled: config.scanner.legacy_office_enabled.unwrap_or(false),
            excel_max_sheets: 1024,
            excel_max_rows: 1048576,
            excel_max_columns: 16384,
            docx_max_paragraphs: 1000000,
            docx_max_tables: 100000,
            docx_table_max_rows: 1048576,
            docx_table_max_cols: 16384,
            pptx_max_slides: 100000,
            pptx_include_notes: true,
            document_excerpt_max_chars: 10000000,
        }
    };
    Ok(ParseRequest {
        file_path: object.to_string_lossy().into_owned(),
        file_type: extension.to_owned(),
        backend,
        remaining_timeout_ms: timeout.as_millis() as u64,
        max_file_size_bytes: config.archive.max_input_mib * 1024 * 1024,
        parser_limits: limits,
        expected_source_version: version,
    })
}
