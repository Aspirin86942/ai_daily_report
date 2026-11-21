use crate::{excerpt, ocr, page_numbers, PdfError, PdfReader, PdfText};
use ai_daily_worker_contract::*;
use pdfium_render::prelude::*;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Cursor, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

mod source;
use source::{source, OpenSource, SourceStamp};

const MAX_CACHED_DOCUMENTS: usize = 32;
const MAX_CACHED_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CACHED_FILE_BYTES: u64 = 1024 * 1024;
const MAX_CACHED_PAGES: usize = 64;
const MAX_CACHED_FILE_TEXT_BYTES: usize = 256 * 1024;
const MAX_CACHED_TEXT_BYTES: usize = 2 * 1024 * 1024;

struct ClassifiedPdf<'a> {
    path: String,
    stamp: SourceStamp,
    document: PdfDocument<'a>,
    snapshot: PdfSnapshot,
    pages: BTreeMap<u32, String>,
    text_bytes: usize,
}

pub struct PdfSession<'a> {
    reader: &'a PdfReader,
    pending: Option<VecDeque<ClassifiedPdf<'a>>>,
    #[cfg(test)]
    work: WorkCounts,
}

#[cfg(test)]
#[derive(Debug, Default, PartialEq, Eq)]
struct WorkCounts {
    document_opens: usize,
    page_reads: usize,
}

pub fn worker_hello() -> WorkerHello {
    WorkerHello {
        contract: CONTRACT.into(),
        protocol_version: PROTOCOL_VERSION,
        frame: "hello".into(),
        worker_contract_version: CONTRACT_VERSION.into(),
        worker_kind: WorkerKind::Pdf,
        worker_version: env!("CARGO_PKG_VERSION").into(),
        worker_build: env!("AI_DAILY_PDF_WORKER_BUILD").into(),
        supported_operations: vec![
            WorkerOperation::PdfClassify,
            WorkerOperation::PdfParse,
            WorkerOperation::ArchivePdfParse,
        ],
    }
}

impl PdfReader {
    pub fn session(&self) -> PdfSession<'_> {
        PdfSession::new(self, true)
    }

    pub fn parse_request(
        &self,
        request: &ParseRequest,
    ) -> Result<ParseResult, Box<WorkerDiagnostic>> {
        PdfSession::new(self, false).parse_request(request)
    }

    pub fn classify_request(&self, request: &ClassifyRequest) -> ClassifyResult {
        PdfSession::new(self, false).classify_request(request)
    }

    pub fn archive_request(
        &self,
        request: &archive::ArchiveRequest,
    ) -> Result<archive::ArchiveArtifact, Box<WorkerDiagnostic>> {
        let started = Instant::now();
        let path = &request.source.file_path;
        if matches!(
            request.source.parser_limits,
            ParserLimits::PdfImageOcr { .. }
        ) {
            return Err(diagnostic(path, "PARSER_INVALID_PAYLOAD", false));
        }
        request
            .validate()
            .map_err(|_| diagnostic(path, "PARSER_INVALID_PAYLOAD", false))?;
        let before = source(path)?;
        if before.0 != request.source.expected_source_version {
            return Err(diagnostic(path, "SOURCE_VERSION_CHANGED", true));
        }
        if before.1 > request.source.max_file_size_bytes {
            return Err(diagnostic(path, "FILE_TOO_LARGE", false));
        }
        let mut writer = archive::ArchiveWriter::new(&request.output_dir, request.max_output_bytes)
            .map_err(|_| diagnostic(path, "ARCHIVE_OUTPUT_FAILED", true))?;
        let mut no_text_pages = vec![];
        let mut image_pages = vec![];
        let mut unchecked_pages = vec![];
        let (pages, truncated) = self
            .visit_archive_pages(Path::new(path), |page, text, images| {
                match images {
                    Some(true) => {
                        image_pages.push(page);
                        writer.gap("PDF_IMAGE_CONTENT_NOT_EXTRACTED");
                    }
                    None => {
                        unchecked_pages.push(page);
                        writer.gap("PDF_IMAGE_INSPECTION_BOUNDED");
                    }
                    _ => {}
                }
                let range = SourceRange::new(
                    SourceRangeUnit::PdfPage,
                    u64::from(page),
                    u64::from(page) + 1,
                    None,
                );
                if !text.chars().any(is_valid_text_char) {
                    no_text_pages.push(page);
                    writer.gap("PDF_PAGE_WITHOUT_TEXT");
                }
                for line in text.lines() {
                    if !writer.push(line, ContentUnitKind::Text, vec![range.clone()]) {
                        return false;
                    }
                }
                true
            })
            .map_err(|e| parse_error(path, &e))?;
        if source(path)?.0 != before.0 {
            return Err(diagnostic(path, "SOURCE_VERSION_CHANGED", true));
        }
        let mut artifact = writer.finish();
        artifact.coverage = serde_json::json!({"pages_total":pages,"no_text_pages":no_text_pages,"image_pages":image_pages,"image_inspection_bounded_pages":unchecked_pages});
        if artifact.error.is_some() {
            return Err(diagnostic(path, "ARCHIVE_OUTPUT_FAILED", true));
        }
        artifact.truncated |= truncated;
        if artifact.usable_chars == 0 {
            artifact.gaps.push("NO_USABLE_TEXT".into());
        }
        artifact.parser_backend = "rust_pdfium_v3".into();
        artifact.worker_lane = "rust_pdf_archive_process_v4".into();
        artifact.raw_sha256 = request.raw_sha256.clone();
        artifact.duration_ms = started.elapsed().as_millis() as u64;
        Ok(artifact)
    }
}

