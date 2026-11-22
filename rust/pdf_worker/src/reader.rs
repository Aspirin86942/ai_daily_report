use crate::{
    excerpt,
    source::{OpenSource, ReadState},
};
use daily_parsers::{
    contract::{
        AuditTextBuilder, ClassifyResult, ClassifyStatus, ContentUnitKind, ParseResult,
        ParserLimits, PdfPageAudit, PdfPageObservation, SourceRange, SourceRangeUnit,
        WorkerDiagnostic,
    },
    pdf_wire::{open_pinned_pdfium, InspectAndExtract, PdfResponse},
};
use pdfium_render::prelude::*;
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
pub(super) enum PdfError {
    #[error("PDFIUM_UNAVAILABLE: {0}")]
    Library(#[from] std::io::Error),
    #[error("PDF_PARSE_FAILED: {0}")]
    Native(#[from] PdfiumError),
    #[error("FPDF_ERR_FORMAT")]
    InvalidPdf,
    #[error("FPDF_ERR_PASSWORD")]
    Protected,
    #[error("PDF source could not be read")]
    SourceIo,
    #[error("PDF source identity changed")]
    SourceChanged,
    #[error("PDF page window is invalid")]
    Pages,
    #[error("PDF document has no pages")]
    NoPages,
    #[error("PDF source exceeds the size limit")]
    TooLarge,
    #[error("PDF request is invalid")]
    Request,
    #[error("PDF request exceeded its deadline")]
    Timeout,
    #[error("PDF_OCR_UNAVAILABLE")]
    OcrUnavailable,
    #[cfg(feature = "ocr")]
    #[error("PDF_OCR_PAGE_LIMIT")]
    OcrLimits,
    #[error("PDF_OCR_PAGE_NOT_IMAGE_ONLY")]
    OcrIneligible,
    #[cfg(feature = "ocr")]
    #[error("PDF_OCR_MODEL_MISSING")]
    OcrModelMissing,
    #[cfg(feature = "ocr")]
    #[error("PDF_OCR_MODEL_MISMATCH")]
    OcrModelMismatch,
    #[cfg(feature = "ocr")]
    #[error("PDF_OCR_FAILED")]
    OcrFailed,
}

impl PdfError {
    fn from_native_open(error: PdfiumError) -> Self {
        match error {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::FormatError) => {
                Self::InvalidPdf
            }
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                Self::Protected
            }
            error => Self::Native(error),
        }
    }

    fn diagnostic(&self, request: &InspectAndExtract) -> WorkerDiagnostic {
        let (code, retryable) = match self {
            Self::Protected => ("PDF_PROTECTED", false),
            Self::SourceIo => ("PARSER_START_FAILED", true),
            Self::SourceChanged => ("SOURCE_VERSION_CHANGED", true),
            Self::TooLarge => ("FILE_TOO_LARGE", false),
            Self::Request | Self::Pages => ("PARSER_INVALID_PAYLOAD", false),
            Self::Timeout => ("PARSER_TIMEOUT", true),
            Self::OcrIneligible => ("PARSER_INVALID_PAYLOAD", false),
            #[cfg(feature = "ocr")]
            Self::OcrLimits => ("PARSER_INVALID_PAYLOAD", false),
            _ => ("PARSER_FAILED", false),
        };
        WorkerDiagnostic {
            error_code: code.into(),
            message: self.to_string(),
            retryable,
            stage: if retryable { "process" } else { "parse" }.into(),
            file_path: Some(request.parse.file_path.clone()),
            backend: Some(request.parse.backend.as_str().into()),
        }
    }
}

pub(super) struct PdfReader {
    pdfium: Pdfium,
    #[cfg(feature = "ocr")]
    program_dir: std::path::PathBuf,
}

#[derive(Default)]
struct Inspection {
    page_count: Option<u64>,
    observations: Vec<PdfPageObservation>,
    document_opens: u8,
    page_reads: u64,
}

type Extraction = Option<Result<ParseResult, WorkerDiagnostic>>;

impl PdfReader {
    pub(super) fn open(program_dir: &Path) -> Result<Self, PdfError> {
        let (library, pinned_file) = open_pinned_pdfium(program_dir)?;
        let pdfium = Pdfium::new(Pdfium::bind_to_library(&library)?);
        drop(pinned_file);
        Ok(Self {
            pdfium,
            #[cfg(feature = "ocr")]
            program_dir: program_dir.to_path_buf(),
        })
    }

