mod captions;
mod charts;
mod compound;
mod documents;
mod legacy;
mod ooxml;
mod parts;
mod spreadsheet;
mod xlsx;

use crate::contract::{
    AuditTextBuilder, ParseRequest, ParseResult, ParsedContentAudit, ParserBackend,
    WorkerDiagnostic, OFFICE_CONTENT_MISSING_MESSAGE,
};
use crate::discovery::build_source_version;
use crate::settings::OfficeParseProfile;
use parts::{CheckedReader, PartBudget};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek};
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

pub const MAX_XLSX_FILE_BYTES: u64 = 50 * 1024 * 1024;
type OfficeContent = (String, ParsedContentAudit, bool);

struct OfficeInput<'a> {
    limits: &'a OfficeParseProfile,
    deadline: Instant,
}

struct ParseFailure {
    code: &'static str,
    message: &'static str,
    retryable: bool,
}

const INVALID_OFFICE: ParseFailure = ParseFailure {
    code: "PARSER_FAILED",
    message: "Office format or structure is invalid or exceeds the parser resource limit",
    retryable: false,
};
const TIMED_OUT: ParseFailure = ParseFailure {
    code: "PARSER_TIMEOUT",
    message: "Office parsing reached its deadline",
    retryable: true,
};
const TOO_LARGE: ParseFailure = ParseFailure {
    code: "FILE_TOO_LARGE",
    message: "file exceeds the Office size limit",
    retryable: false,
};
const SOURCE_CHANGED: ParseFailure = ParseFailure {
    code: "SOURCE_VERSION_CHANGED",
    message: "file source version changed or became unavailable",
    retryable: true,
};
const READ_FAILED: ParseFailure = ParseFailure {
    code: "PARSER_FAILED",
    message: "Office file could not be read",
    retryable: true,
};

pub fn parse(
    request: &ParseRequest<OfficeParseProfile>,
) -> Result<ParseResult, Box<WorkerDiagnostic>> {
    let started = Instant::now();
    let error = |failure: ParseFailure| {
        Box::new(WorkerDiagnostic {
            error_code: failure.code.into(),
            message: failure.message.into(),
            retryable: failure.retryable,
            stage: "parse".into(),
            file_path: Some(request.file_path.clone()),
            backend: Some(request.backend.as_str().into()),
        })
    };
    request.validate().map_err(|_| {
        error(ParseFailure {
            code: "PARSER_INVALID_PAYLOAD",
            message: "Office parse request is invalid",
            retryable: false,
        })
    })?;
    let deadline = started + Duration::from_millis(request.remaining_timeout_ms);
    let path = Path::new(&request.file_path);
    let size_limit = if request.file_type == ".xlsx" {
        request.max_file_size_bytes.min(MAX_XLSX_FILE_BYTES)
    } else {
        request.max_file_size_bytes
    };
    check_time(deadline).map_err(|_| error(TIMED_OUT))?;
    let metadata = fs::metadata(path).map_err(|_| error(READ_FAILED))?;
    if !metadata.is_file() {
        return Err(error(INVALID_OFFICE));
    }
    if metadata.len() > size_limit {
        return Err(error(TOO_LARGE));
    }
    if source_version(&metadata).as_ref().ok() != Some(&request.expected_source_version) {
        return Err(error(SOURCE_CHANGED));
    }
    check_time(deadline).map_err(|_| error(TIMED_OUT))?;
    let file = File::open(path).map_err(|_| error(READ_FAILED))?;
    let opened = file.metadata().map_err(|_| error(READ_FAILED))?;
    if !opened.is_file()
        || source_version(&opened).as_ref().ok() != Some(&request.expected_source_version)
    {
        return Err(error(SOURCE_CHANGED));
    }
    let input = OfficeInput {
        limits: &request.parser_limits,
        deadline,
    };
    let parsed =
        std::panic::catch_unwind(|| input.parse(file, opened.len())).unwrap_or(Err(INVALID_OFFICE));
    // No opaque library result is admitted after the time budget expires.
    check_time(deadline).map_err(|_| error(TIMED_OUT))?;
    let observed_source_version = fs::metadata(path)
        .and_then(|metadata| source_version(&metadata))
        .map_err(|_| error(SOURCE_CHANGED))?;
    if observed_source_version != request.expected_source_version {
        return Err(error(SOURCE_CHANGED));
    }
    check_time(deadline).map_err(|_| error(TIMED_OUT))?;
    let ((content, audit, truncated), parser_backend) = parsed.map_err(error)?;
    Ok(ParseResult {
        file_path: request.file_path.clone(),
        file_type: request.file_type.clone(),
        content,
        evidence_audit: Some(audit),
        parser_backend,
        worker_lane: parser_backend.lane(),
        truncated,
        duration_ms: started.elapsed().as_millis() as u64,
        observed_source_version,
    })
}