impl<'a> PdfSession<'a> {
    fn new(reader: &'a PdfReader, reuse: bool) -> Self {
        Self {
            reader,
            pending: reuse.then(VecDeque::new),
            #[cfg(test)]
            work: WorkCounts::default(),
        }
    }

    fn take_classified(&mut self, path: &str) -> Option<ClassifiedPdf<'a>> {
        let pending = self.pending.as_mut()?;
        let index = pending.iter().position(|entry| entry.path == path)?;
        pending.remove(index)
    }

    fn make_snapshot_room(&mut self, bytes: u64) {
        let pending = self.pending.as_mut().expect("document reuse enabled");
        let mut source_bytes: u64 = pending.iter().map(|entry| entry.stamp.bytes).sum();
        let mut text_bytes: usize = pending.iter().map(|entry| entry.text_bytes).sum();
        while pending.len() >= MAX_CACHED_DOCUMENTS
            || source_bytes + bytes > MAX_CACHED_SOURCE_BYTES
            || text_bytes + MAX_CACHED_FILE_TEXT_BYTES > MAX_CACHED_TEXT_BYTES
        {
            let evicted = pending.pop_front().expect("over capacity");
            source_bytes -= evicted.stamp.bytes;
            text_bytes -= evicted.text_bytes;
        }
    }

    fn open_document(
        &mut self,
        input: impl Read + Seek + Send + Sync + 'a,
    ) -> Result<PdfDocument<'a>, PdfiumError> {
        #[cfg(test)]
        {
            self.work.document_opens += 1;
        }
        self.reader.pdfium.load_pdf_from_reader(input, None)
    }

    fn load_page(
        &mut self,
        document: &PdfDocument<'a>,
        number: u32,
    ) -> Result<PdfPage<'a>, PdfError> {
        #[cfg(test)]
        {
            self.work.page_reads += 1;
        }
        Ok(document.pages().get((number - 1) as PdfPageIndex)?)
    }

    pub fn archive_request(
        &self,
        request: &archive::ArchiveRequest,
    ) -> Result<archive::ArchiveArtifact, Box<WorkerDiagnostic>> {
        self.reader.archive_request(request)
    }

    pub fn parse_request(
        &mut self,
        request: &ParseRequest,
    ) -> Result<ParseResult, Box<WorkerDiagnostic>> {
        let started = Instant::now();
        request
            .validate()
            .map_err(|_| diagnostic(&request.file_path, "PARSER_INVALID_PAYLOAD", false))?;
        if request.backend != ParserBackend::RustPdfiumV3 {
            return Err(diagnostic(
                &request.file_path,
                "PARSER_INVALID_PAYLOAD",
                false,
            ));
        }
        let (max_pages, excerpt_max_chars, selected_pages, image_ocr) = match &request.parser_limits
        {
            ParserLimits::Pdf {
                max_pages,
                excerpt_max_chars,
                selected_pages,
            } => (
                *max_pages,
                *excerpt_max_chars,
                selected_pages.as_deref(),
                false,
            ),
            ParserLimits::PdfImageOcr {
                max_pages,
                excerpt_max_chars,
                selected_pages,
            } => (
                *max_pages,
                *excerpt_max_chars,
                Some(selected_pages.as_slice()),
                true,
            ),
            _ => {
                return Err(diagnostic(
                    &request.file_path,
                    "PARSER_INVALID_PAYLOAD",
                    false,
                ))
            }
        };
        let pages = selected_pages
            .map(|pages| {
                pages
                    .iter()
                    .map(|n| u32::try_from(*n))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|_| diagnostic(&request.file_path, "PARSER_INVALID_PAYLOAD", false))?;
        let mut builder = AuditTextBuilder::new(excerpt_max_chars, "pdf");
        let path = &request.file_path;
        let (partial, observed_source_version) = if image_ocr {
            let (version, bytes) = source(path)?;
            check_parse_source(request, &version, bytes)?;
            let pages = pages
                .as_deref()
                .ok_or_else(|| parse_error(path, &PdfError::OcrLimits))?;
            let deadline =
                started + Duration::from_millis(request.remaining_timeout_ms.min(OCR_TIMEOUT_MS));
            let truncated = ocr::extract(
                self.reader,
                Path::new(path),
                pages,
                excerpt_max_chars,
                deadline,
                &mut builder,
            )
            .map_err(|error| parse_error(path, &error))?;
            if source(path)?.0 != version {
                return Err(diagnostic(path, "SOURCE_VERSION_CHANGED", false));
            }
            (truncated, version)
        } else {
            let (parsed, version) = self.parse_text_request(
                request,
                pages.as_deref(),
                max_pages as u32,
                excerpt_max_chars as usize,
            )?;
            for page in &parsed.pages {
                let range = SourceRange::new(
                    SourceRangeUnit::PdfPage,
                    u64::from(page.page),
                    u64::from(page.page) + 1,
                    None,
                );
                builder.read(range.clone());
                for line in page.text.lines() {
                    builder.push(line, ContentUnitKind::Text, vec![range.clone()]);
                }
            }
            (parsed.truncated, version)
        };
        let (content, audit, truncated) = builder.finish();
        Ok(ParseResult {
            file_path: request.file_path.clone(),
            file_type: request.file_type.clone(),
            content,
            evidence_audit: Some(audit),
            parser_backend: ParserBackend::RustPdfiumV3,
            worker_lane: WorkerLane::RustPdfProcessV3,
            truncated: truncated || partial,
            duration_ms: started.elapsed().as_millis() as u64,
            observed_source_version,
        })
    }

    fn parse_text_request(
        &mut self,
        request: &ParseRequest,
        selected: Option<&[u32]>,
        max_pages: u32,
        max_chars: usize,
    ) -> Result<(PdfText, String), Box<WorkerDiagnostic>> {
        let path = &request.file_path;
        let cached = self.take_classified(path);
        let OpenSource { mut file, stamp } = OpenSource::open(path)?;
        check_parse_source(request, &stamp.version, stamp.bytes)?;
        // Filesystems may report equal timestamps for independent writes of the same length.
        let cached =
            cached.filter(|entry| entry.stamp == stamp && entry.snapshot.matches(&mut file));
        let (document, cached_pages) = match cached {
            Some(entry) => (Ok(entry.document), entry.pages),
            None => (self.open_document(file), BTreeMap::new()),
        };
        let parsed = self.read_document(document, cached_pages, selected, max_pages, max_chars);
        stamp.check(path)?;
        Ok((parsed.map_err(|e| parse_error(path, &e))?, stamp.version))
    }

    fn read_document(
        &mut self,
        document: Result<PdfDocument<'a>, PdfiumError>,
        mut cached_pages: BTreeMap<u32, String>,
        selected: Option<&[u32]>,
        max_pages: u32,
        max_chars: usize,
    ) -> Result<PdfText, PdfError> {
        let document = document?;
        let (total, pages) = page_numbers(&document, selected, max_pages)?;
        excerpt::read_pages(total, &pages, max_chars, |number| {
            if let Some(text) = cached_pages.remove(&number) {
                return Ok(text);
            }
            excerpt::read_page_text(&self.load_page(&document, number)?)
        })
    }

    pub fn classify_request(&mut self, request: &ClassifyRequest) -> ClassifyResult {
        match self.classify_inner(request) {
            Ok(value) => value,
            Err(error) => ClassifyResult {
                page_audit: None,
                status: if error.retryable {
                    ClassifyStatus::Unknown
                } else {
                    ClassifyStatus::Error
                },
                page_count: None,
                result_examined_pages: None,
                diagnostic: Some(*error),
            },
        }
    }

    fn classify_inner(
        &mut self,
        request: &ClassifyRequest,
    ) -> Result<ClassifyResult, Box<WorkerDiagnostic>> {
        let path = &request.file_path;
        request
            .validate()
            .map_err(|_| diagnostic(path, "PARSER_INVALID_PAYLOAD", false))?;
        self.take_classified(path);
        let OpenSource { mut file, stamp } = OpenSource::open(path)?;
        if stamp.version != request.source_version {
            return Err(diagnostic(path, "SOURCE_VERSION_CHANGED", false));
        }

        let eligible = self.pending.is_some()
            && stamp.reusable()
            && (1..=MAX_CACHED_FILE_BYTES).contains(&stamp.bytes);
        let snapshot = if eligible {
            self.make_snapshot_room(stamp.bytes);
            PdfSnapshot::read(&mut file, stamp.bytes as usize)
        } else {
            None
        };
        let retain = snapshot.is_some();
        let io_failed = Arc::new(AtomicBool::new(false));
        let opened = match &snapshot {
            // PDFium owns this reader until it closes the document. The original file can be released.
            Some(bytes) => {
                self.open_document(ClassifierInput::new(Cursor::new(bytes.clone()), &io_failed))
            }
            None => self.open_document(ClassifierInput::new(file, &io_failed)),
        };
        let document = match opened {
            Ok(document) => document,
            Err(error) => {
                stamp.check(path)?;
                return Err(classify_open_error(
                    path,
                    stamp.bytes,
                    error,
                    (eligible && !retain) || io_failed.load(Ordering::Relaxed),
                ));
            }
        };
        let mut cached_pages = BTreeMap::new();
        let mut remaining = MAX_CACHED_FILE_TEXT_BYTES;
        let classified = (|| {
            let count = document.pages().len() as u64;
            if count == 0 {
                return Err(diagnostic(path, "PARSER_FAILED", false));
            }
            let mut audit = PdfPageAudit {
                policy_version: request.policy_version.clone(),
                checked_pages: Vec::new(),
                observations: Some(Vec::new()),
            };
            let mut found = false;
            for number in pdf_sample_pages(count, request.max_pages, &request.policy_version) {
                let page = self
                    .load_page(&document, number as u32)
                    .map_err(|error| parse_error(path, &error))?;
                let text = page
                    .text()
                    .map_err(|error| parse_error(path, &PdfError::Parse(error)))?;
                let raw = text.all();
                found = raw.chars().any(is_valid_text_char);
                let has_image = crate::page_images(&page);
                if retain && cached_pages.len() < MAX_CACHED_PAGES && raw.len() <= remaining {
                    let formatted = excerpt::format_page_text(&page, &text, raw);
                    if formatted.capacity() <= remaining {
                        remaining -= formatted.capacity();
                        cached_pages.insert(number as u32, formatted);
                    }
                }
                audit.checked_pages.push(number);
                audit
                    .observations
                    .as_mut()
                    .unwrap()
                    .push(PdfPageObservation {
                        page: number,
                        has_text: found,
                        has_image,
                    });
                if found {
                    break;
                }
            }
            Ok(ClassifyResult {
                status: if found {
                    ClassifyStatus::TextInParseWindow
                } else {
                    ClassifyStatus::NoTextInParseWindow
                },
                page_count: Some(count),
                result_examined_pages: Some(audit.checked_pages.len() as u64),
                page_audit: Some(audit),
                diagnostic: None,
            })
        })();
        stamp.check(path)?;
        let result = classified?;
        if let Some(snapshot) = snapshot.filter(|_| {
            result.status == ClassifyStatus::TextInParseWindow && !io_failed.load(Ordering::Relaxed)
        }) {
            self.pending
                .as_mut()
                .expect("document reuse enabled")
                .push_back(ClassifiedPdf {
                    path: path.clone(),
                    stamp,
                    document,
                    snapshot,
                    pages: cached_pages,
                    text_bytes: MAX_CACHED_FILE_TEXT_BYTES - remaining,
                });
        }
        Ok(result)
    }
}