    pub(super) fn inspect_and_extract(&self, request: &InspectAndExtract) -> PdfResponse {
        let mut inspection = Inspection::default();
        let started = Instant::now();
        let result = request
            .validate()
            .map_err(|_| PdfError::Request)
            .and_then(|()| self.process(request, &mut inspection, started));
        let (status, extraction, diagnostic) = match result {
            Ok((status, extraction)) => (status, extraction, None),
            Err(error) => {
                let diagnostic = error.diagnostic(request);
                let status = if diagnostic.retryable {
                    ClassifyStatus::Unknown
                } else {
                    ClassifyStatus::Error
                };
                (status, None, Some(diagnostic))
            }
        };
        let examined = inspection.observations.len() as u64;
        PdfResponse {
            classification: ClassifyResult {
                status,
                page_count: inspection.page_count,
                result_examined_pages: inspection.page_count.map(|_| examined),
                page_audit: inspection.page_count.map(|_| PdfPageAudit {
                    policy_version: request.policy_version.clone(),
                    checked_pages: inspection
                        .observations
                        .iter()
                        .map(|page| page.page)
                        .collect(),
                    observations: Some(inspection.observations),
                }),
                diagnostic,
            },
            extraction,
            native_document_opens: inspection.document_opens,
            native_page_reads: inspection.page_reads,
        }
    }

    fn process(
        &self,
        request: &InspectAndExtract,
        inspection: &mut Inspection,
        started: Instant,
    ) -> Result<(ClassifyStatus, Extraction), PdfError> {
        let deadline = started + Duration::from_millis(request.parse.remaining_timeout_ms);
        let source = OpenSource::open(request, deadline)?;
        let inspection_deadline = request
            .inspect_timeout_ms
            .map(|ms| deadline.min(Instant::now() + Duration::from_millis(ms)));
        let state = ReadState::new(deadline, inspection_deadline);
        inspection.document_opens += 1;
        let opened = self
            .pdfium
            .load_pdf_from_reader(state.input(&source.file, true), None);
        let result = state.check().and_then(|()| match opened {
            Ok(document) => self.inspect_document(&document, request, inspection, &state, started),
            Err(error) => Err(PdfError::from_native_open(error)),
        });
        if let Err(error) = source.verify(request, deadline) {
            // The observations cannot be attributed to the requested source.
            inspection.page_count = None;
            inspection.observations.clear();
            return Err(error);
        }
        match result {
            Ok(result) => Ok(result),
            Err(error) => state.check().and(Err(error)),
        }
    }

