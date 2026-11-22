//! Framed requests and source identity for the isolated PDF worker.

use crate::{
    contract::{
        ClassifyResult, ClassifyStatus, ParseRequest, ParseResult, ParserBackend, ParserLimits,
        SourceRangeUnit, WorkerDiagnostic, OCR_RANGE_LABEL,
    },
    settings::{pdf_sample_pages, require_range, validate_pdf_policy, SourceGuardKind},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::LazyLock,
};

pub const PDF_PROTOCOL_VERSION: u16 = 1;
pub const MAX_REQUEST_FRAME: usize = 512 * 1024;
const MAX_RESPONSE_FRAME: usize = 64 * 1024 * 1024;
const HELLO_FRAME_LIMIT: usize = 16 * 1024;
const MAX_CONTENT_AUDIT_BYTES: usize = 256 * 1024;
const SOURCE_GUARD_DOMAIN: &[u8] = b"source-guard-v2\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfSourceGuard {
    pub kind: SourceGuardKind,
    pub sha256: String,
}

impl PdfSourceGuard {
    fn valid(&self) -> bool {
        self.kind != SourceGuardKind::Unavailable
            && self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectAndExtract {
    pub parse: ParseRequest,
    pub inspect_max_pages: u64,
    pub policy_version: String,
    pub extract: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspect_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_source_guard: Option<PdfSourceGuard>,
}

impl InspectAndExtract {
    pub fn new(
        parse: ParseRequest,
        inspect_max_pages: u64,
        policy_version: String,
        extract: bool,
    ) -> Self {
        Self {
            parse,
            inspect_max_pages,
            policy_version,
            extract,
            inspect_timeout_ms: None,
            expected_source_guard: None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        self.parse.validate()?;
        require_range(self.inspect_max_pages, 1, 10_000, "inspect_max_pages")?;
        validate_pdf_policy(&self.policy_version)?;
        if !std::path::Path::new(&self.parse.file_path).is_absolute()
            || self
                .inspect_timeout_ms
                .is_some_and(|ms| !(1..=3_600_000).contains(&ms))
            || self
                .expected_source_guard
                .as_ref()
                .is_some_and(|guard| !guard.valid())
        {
            return Err("invalid PDF request route or source identity".into());
        }
        Ok(())
    }

    pub fn inspection_pages(&self, page_count: u64) -> Vec<u64> {
        pdf_sample_pages(page_count, self.inspect_max_pages, &self.policy_version)
    }

    pub fn extraction_pages(&self, page_count: u64, first_text_page: Option<u64>) -> Vec<u64> {
        let (limit, selected) = match &self.parse.parser_limits {
            ParserLimits::Pdf {
                max_pages,
                selected_pages,
                ..
            } => (*max_pages, selected_pages.as_deref()),
            ParserLimits::PdfImageOcr {
                max_pages,
                selected_pages,
                ..
            } => (*max_pages, Some(selected_pages.as_slice())),
        };
        crate::settings::pdf_extraction_pages(
            page_count,
            limit,
            first_text_page,
            selected,
            &self.policy_version,
        )
    }

    pub fn can_extract(&self, status: ClassifyStatus) -> bool {
        self.extract
            && match self.parse.parser_limits {
                ParserLimits::Pdf { .. } => status == ClassifyStatus::TextInParseWindow,
                ParserLimits::PdfImageOcr { .. } => status == ClassifyStatus::NoTextInParseWindow,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfResponse {
    pub classification: ClassifyResult,
    pub extraction: Option<Result<ParseResult, WorkerDiagnostic>>,
    pub native_document_opens: u8,
    pub native_page_reads: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfNegativeKind {
    NoText,
    InvalidPdf,
    Protected,
}

impl PdfResponse {
    /// Requires a response validated against its request.
    pub fn negative_kind(&self) -> Option<PdfNegativeKind> {
        if self.native_document_opens != 1 || self.extraction.is_some() {
            return None;
        }
        match self.classification.status {
            ClassifyStatus::NoTextInParseWindow => Some(PdfNegativeKind::NoText),
            ClassifyStatus::Error if self.classification.page_count.is_none() => {
                let failure = self.classification.diagnostic.as_ref()?;
                match (failure.error_code.as_str(), failure.message.as_str()) {
                    ("PARSER_FAILED", "FPDF_ERR_FORMAT") => Some(PdfNegativeKind::InvalidPdf),
                    ("PDF_PROTECTED", "FPDF_ERR_PASSWORD") => Some(PdfNegativeKind::Protected),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn validate_for(&self, request: &InspectAndExtract) -> Result<(), String> {
        let classification = &self.classification;
        classification.validate_for_max_pages(request.inspect_max_pages)?;
        if self.native_document_opens > 1
            || (self.native_document_opens == 0
                && (classification.page_count.is_some() || self.native_page_reads != 0))
            || self.native_page_reads < classification.result_examined_pages.unwrap_or(0)
        {
            return Err("invalid PDF native work counts".into());
        }
        let mut first_text_page = None;
        if let Some(audit) = &classification.page_audit {
            let pages = request.inspection_pages(classification.page_count.unwrap_or(0));
            let observations = audit
                .observations
                .as_ref()
                .ok_or("missing PDF page observations")?;
            if audit.policy_version != request.policy_version
                || !pages.starts_with(&audit.checked_pages)
                || observations
                    .iter()
                    .take(observations.len().saturating_sub(1))
                    .any(|page| page.has_text)
            {
                return Err("PDF page observations do not match the request".into());
            }
            first_text_page = observations
                .last()
                .filter(|page| page.has_text)
                .map(|page| page.page);
            if (classification.status == ClassifyStatus::TextInParseWindow)
                != first_text_page.is_some()
            {
                return Err("PDF classification does not match its observations".into());
            }
        } else if matches!(
            classification.status,
            ClassifyStatus::TextInParseWindow | ClassifyStatus::NoTextInParseWindow
        ) {
            return Err("missing PDF page audit".into());
        }
        if let Some(error) = &classification.diagnostic {
            validate_diagnostic(error, request)?;
        }
        if self.extraction.is_some() != request.can_extract(classification.status) {
            return Err("PDF extraction does not match the request".into());
        }
        if matches!(
            request.parse.parser_limits,
            ParserLimits::PdfImageOcr { .. }
        ) && self.extraction.as_ref().is_some_and(Result::is_ok)
            && !classification.page_audit.as_ref().is_some_and(|audit| {
                audit.all_pages_are_images(classification.page_count.unwrap_or(0))
            })
        {
            return Err("OCR evidence requires fully observed image-only pages".into());
        }
        if let Some(extraction) = &self.extraction {
            match extraction {
                Ok(parsed) => validate_extraction(
                    parsed,
                    request,
                    classification.page_count.unwrap_or(0),
                    first_text_page,
                )?,
                Err(error) => validate_diagnostic(error, request)?,
            }
        }
        Ok(())
    }
}

fn validate_diagnostic(
    error: &WorkerDiagnostic,
    request: &InspectAndExtract,
) -> Result<(), String> {
    if error.file_path.as_deref() != Some(request.parse.file_path.as_str())
        || error.backend.as_deref() != Some(ParserBackend::RustPdfiumV3.as_str())
    {
        return Err("PDF diagnostic identity mismatch".into());
    }
    Ok(())
}

fn validate_extraction(
    parsed: &ParseResult,
    request: &InspectAndExtract,
    page_count: u64,
    first_text_page: Option<u64>,
) -> Result<(), String> {
    parsed.validate()?;
    let (limit, label) = match request.parse.parser_limits {
        ParserLimits::Pdf {
            excerpt_max_chars, ..
        } => (excerpt_max_chars, None),
        ParserLimits::PdfImageOcr {
            excerpt_max_chars, ..
        } => (excerpt_max_chars, Some(OCR_RANGE_LABEL)),
    };
    if parsed.file_path != request.parse.file_path
        || parsed.file_type != request.parse.file_type
        || parsed.parser_backend != request.parse.backend
        || parsed.observed_source_version != request.parse.expected_source_version
        || parsed.content.chars().count() as u64 > limit
    {
        return Err("PDF extraction identity or size mismatch".into());
    }
    let audit = parsed
        .evidence_audit
        .as_ref()
        .ok_or("missing PDF extraction audit")?;
    let pages = request.extraction_pages(page_count, first_text_page);
    let units = audit
        .content_units
        .as_ref()
        .ok_or("missing PDF source ranges")?;
    let ranges = audit
        .read_ranges
        .iter()
        .flatten()
        .chain(units.iter().flat_map(|unit| &unit.ranges));
    if units.iter().any(|unit| unit.ranges.is_empty())
        || ranges.into_iter().any(|range| {
            range.unit != SourceRangeUnit::PdfPage
                || range.label.as_deref() != label
                || range.start.checked_add(1) != Some(range.end)
                || range.start > page_count
                || !pages.contains(&range.start)
        })
    {
        return Err("PDF extraction source range is outside the requested pages".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfWorkerHello {
    pub pid: u32,
    pub parser_identity: String,
    pub pdfium_sha256: String,
    pub ocr: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfRequestFrame {
    pub version: u16,
    pub id: u64,
    pub command: PdfCommand,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PdfCommand {
    Hello,
    InspectAndExtract(Box<InspectAndExtract>),
}

impl PdfCommand {
    /// Bound the response before allocating its frame using only the request.
    pub fn response_frame_limit(&self) -> usize {
        let Self::InspectAndExtract(request) = self else {
            return HELLO_FRAME_LIMIT;
        };
        let chars = match request.parse.parser_limits {
            ParserLimits::Pdf {
                excerpt_max_chars, ..
            }
            | ParserLimits::PdfImageOcr {
                excerpt_max_chars, ..
            } if request.extract => excerpt_max_chars.min(10_000_000) as usize,
            _ => 0,
        };
        let content_audit = if chars == 0 {
            0
        } else {
            MAX_CONTENT_AUDIT_BYTES
        };
        // One JSON scalar can require six bytes. The path and page observations
        // are outside ParsedContentAudit and have their own request bounds.
        let content = 6 * chars;
        let path = 6 * request.parse.file_path.len().min(32_767);
        let page_audit = 96 * request.inspect_max_pages.min(10_000) as usize;
        (content + content_audit + path + page_audit + 4 * 1024).min(MAX_RESPONSE_FRAME)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfResponseFrame {
    pub version: u16,
    pub id: u64,
    pub reply: PdfReply,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PdfReply {
    Hello(PdfWorkerHello),
    PdfiumUnavailable(String),
    InspectAndExtract(Box<PdfResponse>),
}

/// EOF between frames is normal. EOF in either part of a frame is an error.
pub fn read_frame<T: DeserializeOwned>(
    input: &mut impl Read,
    limit: usize,
) -> io::Result<Option<T>> {
    let mut prefix = [0; 4];
    loop {
        match input.read(&mut prefix[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    input.read_exact(&mut prefix[1..])?;
    let length = u32::from_le_bytes(prefix) as usize;
    if length == 0 || length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PDF frame length out of bounds",
        ));
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

pub fn write_frame<T: Serialize>(
    output: &mut impl Write,
    frame: &T,
    limit: usize,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    let length = u32::try_from(bytes.len())
        .ok()
        .filter(|&length| length > 0 && length as usize <= limit)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "PDF frame length out of bounds")
        })?;
    output.write_all(&length.to_le_bytes())?;
    output.write_all(&bytes)?;
    output.flush()
}

pub fn pinned_pdfium_sha256() -> &'static str {
    static SHA256: LazyLock<String> = LazyLock::new(|| {
        let inputs: serde_json::Value =
            serde_json::from_str(include_str!("../../build-inputs.json")).expect("build inputs");
        inputs["pdfium"]["dll_sha256"]
            .as_str()
            .expect("PDFium DLL pin")
            .to_owned()
    });
    SHA256.as_str()
}

/// Keep the returned file alive until the pinned DLL is loaded.
pub fn open_pinned_pdfium(program_dir: &Path) -> io::Result<(PathBuf, File)> {
    let directory = program_dir.canonicalize()?;
    let path = directory.join("pdfium.dll").canonicalize()?;
    if path.parent() != Some(directory.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PDFium DLL is outside the worker directory",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 1);
    let mut file = options.open(&path)?;
    if reader_sha256(&mut file)? != pinned_pdfium_sha256() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PDFIUM_VERSION_MISMATCH",
        ));
    }
    Ok((path, file))
}

pub use crate::reader_sha256;

#[cfg(windows)]
pub fn file_identity_sha256(file: &File) -> Option<(SourceGuardKind, String)> {
    use std::{mem::size_of_val, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_ID_INFO,
    };

    let mut summary = BY_HANDLE_FILE_INFORMATION::default();
    let mut id = FILE_ID_INFO::default();
    let mut basic = FILE_BASIC_INFO::default();
    let handle = file.as_raw_handle();
    // The file owns the live handle and each output has the Win32 layout for its query.
    let known = unsafe {
        GetFileInformationByHandle(handle, &mut summary) != 0
            && GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&raw mut id).cast(),
                size_of_val(&id) as u32,
            ) != 0
            && GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&raw mut basic).cast(),
                size_of_val(&basic) as u32,
            ) != 0
    };
    let identifier = id.FileId.Identifier;
    if !known
        || summary.dwVolumeSerialNumber == 0
        || identifier == [0; 16]
        || basic.LastWriteTime <= 0
        || basic.ChangeTime <= 0
    {
        return None;
    }
    let size = (u64::from(summary.nFileSizeHigh) << 32) | u64::from(summary.nFileSizeLow);
    Some(metadata_guard(
        SourceGuardKind::WindowsFileIdChangeTimeV1,
        &[
            &summary.dwVolumeSerialNumber.to_le_bytes(),
            &identifier,
            &size.to_le_bytes(),
            &basic.LastWriteTime.to_le_bytes(),
            &basic.ChangeTime.to_le_bytes(),
        ],
    ))
}

#[cfg(unix)]
pub fn file_identity_sha256(file: &File) -> Option<(SourceGuardKind, String)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().ok()?;
    let mtime = u128::try_from(metadata.mtime()).ok()? * 1_000_000_000
        + u128::try_from(metadata.mtime_nsec()).ok()?;
    let ctime = u128::try_from(metadata.ctime()).ok()? * 1_000_000_000
        + u128::try_from(metadata.ctime_nsec()).ok()?;
    if metadata.dev() == 0 || metadata.ino() == 0 || mtime == 0 || ctime == 0 {
        return None;
    }
    Some(metadata_guard(
        SourceGuardKind::UnixInodeCtimeV1,
        &[
            &metadata.dev().to_le_bytes(),
            &metadata.ino().to_le_bytes(),
            &metadata.size().to_le_bytes(),
            &mtime.to_le_bytes(),
            &ctime.to_le_bytes(),
        ],
    ))
}

#[cfg(not(any(unix, windows)))]
pub fn file_identity_sha256(_: &File) -> Option<(SourceGuardKind, String)> {
    None
}

#[cfg(any(unix, windows))]
fn metadata_guard(kind: SourceGuardKind, fields: &[&[u8]]) -> (SourceGuardKind, String) {
    let mut hasher = Sha256::new();
    hasher.update(SOURCE_GUARD_DOMAIN);
    for field in fields {
        hasher.update(field);
    }
    (kind, crate::hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_use_a_fixed_little_endian_length_and_handle_large_values() {
        use crate::contract::{AuditTextBuilder, ContentUnitKind, SourceRange};

        let command = business_request(400_000, true);
        let limit = command.response_frame_limit();
        let PdfCommand::InspectAndExtract(request) = command else {
            unreachable!()
        };
        let mut builder = AuditTextBuilder::new(400_000, "pdf");
        let range = SourceRange::new(SourceRangeUnit::PdfPage, 1, 2, None);
        builder.read(range.clone());
        assert!(builder.push(&"中".repeat(400_000), ContentUnitKind::Text, vec![range]));
        let (content, evidence_audit, truncated) = builder.finish();
        let response = PdfResponse {
            classification: ClassifyResult {
                page_audit: Some(crate::contract::PdfPageAudit {
                    policy_version: request.policy_version.clone(),
                    checked_pages: vec![1],
                    observations: Some(vec![crate::contract::PdfPageObservation {
                        page: 1,
                        has_text: true,
                        has_image: Some(false),
                    }]),
                }),
                status: ClassifyStatus::TextInParseWindow,
                page_count: Some(1),
                result_examined_pages: Some(1),
                diagnostic: None,
            },
            extraction: Some(Ok(ParseResult {
                file_path: request.parse.file_path.clone(),
                file_type: request.parse.file_type.clone(),
                content,
                evidence_audit: Some(evidence_audit),
                parser_backend: request.parse.backend,
                worker_lane: request.parse.backend.lane(),
                truncated,
                duration_ms: 0,
                observed_source_version: request.parse.expected_source_version.clone(),
            })),
            native_document_opens: 1,
            native_page_reads: 1,
        };
        response.validate_for(&request).unwrap();
        let payload = PdfResponseFrame {
            version: PDF_PROTOCOL_VERSION,
            id: 1,
            reply: PdfReply::InspectAndExtract(Box::new(response.clone())),
        };
        let mut frame = Vec::new();
        write_frame(&mut frame, &payload, limit).unwrap();
        assert_eq!(
            u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize,
            frame.len() - 4
        );
        assert!(frame.len() > 1024 * 1024);
        let mut reader = frame.as_slice();
        let decoded: PdfResponseFrame = read_frame(&mut reader, limit).unwrap().unwrap();
        let PdfReply::InspectAndExtract(decoded_response) = decoded.reply else {
            unreachable!()
        };
        assert_eq!(*decoded_response, response);
        assert!(read_frame::<PdfResponseFrame>(&mut reader, limit)
            .unwrap()
            .is_none());
    }

    #[test]
    fn partial_or_oversize_frames_cannot_be_mistaken_for_eof() {
        for mut truncated in [&[1_u8, 0][..], b"\x04\0\0\0{\""] {
            assert_eq!(
                read_frame::<String>(&mut truncated, MAX_RESPONSE_FRAME)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
        let size = (MAX_REQUEST_FRAME as u32 + 1).to_le_bytes();
        assert_eq!(
            read_frame::<String>(&mut size.as_slice(), MAX_REQUEST_FRAME)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn default_response_rejects_oversize_header_before_reading_any_body() {
        struct NoBody;
        impl Read for NoBody {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("an oversized frame must be rejected at the header");
            }
        }

        let limit = business_request(60_000, true).response_frame_limit();
        assert!(limit < 1024 * 1024);
        assert!(business_request(60_000, false).response_frame_limit() < 16 * 1024);
        assert_eq!(PdfCommand::Hello.response_frame_limit(), HELLO_FRAME_LIMIT);
        let header = (limit as u32 + 1).to_le_bytes();
        let mut input = header.as_slice().chain(NoBody);
        let error = read_frame::<PdfResponseFrame>(&mut input, limit).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let maximum = business_request(10_000_000, true).response_frame_limit();
        assert!(maximum >= 60_000_000 + MAX_CONTENT_AUDIT_BYTES);
        assert!(maximum <= MAX_RESPONSE_FRAME);
    }

    fn business_request(chars: u64, extract: bool) -> PdfCommand {
        PdfCommand::InspectAndExtract(Box::new(InspectAndExtract::new(
            ParseRequest {
                file_path: r"C:\synthetic\test.pdf".into(),
                file_type: ".pdf".into(),
                backend: ParserBackend::RustPdfiumV3,
                remaining_timeout_ms: 500,
                max_file_size_bytes: 1024 * 1024,
                parser_limits: ParserLimits::Pdf {
                    max_pages: 20,
                    excerpt_max_chars: chars,
                    selected_pages: None,
                },
                expected_source_version: "mtime_ns=1:size=1".into(),
            },
            20,
            "pdf_text_presence_v2".into(),
            extract,
        )))
    }
}