#[derive(Clone)]
struct PdfSnapshot(Arc<Vec<u8>>);

impl PdfSnapshot {
    fn read(source: &mut impl Read, len: usize) -> Option<Self> {
        let mut bytes = vec![0; len];
        source.read_exact(&mut bytes).ok()?;
        at_eof(source).then(|| Self(Arc::new(bytes)))
    }

    fn matches(&self, source: &mut impl Read) -> bool {
        let mut buffer = [0; 16 * 1024];
        for chunk in self.as_ref().chunks(buffer.len()) {
            let current = &mut buffer[..chunk.len()];
            if source.read_exact(current).is_err() || current != chunk {
                return false;
            }
        }
        at_eof(source)
    }
}

impl AsRef<[u8]> for PdfSnapshot {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

fn at_eof(source: &mut impl Read) -> bool {
    loop {
        match source.read(&mut [0]) {
            Ok(read) => return read == 0,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
}

struct ClassifierInput<R> {
    reader: R,
    io_failed: Arc<AtomicBool>,
}

impl<R> ClassifierInput<R> {
    fn new(reader: R, io_failed: &Arc<AtomicBool>) -> Self {
        Self {
            reader,
            io_failed: Arc::clone(io_failed),
        }
    }
}

impl<R: Read> Read for ClassifierInput<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = self.reader.read(buf);
        if !matches!(result, Ok(n) if n == buf.len()) {
            self.io_failed.store(true, Ordering::Relaxed);
        }
        result
    }
}

impl<R: Seek> Seek for ClassifierInput<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.reader
            .seek(pos)
            .inspect_err(|_| self.io_failed.store(true, Ordering::Relaxed))
    }
}