    fn inspect_document(
        &self,
        document: &PdfDocument<'_>,
        request: &InspectAndExtract,
        inspection: &mut Inspection,
        state: &ReadState,
        started: Instant,
    ) -> Result<(ClassifyStatus, Extraction), PdfError> {
        let count = u64::try_from(document.pages().len()).map_err(|_| PdfError::NoPages)?;
        if count == 0 {
            return Err(PdfError::NoPages);
        }
        inspection.page_count = Some(count);
        let mut cached_text = BTreeMap::new();
        #[cfg(feature = "ocr")]
        let mut image_pages = BTreeMap::new();
        let mut first_text_page = None;
        for number in request.inspection_pages(count) {
            let page = inspection.load_page(document, number, state)?;
            let text = page.text()?;
            let raw = text.inside_rect(excerpt::page_bounds(&page));
            let has_text = raw.chars().any(is_valid_text_char);
            let has_image = page_images(&page);
            if request.extract && matches!(request.parse.parser_limits, ParserLimits::Pdf { .. }) {
                // All previously inspected pages have no usable text. Keep only the first text page.
                let formatted = if has_text {
                    excerpt::format_page_text(&page, &text, raw)
                } else {
                    String::new()
                };
                cached_text.insert(number, formatted);
            }
            drop(text);
            state.check()?;
            inspection.observations.push(PdfPageObservation {
                page: number,
                has_text,
                has_image,
            });
            #[cfg(feature = "ocr")]
            if request.extract
                && matches!(&request.parse.parser_limits,
                    ParserLimits::PdfImageOcr { selected_pages, .. } if selected_pages.contains(&number))
            {
                image_pages.insert(number, page);
            }
            if has_text {
                first_text_page = Some(number);
                break;
            }
        }
        let status = if first_text_page.is_some() {
            ClassifyStatus::TextInParseWindow
        } else {
            ClassifyStatus::NoTextInParseWindow
        };
        state.finish_inspection();
        if !request.can_extract(status) {
            return Ok((status, None));
        }
        if matches!(
            request.parse.parser_limits,
            ParserLimits::PdfImageOcr { .. }
        ) && (inspection.observations.len() as u64 != count
            || !inspection
                .observations
                .iter()
                .all(|page| !page.has_text && page.has_image == Some(true)))
        {
            return Ok((
                status,
                Some(Err(PdfError::OcrIneligible.diagnostic(request))),
            ));
        }
        let pages = request.extraction_pages(count, first_text_page);
        let excerpt_max_chars = match request.parse.parser_limits {
            ParserLimits::Pdf {
                excerpt_max_chars, ..
            }
            | ParserLimits::PdfImageOcr {
                excerpt_max_chars, ..
            } => excerpt_max_chars,
        };
        let mut builder = AuditTextBuilder::new(excerpt_max_chars, "pdf");
        let extracted = if pages.is_empty()
            || pages.iter().any(|&page| !(1..=count).contains(&page))
        {
            Err(PdfError::Pages)
        } else if matches!(
            request.parse.parser_limits,
            ParserLimits::PdfImageOcr { .. }
        ) {
            #[cfg(feature = "ocr")]
            {
                use daily_parsers::contract::OCR_TIMEOUT_MS;
                let deadline = started
                    + Duration::from_millis(request.parse.remaining_timeout_ms.min(OCR_TIMEOUT_MS));
                crate::ocr::extract(
                    &self.program_dir,
                    count,
                    &pages,
                    excerpt_max_chars,
                    deadline,
                    &mut builder,
                    |number| image_pages.remove(&number).ok_or(PdfError::OcrIneligible),
                )
            }
            #[cfg(not(feature = "ocr"))]
            Err(PdfError::OcrUnavailable)
        } else {
            excerpt::read_pages(count, &pages, excerpt_max_chars as usize, |number| {
                if let Some(text) = cached_text.remove(&number) {
                    return Ok(text);
                }
                let text =
                    excerpt::read_page_text(&inspection.load_page(document, number, state)?)?;
                state.check()?;
                Ok(text)
            })
            .map(|parsed| {
                for page in parsed.pages {
                    let range =
                        SourceRange::new(SourceRangeUnit::PdfPage, page.page, page.page + 1, None);
                    builder.read(range.clone());
                    for line in page.text.lines() {
                        builder.push(line, ContentUnitKind::Text, vec![range.clone()]);
                    }
                }
                parsed.truncated
            })
        };
        let extraction = state
            .check()
            .and(extracted)
            .map(|partial| {
                let (content, audit, truncated) = builder.finish();
                ParseResult {
                    file_path: request.parse.file_path.clone(),
                    file_type: request.parse.file_type.clone(),
                    content,
                    evidence_audit: Some(audit),
                    parser_backend: request.parse.backend,
                    worker_lane: request.parse.backend.lane(),
                    truncated: truncated || partial,
                    duration_ms: started.elapsed().as_millis() as u64,
                    observed_source_version: request.parse.expected_source_version.clone(),
                }
            })
            .map_err(|error| error.diagnostic(request));
        Ok((status, Some(extraction)))
    }
}

impl Inspection {
    fn load_page<'a>(
        &mut self,
        document: &PdfDocument<'a>,
        number: u64,
        state: &ReadState,
    ) -> Result<PdfPage<'a>, PdfError> {
        state.check()?;
        self.page_reads += 1;
        let page = document.pages().get((number - 1) as PdfPageIndex);
        state.check()?;
        Ok(page?)
    }
}

fn page_images(page: &PdfPage<'_>) -> Option<bool> {
    let mut found = Some(false);
    for (index, object) in page.objects().iter().enumerate() {
        if index >= 2048 {
            return None;
        }
        if object.as_image_object().is_some() {
            return Some(true);
        }
        if object.as_x_object_form_object().is_some() {
            found = None;
        }
    }
    found
}

pub(super) fn check_deadline(deadline: Instant) -> Result<(), PdfError> {
    if Instant::now() >= deadline {
        Err(PdfError::Timeout)
    } else {
        Ok(())
    }
}

/// The frozen classifier ignores whitespace, controls, format and private-use characters.
pub(super) fn is_valid_text_char(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && c != '\u{fffd}'
        && !matches!(c as u32,
        0xad | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x890..=0x891 | 0x8e2 |
        0x180e | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2064 | 0x2066..=0x206f |
        0xfeff | 0xfff9..=0xfffb | 0x110bd | 0x110cd | 0x13430..=0x1343f |
        0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a | 0xe0001 | 0xe0020..=0xe007f |
        0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd)
}
