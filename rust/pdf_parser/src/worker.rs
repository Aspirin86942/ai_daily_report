use crate::{PdfError, PdfReader};
use ai_daily_worker_contract::*;
use pdfium_render::prelude::*;
use std::{
    path::Path,
    time::{Instant, UNIX_EPOCH},
};

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
    pub fn archive_request(
        &self,
        request: &archive::ArchiveRequest,
    ) -> Result<archive::ArchiveArtifact, Box<WorkerDiagnostic>> {
        let started = Instant::now();
        let path = &request.source.file_path;
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
            .visit_pages_with_audit(
                Path::new(path),
                None,
                u32::MAX,
                usize::MAX,
                true,
                |page, images| {
                    match images {
                        Some(true) => {
                            image_pages.push(page.page);
                            writer.gap("PDF_IMAGE_CONTENT_NOT_EXTRACTED");
                        }
                        None => {
                            unchecked_pages.push(page.page);
                            writer.gap("PDF_IMAGE_INSPECTION_BOUNDED");
                        }
                        _ => {}
                    }
                    let range = SourceRange::new(
                        SourceRangeUnit::PdfPage,
                        u64::from(page.page),
                        u64::from(page.page) + 1,
                        None,
                    );
                    if !page.text.chars().any(is_valid_text_char) {
                        no_text_pages.push(page.page);
                        writer.gap("PDF_PAGE_WITHOUT_TEXT");
                    }
                    for line in page.text.lines() {
                        if !writer.push(line, ContentUnitKind::Text, vec![range.clone()]) {
                            return false;
                        }
                    }
                    true
                },
            )
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

    pub fn parse_request(
        &self,
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
        let ParserLimits::Pdf {
            max_pages,
            excerpt_max_chars,
            selected_pages,
        } = &request.parser_limits
        else {
            return Err(diagnostic(
                &request.file_path,
                "PARSER_INVALID_PAYLOAD",
                false,
            ));
        };
        let before = source(&request.file_path)?;
        if before.0 != request.expected_source_version {
            return Err(diagnostic(
                &request.file_path,
                "SOURCE_VERSION_CHANGED",
                false,
            ));
        }
        if before.1 > request.max_file_size_bytes {
            return Err(diagnostic(&request.file_path, "FILE_TOO_LARGE", false));
        }
        let pages = selected_pages
            .as_ref()
            .map(|pages| {
                pages
                    .iter()
                    .map(|n| u32::try_from(*n))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|_| diagnostic(&request.file_path, "PARSER_INVALID_PAYLOAD", false))?;
        let parsed = self
            .read(
                Path::new(&request.file_path),
                pages.as_deref(),
                *max_pages as u32,
                *excerpt_max_chars as usize,
            )
            .map_err(|error| parse_error(&request.file_path, &error))?;
        let mut builder = AuditTextBuilder::new(*excerpt_max_chars, "pdf");
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
        if source(&request.file_path)?.0 != before.0 {
            return Err(diagnostic(
                &request.file_path,
                "SOURCE_VERSION_CHANGED",
                false,
            ));
        }
        let (content, audit, truncated) = builder.finish();
        Ok(ParseResult {
            file_path: request.file_path.clone(),
            file_type: request.file_type.clone(),
            content,
            evidence_audit: Some(audit),
            parser_backend: ParserBackend::RustPdfiumV3,
            worker_lane: WorkerLane::RustPdfProcessV3,
            truncated: truncated || parsed.truncated,
            duration_ms: started.elapsed().as_millis() as u64,
            observed_source_version: before.0,
        })
    }

    pub fn classify_request(&self, request: &ClassifyRequest) -> ClassifyResult {
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
        &self,
        request: &ClassifyRequest,
    ) -> Result<ClassifyResult, Box<WorkerDiagnostic>> {
        request
            .validate()
            .map_err(|_| diagnostic(&request.file_path, "PARSER_INVALID_PAYLOAD", false))?;
        if source(&request.file_path)?.0 != request.source_version {
            return Err(diagnostic(
                &request.file_path,
                "SOURCE_VERSION_CHANGED",
                false,
            ));
        }
        let document = self
            .pdfium
            .load_pdf_from_file(&request.file_path, None)
            .map_err(|error| parse_error(&request.file_path, &PdfError::Parse(error)))?;
        let count = document.pages().len() as u64;
        if count == 0 {
            return Err(diagnostic(&request.file_path, "PARSER_FAILED", false));
        }
        let mut audit = PdfPageAudit {
            policy_version: request.policy_version.clone(),
            checked_pages: Vec::new(),
            observations: Some(Vec::new()),
        };
        let mut found = false;
        for number in pdf_sample_pages(count, request.max_pages, &request.policy_version) {
            let page = document
                .pages()
                .get((number - 1) as PdfPageIndex)
                .map_err(|error| parse_error(&request.file_path, &PdfError::Parse(error)))?;
            found = page
                .text()
                .map_err(|error| parse_error(&request.file_path, &PdfError::Parse(error)))?
                .all()
                .chars()
                .any(is_valid_text_char);
            let mut has_image = Some(false);
            for (index, object) in page.objects().iter().enumerate() {
                if index >= 2048 {
                    has_image = None;
                    break;
                }
                if object.as_image_object().is_some() {
                    has_image = Some(true);
                    break;
                }
                if object.as_x_object_form_object().is_some() {
                    has_image = None;
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
        if source(&request.file_path)?.0 != request.source_version {
            return Err(diagnostic(
                &request.file_path,
                "SOURCE_VERSION_CHANGED",
                false,
            ));
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
    }
}

fn source(path: &str) -> Result<(String, u64), Box<WorkerDiagnostic>> {
    let metadata =
        std::fs::metadata(path).map_err(|_| diagnostic(path, "PARSER_START_FAILED", true))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .ok_or_else(|| diagnostic(path, "SOURCE_VERSION_CHANGED", false))?;
    Ok((
        ai_daily_discovery::build_source_version(modified.as_nanos(), metadata.len()),
        metadata.len(),
    ))
}

fn parse_error(path: &str, error: &PdfError) -> Box<WorkerDiagnostic> {
    let protected = matches!(
        error,
        PdfError::Parse(PdfiumError::PdfiumLibraryInternalError(
            PdfiumInternalError::PasswordError
        ))
    );
    diagnostic(
        path,
        if protected {
            "PDF_PROTECTED"
        } else {
            "PARSER_FAILED"
        },
        false,
    )
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