fn check_parse_source(
    request: &ParseRequest,
    version: &str,
    bytes: u64,
) -> Result<(), Box<WorkerDiagnostic>> {
    if version != request.expected_source_version {
        return Err(diagnostic(
            &request.file_path,
            "SOURCE_VERSION_CHANGED",
            false,
        ));
    }
    if bytes > request.max_file_size_bytes {
        return Err(diagnostic(&request.file_path, "FILE_TOO_LARGE", false));
    }
    Ok(())
}

fn classify_open_error(
    path: &str,
    bytes: u64,
    error: PdfiumError,
    io_failed: bool,
) -> Box<WorkerDiagnostic> {
    let native_cause = match error {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::FormatError) => {
            Some("FPDF_ERR_FORMAT")
        }
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
            Some("FPDF_ERR_PASSWORD")
        }
        _ => None,
    };
    let mut failure = parse_error(path, &PdfError::Parse(error));
    // PDFium collapses callback failures into short reads and uses a 32-bit file length on Windows.
    if let Some(code) = native_cause.filter(|_| bytes <= u64::from(u32::MAX) && !io_failed) {
        failure.message = code.into();
    }
    failure
}

fn parse_error(path: &str, error: &PdfError) -> Box<WorkerDiagnostic> {
    let code = match error {
        PdfError::Parse(PdfiumError::PdfiumLibraryInternalError(
            PdfiumInternalError::PasswordError,
        )) => return diagnostic(path, "PDF_PROTECTED", false),
        PdfError::OcrLimits | PdfError::OcrIneligible => "PARSER_INVALID_PAYLOAD",
        PdfError::OcrTimeout => "PARSER_TIMEOUT",
        PdfError::OcrUnavailable
        | PdfError::OcrModelMissing
        | PdfError::OcrModelMismatch
        | PdfError::OcrFailed => "PARSER_FAILED",
        _ => return diagnostic(path, "PARSER_FAILED", false),
    };
    let mut failure = diagnostic(path, code, false);
    failure.message = error.to_string();
    failure
}

fn diagnostic(path: &str, code: &str, retryable: bool) -> Box<WorkerDiagnostic> {
    Box::new(WorkerDiagnostic {
        error_code: code.into(),
        message: "PDF input could not be processed".into(),
        retryable,
        stage: if retryable { "process" } else { "parse" }.into(),
        file_path: Some(path.into()),
        backend: Some("rust_pdfium_v3".into()),
    })
}

/// Match the frozen classifier: whitespace, replacement, controls, format and
/// private-use scalars are not evidence of a text layer (Unicode 15.1).
pub fn is_valid_text_char(c: char) -> bool {
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

#[cfg(test)]
mod tests;