impl OfficeInput<'_> {
    fn spreadsheet_budget(&self, xls: bool) -> spreadsheet::XlsxBudget {
        spreadsheet::XlsxBudget {
            deadline: self.deadline,
            max_sheets: self.limits.excel_max_sheets as usize,
            max_rows: self.limits.excel_max_rows as usize + usize::from(xls),
            max_columns: self.limits.excel_max_columns as usize,
            max_chars: self.limits.document_excerpt_max_chars as usize,
        }
    }

    fn parse(&self, file: File, size: u64) -> Result<(OfficeContent, ParserBackend), ParseFailure> {
        let mut reader = CheckedReader::new(file, self.deadline);
        let mut signature = [0; 8];
        reader
            .read_exact(&mut signature)
            .map_err(|_| INVALID_OFFICE)?;
        reader.rewind().map_err(|_| READ_FAILED)?;
        let parts = PartBudget::new(self.deadline);
        if signature.starts_with(b"PK") {
            let mut zip = parts::open_package(reader, size).map_err(|_| INVALID_OFFICE)?;
            let format = parts::detect_format(&zip)?;
            if format == ".xlsx" {
                if size > MAX_XLSX_FILE_BYTES {
                    return Err(TOO_LARGE);
                }
                return xlsx::parse(parts, &mut zip, &self.spreadsheet_budget(false))
                    .map(|content| (content, ParserBackend::RustXlsxBoundedV2))
                    .map_err(|_| INVALID_OFFICE);
            }
            return documents::parse(self, parts, format, &mut zip)
                .map(|content| (content, ParserBackend::RustOfficeOxideV2))
                .map_err(|reason| {
                    let message = [
                        "Office XML event limit",
                        "Office XML token or nesting limit",
                        "Office row, cell or paragraph limit",
                        "Office text parts exceed their decompression budget",
                    ]
                    .into_iter()
                    .find(|message| *message == reason)
                    .unwrap_or(INVALID_OFFICE.message);
                    ParseFailure {
                        message,
                        ..INVALID_OFFICE
                    }
                });
        }
        if signature != [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1] {
            return Err(INVALID_OFFICE);
        }
        match legacy::read_compound_file(
            reader,
            self.deadline,
            self.limits.legacy_extensions_enabled,
        )
        .map_err(|_| INVALID_OFFICE)?
        {
            legacy::CompoundPayload::Workbook(file) => {
                legacy::parse_xls(file, parts, &self.spreadsheet_budget(true))
                    .map(|content| (content, ParserBackend::RustXlsCalamineV3))
                    .map_err(|_| INVALID_OFFICE)
            }
            legacy::CompoundPayload::Document(format, file) => {
                documents::parse_legacy(self, file, format)
                    .map(|content| (content, ParserBackend::RustLegacyOfficeV3))
                    .map_err(|_| INVALID_OFFICE)
            }
            legacy::CompoundPayload::Disabled => Err(ParseFailure {
                code: "UNSUPPORTED_EXTENSION",
                message: "Legacy Office parsing is disabled",
                retryable: false,
            }),
        }
    }

    fn builder(&self, format: &str) -> AuditTextBuilder {
        AuditTextBuilder::new(
            self.limits.document_excerpt_max_chars,
            format.trim_start_matches('.'),
        )
    }
}

fn check_time(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(())
}

fn source_version(metadata: &Metadata) -> io::Result<String> {
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok(build_source_version(modified.as_nanos(), metadata.len()))
}

fn escape_markdown_cell(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .replace('|', "\\|")
        .trim()
        .to_string()
}
