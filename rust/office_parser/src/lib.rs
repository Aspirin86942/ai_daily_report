use ai_daily_discovery::build_source_version;
use ai_daily_worker_contract::{
    AuditTextBuilder, ContentUnitKind, ParseRequest, ParseResult, ParsedContentAudit,
    ParserBackend, ParserLimits, SourceRange, SourceRangeUnit, WorkerDiagnostic, WorkerHello,
    WorkerKind, WorkerLane, WorkerOperation, CONTRACT as WORKER_CONTRACT,
    CONTRACT_VERSION as WORKER_CONTRACT_VERSION, PROTOCOL_VERSION as WORKER_PROTOCOL_VERSION,
};
use quick_xml::events::{BytesRef, BytesStart, BytesText, Event};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::PathBuf;
use std::time::{Instant, UNIX_EPOCH};
use zip::read::ZipArchive;

mod captions;
mod charts;
mod legacy;
mod parts;
mod spreadsheet;

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
        evidence_audit: None,
        file_path: request.file_path.to_string_lossy().to_string(),
        file_type: file_type.clone(),
        content: String::new(),
        error: Some(format!("RUST_OFFICE_UNSUPPORTED_EXTENSION: {file_type}")),
        parser_backend: RUST_OFFICE_BACKEND.to_string(),
        truncated: false,
    }
}

enum OpenOffice {
    Word(Box<office_oxide::docx::DocxDocument>),
    Slides(Box<office_oxide::pptx::PptxDocument>),
    Legacy(office_oxide::Document),
}

fn open_office(file: File, format: office_oxide::DocumentFormat) -> Result<OpenOffice, String> {
    use office_oxide::{docx::DocxDocument, pptx::PptxDocument, DocumentFormat};
    // The typed constructors are the same parsers used by Document::from_reader.
    // Owning their results lets rendering move Word blocks and remove notes in place.
    let parse = move || match format {
        DocumentFormat::Docx => DocxDocument::from_reader(file)
            .map(|v| OpenOffice::Word(Box::new(v)))
            .map_err(|e| e.to_string()),
        DocumentFormat::Pptx => PptxDocument::from_reader(file)
            .map(|v| OpenOffice::Slides(Box::new(v)))
            .map_err(|e| e.to_string()),
        _ => office_oxide::Document::from_reader(file, format)
            .map(OpenOffice::Legacy)
            .map_err(|e| e.to_string()),
    };
    // Match the pinned facade's large parse stack on Unix; Windows parses inline.
    #[cfg(unix)]
    if matches!(format, DocumentFormat::Docx | DocumentFormat::Pptx) {
        return std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(parse)
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "Office parser terminated".to_string())?;
    }
    parse()
}

pub fn parse_office_file(request: &OfficeParseRequest) -> FileContextOut {
    let file_type = normalize_file_type(&request.file_type);
    if !is_supported_office_type(&file_type) {
        return unsupported_context(request);
    }

    let max_chars = positive_limit(&request.limits, "document_excerpt_max_chars", 6000);

    if file_type == ".xlsx" {
        return parse_bounded_xlsx(request, &file_type, max_chars);
    }
    if file_type == ".xls" {
        return legacy::parse_xls(request);
    }

    let parts = parts::PartBudget::new(request);
    let format = office_oxide::DocumentFormat::from_extension(file_type.trim_start_matches('.'));
    let opened = File::open(&request.file_path)
        .map_err(|error| error.to_string())
        .and_then(|file| open_office(file, format.expect("supported extension")));
    match opened {
        Ok(document) => {
            let (content, audit, truncated) =
                audited_office_content(document, request, max_chars, parts);
            FileContextOut {
                file_path: request.file_path.to_string_lossy().to_string(),
                file_type,
                content,
                evidence_audit: Some(audit),
                error: None,
                parser_backend: RUST_OFFICE_BACKEND.to_string(),
                truncated,
            }
        }
        Err(error) => FileContextOut {
            evidence_audit: None,
            file_path: request.file_path.to_string_lossy().to_string(),
            file_type,
            content: String::new(),
            error: Some(format!("RUST_OFFICE_PARSE_FAILED: {error}")),
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
            truncated: false,
        },
    }
}

fn audited_office_content(
    document: OpenOffice,
    request: &OfficeParseRequest,
    max_chars: usize,
    mut parts: parts::PartBudget,
) -> (String, ParsedContentAudit, bool) {
    use office_oxide::docx::BlockElement;
    let deadline = parts.deadline;
    let format = normalize_file_type(&request.file_type);
    let mut builder = content_builder(request, max_chars, format.trim_start_matches('.'));
    let mut bounded = !request.limits.contains_key("archive_output") && Instant::now() >= deadline;
    match document {
        OpenOffice::Word(mut docx) => {
            // office_oxide opens the document once. Record all logical positions it parsed;
            // output units obey the existing paragraph/table limits independently.

            let archival = request.limits.contains_key("archive_output");
            let captions = if !archival
                && docx
                    .body
                    .elements
                    .iter()
                    .any(|e| matches!(e, BlockElement::Table(_)))
            {
                let (captions, incomplete) = captions::read(request, &docx, &mut parts)
                    .unwrap_or_else(|| (BTreeMap::new(), true));
                bounded |= incomplete;
                captions
            } else {
                BTreeMap::new()
            };
            if archival && !docx.images.is_empty() {
                builder.gap("DOCX_IMAGE_CONTENT_NOT_EXTRACTED");
            }
            let mut paragraph_number = 0;
            let mut table_number = 0;
            let max_paragraphs = positive_limit(&request.limits, "docx_max_paragraphs", 200);
            let max_tables = positive_limit(&request.limits, "docx_max_tables", 20);
            let max_rows = positive_limit(&request.limits, "docx_table_max_rows", 50);
            let max_cols = positive_limit(&request.limits, "docx_table_max_cols", 12);
            let body = std::mem::take(&mut docx.body.elements);
            let headers_footers = std::mem::take(&mut docx.headers_footers);
            for (element, area) in body.into_iter().map(|e| (e, None)).chain(
                headers_footers
                    .into_iter()
                    .enumerate()
                    .filter(|_| archival)
                    .flat_map(|(index, hf)| {
                        let area = format!(
                            "{} {}",
                            if hf.is_header { "header" } else { "footer" },
                            index + 1
                        );
                        hf.content.into_iter().map(move |e| (e, Some(area.clone())))
                    }),
            ) {
                if !archival && Instant::now() >= deadline {
                    bounded = true;
                    break;
                }
                match element {
                    BlockElement::Paragraph(paragraph) => {
                        paragraph_number += 1;
                        let range = SourceRange::new(
                            SourceRangeUnit::Paragraph,
                            paragraph_number as u64,
                            paragraph_number as u64 + 1,
                            area.clone(),
                        );
                        builder.read(range.clone());
                        if paragraph_number > max_paragraphs {
                            bounded = true;
                            continue;
                        }
                        docx.body.elements = vec![BlockElement::Paragraph(paragraph)];
                        let text = docx.to_markdown();
                        let kind = if text.trim_start().starts_with('#') {
                            ContentUnitKind::Heading
                        } else {
                            ContentUnitKind::Paragraph
                        };
                        for part in text.split("\n\n") {
                            if !builder.push(part, kind, vec![range.clone()]) {
                                bounded = true;
                            }
                        }
                    }
                    BlockElement::Table(table) => {
                        table_number += 1;
                        let label = if let Some(area) = &area {
                            format!("{area}/Table {table_number}")
                        } else {
                            format!("Table {table_number}")
                        };
                        let caption = captions.get(&table_number);
                        let caption_range = caption
                            .and_then(|c| c.paragraph)
                            .map(|n| SourceRange::new(SourceRangeUnit::Paragraph, n, n + 1, None));
                        if table_number <= max_tables {
                            let heading = caption.map_or_else(
                                || format!("## {label}"),
                                |c| format!("## {label} · {}", c.text),
                            );
                            builder.push(
                                &heading,
                                ContentUnitKind::Heading,
                                caption_range.iter().cloned().collect(),
                            );
                        }
                        for (index, row) in table.rows.into_iter().enumerate() {
                            if !archival && Instant::now() >= deadline {
                                bounded = true;
                                break;
                            }
                            if row.cells.is_empty() {
                                continue;
                            }
                            let mut range = SourceRange::new(
                                SourceRangeUnit::TableRow,
                                index as u64 + 1,
                                index as u64 + 2,
                                Some(label.clone()),
                            );
                            range.column_start = Some(1);
                            range.column_end = Some(row.cells.len() as u64 + 1);
                            builder.read(range.clone());
                            if table_number > max_tables || index >= max_rows {
                                bounded = true;
                                continue;
                            }
                            if row.cells.len() > max_cols {
                                bounded = true;
                            }
                            range.column_end = Some(row.cells.len().min(max_cols) as u64 + 1);
                            let cells: Vec<String> = row
                                .cells
                                .into_iter()
                                .take(max_cols)
                                .map(|cell| {
                                    docx.body.elements = cell.content;
                                    escape_markdown_cell(&docx.to_markdown())
                                })
                                .collect();
                            if !cells.iter().any(|cell| !cell.is_empty()) {
                                continue;
                            }
                            let line = format!("| {} |", cells.join(" | "));
                            let kind = if index == 0 {
                                ContentUnitKind::TableHeader
                            } else if line.contains("合计") || line.contains("小计") {
                                ContentUnitKind::Total
                            } else {
                                ContentUnitKind::TableRow
                            };
                            let mut ranges = vec![range];
                            ranges.extend(caption_range.iter().cloned());
                            let line = if let Some(c) = caption {
                                format!("{} {line}", c.text)
                            } else {
                                line
                            };
                            if !builder.push(&line, kind, ranges) {
                                bounded = true;
                            }
                        }
                    }
                }
            }
        }
        OpenOffice::Slides(mut pptx) => {
            let max_slides = positive_limit(&request.limits, "pptx_max_slides", 50);
            let include_notes = request
                .limits
                .get("pptx_include_notes")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let daily = !request.limits.contains_key("archive_output");
            let mut charts = (daily
                && pptx
                    .slides
                    .iter()
                    .take(max_slides)
                    .any(|s| charts::may_have_chart(&s.shapes)))
            .then(|| charts::SlideCharts::open(request, parts))
            .flatten();
            if !include_notes {
                for slide in pptx.slides.iter_mut().take(max_slides) {
                    slide.notes = None;
                }
            }
            for index in 0..pptx.slides.len() {
                if daily && Instant::now() >= deadline {
                    bounded = true;
                    break;
                }
                let range = SourceRange::new(
                    SourceRangeUnit::Slide,
                    index as u64 + 1,
                    index as u64 + 2,
                    Some(
                        if include_notes {
                            "body_and_notes"
                        } else {
                            "body"
                        }
                        .into(),
                    ),
                );
                builder.read(range.clone());
                if index >= max_slides {
                    bounded = true;
                    continue;
                }
                let text = pptx.slide_to_markdown(index).unwrap_or_default();
                for part in text.split("\n\n") {
                    // A slide's actual title is source text. Only the generated
                    // positional heading is decorative metadata.
                    let kind = if part.trim() == format!("## Slide {}", index + 1) {
                        ContentUnitKind::Heading
                    } else {
                        ContentUnitKind::Slide
                    };
                    if !builder.push(part, kind, vec![range.clone()]) {
                        bounded = true;
                    }
                }
                if daily && charts::may_have_chart(&pptx.slides[index].shapes) {
                    // A skipped broken slide in office_oxide must never shift chart attribution.
                    let addition = charts
                        .as_mut()
                        .filter(|_| pptx.presentation.slides.len() == pptx.slides.len())
                        .and_then(|c| {
                            c.append(
                                &pptx.presentation.slides[index].rel_id,
                                index as u64 + 1,
                                &mut builder,
                            )
                        });
                    bounded |= addition.unwrap_or(true);
                }
            }
        }
        OpenOffice::Legacy(document) => {
            if let Some(doc) = document.as_doc() {
                let text = doc.plain_text();
                let mut offset = 0u64;
                let max_paragraphs = positive_limit(&request.limits, "docx_max_paragraphs", 200);
                for (index, part) in text.split_inclusive('\n').enumerate() {
                    let end = offset + part.chars().count() as u64;
                    if end > offset {
                        let range = SourceRange::new(
                            SourceRangeUnit::TextChar,
                            offset,
                            end,
                            Some("extracted_main_text".into()),
                        );
                        builder.read(range.clone());
                        if index < max_paragraphs {
                            if !builder.push(part, ContentUnitKind::Paragraph, vec![range]) {
                                bounded = true;
                            }
                        } else {
                            bounded = true;
                        }
                    }
                    offset = end;
                }
            } else if let Some(ppt) = document.as_ppt() {
                let max_slides = positive_limit(&request.limits, "pptx_max_slides", 50);
                let notes = request
                    .limits
                    .get("pptx_include_notes")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                for (index, slide) in ppt.slides.iter().enumerate() {
                    let range = SourceRange::new(
                        SourceRangeUnit::Slide,
                        index as u64 + 1,
                        index as u64 + 2,
                        None,
                    );
                    builder.read(range.clone());
                    if index >= max_slides {
                        bounded = true;
                        continue;
                    }
                    for run in &slide.text_runs {
                        if (notes || run.text_type != office_oxide::ppt::TextType::Notes)
                            && !builder.push(&run.text, ContentUnitKind::Slide, vec![range.clone()])
                        {
                            bounded = true;
                        }
                    }
                }
            }
        }
    }
    let (content, audit, truncated) = builder.finish();
    (content, audit, bounded || truncated)
}

pub fn parse_worker_request(request: &ParseRequest) -> Result<ParseResult, Box<WorkerDiagnostic>> {
    parse_request_inner(request, None)
}

pub fn parse_archive_request(
    request: &ai_daily_worker_contract::archive::ArchiveRequest,
) -> Result<ai_daily_worker_contract::archive::ArchiveArtifact, Box<WorkerDiagnostic>> {
    request.validate().map_err(|message| {
        Box::new(worker_error(
            &request.source,
            "PARSER_INVALID_PAYLOAD",
            message,
            false,
        ))
    })?;
    let parsed = parse_request_inner(&request.source, Some(request))?;
    let mut artifact = parsed
        .evidence_audit
        .and_then(|audit| audit.archive)
        .ok_or_else(|| {
            Box::new(worker_error(
                &request.source,
                "ARCHIVE_OUTPUT_FAILED",
                "archive output could not be created".into(),
                true,
            ))
        })?;
    if artifact.error.is_some() {
        return Err(Box::new(worker_error(
            &request.source,
            "ARCHIVE_OUTPUT_FAILED",
            "archive output could not be persisted".into(),
            true,
        )));
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
    archive: Option<&ai_daily_worker_contract::archive::ArchiveRequest>,
) -> Result<ParseResult, Box<WorkerDiagnostic>> {
    let started_at = Instant::now();
    if let Err(message) = request.validate() {
        return Err(Box::new(worker_error(
            request,
            "PARSER_INVALID_PAYLOAD",
            message,
            false,
        )));
    }
    if request.backend.lane() != WorkerLane::RustOfficeProcessV3 {
        return Err(Box::new(worker_error(
            request,
            "PARSER_INVALID_PAYLOAD",
            "backend is not supported by the Office worker".to_string(),
            false,
        )));
    }

    let path = PathBuf::from(&request.file_path);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(Box::new(worker_error(
                request,
                "PARSER_FAILED",
                format!("file metadata is unavailable: {error}"),
                false,
            )));
        }
    };
    let observed_before = match metadata_source_version(&metadata) {
        Ok(value) => value,
        Err(error) => {
            return Err(Box::new(worker_error(
                request,
                "PARSER_FAILED",
                format!("file source version is unavailable: {error}"),
                false,
            )));
        }
    };
    if metadata.len() > request.max_file_size_bytes {
        return Err(Box::new(worker_error(
            request,
            "FILE_TOO_LARGE",
            "file exceeds the configured size limit".to_string(),
            false,
        )));
    }
    if observed_before != request.expected_source_version {
        return Err(Box::new(worker_error(
            request,
            "SOURCE_VERSION_CHANGED",
            "file source version changed before parsing".to_string(),
            true,
        )));
    }

    let mut detected_request = request.clone();
    let (format, backend) = legacy::detect_office_format(&path).map_err(|reason| {
        let message = match reason {
            legacy::OfficeFormatError::MissingContent => {
                ai_daily_worker_contract::OFFICE_CONTENT_MISSING_MESSAGE
            }
            legacy::OfficeFormatError::Invalid => "Office format is invalid",
        };
        Box::new(worker_error(
            request,
            "PARSER_FAILED",
            message.into(),
            false,
        ))
    })?;
    if matches!(format, ".doc" | ".ppt")
        && !matches!(
            request.parser_limits,
            ParserLimits::Office {
                legacy_office_enabled: true,
                ..
            }
        )
    {
        return Err(Box::new(worker_error(
            request,
            "UNSUPPORTED_EXTENSION",
            "Legacy Office parsing is disabled".into(),
            false,
        )));
    }
    if archive.is_none() {
        detected_request.remaining_timeout_ms = request
            .remaining_timeout_ms
            .saturating_sub(elapsed_ms(started_at))
            .max(1);
    }
    detected_request.file_type = format.into();
    detected_request.backend = backend;
    let (context, retryable) = parse_strict_office_context(&detected_request, archive);
    let observed_after =
        match std::fs::metadata(&path).and_then(|value| metadata_source_version(&value)) {
            Ok(value) => value,
            Err(_) => {
                return Err(Box::new(worker_error(
                    request,
                    "SOURCE_VERSION_CHANGED",
                    "file source version became unavailable during parsing".to_string(),
                    true,
                )));
            }
        };
    if observed_after != observed_before {
        return Err(Box::new(worker_error(
            request,
            "SOURCE_VERSION_CHANGED",
            "file source version changed during parsing".to_string(),
            true,
        )));
    }
    if context.error.is_some() {
        return Err(Box::new(worker_error(
            request,
            "PARSER_FAILED",
            "Office parser reported an error".to_string(),
            retryable,
        )));
    }

    Ok(ParseResult {
        file_path: request.file_path.clone(),
        file_type: request.file_type.clone(),
        content: context.content,
        evidence_audit: context.evidence_audit,
        parser_backend: backend,
        worker_lane: WorkerLane::RustOfficeProcessV3,
        truncated: context.truncated,
        duration_ms: elapsed_ms(started_at),
        observed_source_version: observed_after,
    })
}

fn parse_strict_office_context(
    request: &ParseRequest,
    archive: Option<&ai_daily_worker_contract::archive::ArchiveRequest>,
) -> (FileContextOut, bool) {
    let mut limits = office_limits_map(&request.parser_limits);
    limits.insert(
        "remaining_timeout_ms".into(),
        request.remaining_timeout_ms.into(),
    );
    if let Some(archive) = archive {
        limits.insert(
            "archive_output".into(),
            serde_json::to_value(archive).unwrap(),
        );
        limits.insert(
            "document_excerpt_max_chars".into(),
            serde_json::json!(archive.max_output_bytes),
        );
    }
    let office_request = OfficeParseRequest {
        file_path: PathBuf::from(&request.file_path),
        file_type: request.file_type.clone(),
        limits,
        parser_backend: request.backend.as_str().to_string(),
    };
    if request.backend == ParserBackend::RustXlsCalamineV3 {
        return (legacy::parse_xls(&office_request), false);
    }
    if request.backend == ParserBackend::RustLegacyOfficeV3 {
        return (parse_office_file(&office_request), false);
    }
    if request.backend == ParserBackend::RustXlsxBoundedV2 {
        let max_chars = positive_limit(&office_request.limits, "document_excerpt_max_chars", 6000);
        return match parse_bounded_xlsx_inner(&office_request, max_chars) {
            Ok((content, truncated, audit)) => (
                FileContextOut {
                    file_path: request.file_path.clone(),
                    file_type: request.file_type.clone(),
                    content,
                    evidence_audit: Some(audit),
                    error: None,
                    parser_backend: RUST_XLSX_BOUNDED_BACKEND.to_string(),
                    truncated,
                },
                false,
            ),
            Err(error) => {
                let retryable = !error.is_deterministic();
                (
                    FileContextOut {
                        evidence_audit: None,
                        file_path: request.file_path.clone(),
                        file_type: request.file_type.clone(),
                        content: String::new(),
                        error: Some(format!("RUST_XLSX_BOUNDED_PARSE_FAILED: {error}")),
                        parser_backend: RUST_XLSX_BOUNDED_BACKEND.to_string(),
                        truncated: false,
                    },
                    retryable,
                )
            }
        };
    }
    if let Err((message, retryable)) = validate_ooxml_zip(&office_request.file_path) {
        return (
            FileContextOut {
                evidence_audit: None,
                file_path: request.file_path.clone(),
                file_type: request.file_type.clone(),
                content: String::new(),
                error: Some(message),
                parser_backend: RUST_OFFICE_BACKEND.to_string(),
                truncated: false,
            },
            retryable,
        );
    }
    (parse_office_file(&office_request), true)
}

fn content_builder(
    request: &OfficeParseRequest,
    max_chars: usize,
    format: &str,
) -> AuditTextBuilder {
    if let Some(value) = request.limits.get("archive_output") {
        if let Ok(output) = serde_json::from_value::<
            ai_daily_worker_contract::archive::ArchiveRequest,
        >(value.clone())
        {
            // Missing artifact is rejected by the archive operation, never accepted as an excerpt.
            return AuditTextBuilder::for_archive(
                &output.output_dir,
                output.max_output_bytes,
                format,
            )
            .unwrap_or_else(|_| AuditTextBuilder::new(0, format));
        }
    }
    AuditTextBuilder::new(max_chars as u64, format)
}

fn validate_ooxml_zip(file_path: &PathBuf) -> Result<(), (String, bool)> {
    let file = File::open(file_path)
        .map_err(|error| (format!("RUST_OFFICE_OPEN_FAILED: {error}"), true))?;
    ZipArchive::new(file).map(|_| ()).map_err(|error| {
        let retryable = matches!(error, zip::result::ZipError::Io(_));
        (format!("RUST_OFFICE_CORRUPT_ZIP: {error}"), retryable)
    })
}

fn office_limits_map(limits: &ParserLimits) -> BTreeMap<String, serde_json::Value> {
    let ParserLimits::Office {
        excel_max_sheets,
        excel_max_rows,
        excel_max_columns,
        docx_max_paragraphs,
        docx_max_tables,
        docx_table_max_rows,
        docx_table_max_cols,
        pptx_max_slides,
        pptx_include_notes,
        document_excerpt_max_chars,
        ..
    } = limits
    else {
        return BTreeMap::new();
    };
    BTreeMap::from([
        (
            "excel_max_sheets".to_string(),
            serde_json::json!(excel_max_sheets),
        ),
        (
            "excel_max_rows".to_string(),
            serde_json::json!(excel_max_rows),
        ),
        (
            "excel_max_columns".to_string(),
            serde_json::json!(excel_max_columns),
        ),
        (
            "docx_max_paragraphs".to_string(),
            serde_json::json!(docx_max_paragraphs),
        ),
        (
            "docx_max_tables".to_string(),
            serde_json::json!(docx_max_tables),
        ),
        (
            "docx_table_max_rows".to_string(),
            serde_json::json!(docx_table_max_rows),
        ),
        (
            "docx_table_max_cols".to_string(),
            serde_json::json!(docx_table_max_cols),
        ),
        (
            "pptx_max_slides".to_string(),
            serde_json::json!(pptx_max_slides),
        ),
        (
            "pptx_include_notes".to_string(),
            serde_json::json!(pptx_include_notes),
        ),
        (
            "document_excerpt_max_chars".to_string(),
            serde_json::json!(document_excerpt_max_chars),
        ),
    ])
}

fn worker_error(
    request: &ParseRequest,
    error_code: &str,
    message: String,
    retryable: bool,
) -> WorkerDiagnostic {
    let message: String = message.chars().take(4096).collect();
    WorkerDiagnostic {
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
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn local_office_worker_build_uses_the_shared_source_fingerprint() {
        let build = worker_hello().worker_build;
        if let Some(ci_build) =
            option_env!("AI_DAILY_BUILD_ID").filter(|value| !value.trim().is_empty())
        {
            assert_eq!(build, ci_build);
        } else {
            let digest = build
                .strip_prefix("sha256-source-v1:")
                .expect("local build must use the source hash prefix");
            assert_eq!(digest.len(), 64);
            assert!(digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        }
    }

    fn write_test_xlsx(parts: &[(&str, &str)]) -> PathBuf {
        static NEXT_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let sequence = NEXT_FILE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bounded-xlsx-{}-{unique}-{sequence}.xlsx",
            std::process::id()
        ));
        let file = File::create_new(&path).expect("test xlsx should be creatable");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        for (name, body) in parts {
            zip.start_file(name, options)
                .expect("zip entry should be writable");
            zip.write_all(body.as_bytes())
                .expect("zip entry body should be writable");
        }
        zip.finish().expect("zip should finish");
        path
    }

    fn minimal_xlsx_parts() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Data" sheetId="1" r:id="rId1"/>
    <sheet name="Second" sheetId="2" r:id="rId2"/>
  </sheets>
</workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="worksheet" Target="worksheets/sheet2.xml"/>
</Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <si><t>Alpha</t></si>
  <si><t>unused-over-budget</t></si>
</sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><t>Name</t></is></c>
      <c r="B1" t="inlineStr"><is><t>Amount</t></is></c>
      <c r="C1" t="inlineStr"><is><t>hidden-over-budget</t></is></c>
    </row>
    <row r="2">
      <c r="A2" t="s"><v>0</v></c>
      <c r="B2"><v>10</v></c>
    </row>
    <row r="3">
      <c r="A3" t="inlineStr"><is><t>row-over-budget</t></is></c>
    </row>
  </sheetData>
</worksheet>"#,
            ),
            (
                "xl/worksheets/sheet2.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>second-sheet-over-budget</t></is></c></row>
  </sheetData>
</worksheet>"#,
            ),
        ]
    }

    fn bounded_test_request(path: &Path) -> OfficeParseRequest {
        OfficeParseRequest {
            file_path: path.to_path_buf(),
            file_type: ".xlsx".into(),
            limits: BTreeMap::from([
                ("excel_max_sheets".into(), serde_json::json!(1)),
                ("excel_max_rows".into(), serde_json::json!(5)),
                ("excel_max_columns".into(), serde_json::json!(2)),
                ("document_excerpt_max_chars".into(), serde_json::json!(2000)),
            ]),
            parser_backend: RUST_OFFICE_BACKEND.into(),
        }
    }

    #[test]
    fn lowercase_xlsx_parts_resolve_shared_strings_with_original_row_limits() {
        let mut parts = minimal_xlsx_parts();
        parts[2].0 = "xl/sharedstrings.xml";
        parts[3].0 = "xl/worksheets/SHEET1.XML";
        let path = write_test_xlsx(&parts);
        let mut request = bounded_test_request(&path);
        request
            .limits
            .insert("excel_max_rows".into(), serde_json::json!(2));
        let parsed = parse_office_file(&request);
        std::fs::remove_file(path).ok();
        assert!(parsed.error.is_none());
        assert!(parsed.content.contains("| Alpha | 10 |"));
        assert!(!parsed.content.contains("#SHARED_STRING"));
        assert!(!parsed.content.contains("row-over-budget"));
        assert_eq!(parsed.evidence_audit.unwrap().read_ranges.unwrap().len(), 2);
    }

    #[test]
    fn missing_and_ambiguous_shared_strings_fail_without_placeholder_content() {
        for ambiguous in [false, true] {
            let mut parts = minimal_xlsx_parts();
            if ambiguous {
                parts.push(("xl/sharedstrings.xml", parts[2].1));
            } else {
                parts.remove(2);
            }
            let path = write_test_xlsx(&parts);
            let parsed = parse_office_file(&bounded_test_request(&path));
            std::fs::remove_file(path).ok();
            let error = parsed.error.unwrap();
            assert!(error.contains(if ambiguous {
                "ambiguous case"
            } else {
                "missing"
            }));
            assert!(parsed.content.is_empty());
            assert!(parsed.evidence_audit.is_none());
        }
    }

    #[test]
    fn missing_referenced_shared_string_fails_instead_of_returning_usable_placeholder() {
        let original = minimal_xlsx_parts();
        let invalid_sheet = original[3].1.replace("<v>0</v>", "<v>9</v>");
        let parts: Vec<_> = original
            .iter()
            .enumerate()
            .map(|(index, (name, body))| {
                (
                    *name,
                    if index == 3 {
                        invalid_sheet.as_str()
                    } else {
                        *body
                    },
                )
            })
            .collect();
        let path = write_test_xlsx(&parts);
        let parsed = parse_office_file(&bounded_test_request(&path));
        std::fs::remove_file(path).ok();
        assert!(parsed
            .error
            .unwrap()
            .contains("referenced shared string is missing"));
        assert!(parsed.content.is_empty());
    }

    #[test]
    fn header_with_total_column_stays_header_and_vertical_sum_keeps_cached_value() {
        let mut parts = minimal_xlsx_parts();
        parts[3].1 = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>姓名</t></is></c><c r="B1" t="inlineStr"><is><t>代扣代缴合计</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>员工甲</t></is></c><c r="B2"><v>20.25</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>员工乙</t></is></c><c r="B3"><v>30.5</v></c></row>
          <row r="4"><c r="B4"><f>SUM(B2:B3)</f><v>50.75</v></c></row>
          <row r="5"><c r="A5"><v>2</v></c><c r="B5"><f>SUM(A5:A5)</f><v>2</v></c></row>
        </sheetData></worksheet>"#;
        let path = write_test_xlsx(&parts);
        let parsed = parse_office_file(&bounded_test_request(&path));
        std::fs::remove_file(path).ok();
        assert!(parsed.error.is_none());
        let audit = parsed.evidence_audit.unwrap();
        let units = audit.content_units.unwrap();
        assert!(units
            .iter()
            .filter(|u| u.ranges.first().is_some_and(|r| r.start == 1))
            .all(|u| u.kind == ContentUnitKind::TableHeader));
        assert!(units.iter().any(|u| u.kind == ContentUnitKind::TableRow
            && u.ranges.first().is_some_and(|r| r.start == 2)));
        let total = units
            .iter()
            .find(|u| {
                u.kind == ContentUnitKind::Total
                    && u.ranges.first().is_some_and(|range| range.start == 4)
            })
            .unwrap();
        assert_eq!(total.ranges[0].start, 4);
        assert!(parsed
            .content
            .chars()
            .skip(total.text_start as usize)
            .take((total.text_end - total.text_start) as usize)
            .collect::<String>()
            .contains("50.75"));
        assert!(units.iter().any(|u| u.kind == ContentUnitKind::TableRow
            && u.ranges.first().is_some_and(|r| r.start == 5)));
    }

    #[test]
    fn xlsx_character_budget_keeps_later_sheet_data_and_total_before_bulk_rows() {
        let original = minimal_xlsx_parts();
        let bulk = (2..=35).map(|row| format!(r#"<row r="{row}"><c r="A{row}" t="inlineStr"><is><t>条目{row}附带较长的常规说明</t></is></c><c r="B{row}"><v>10</v></c></row>"#)).collect::<String>();
        let first = format!(
            r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>{bulk}</sheetData></worksheet>"#
        );
        let second = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>另一表甲</t></is></c><c r="B2"><v>1234.56</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>另一表乙</t></is></c><c r="B3"><v>20.25</v></c></row>
          <row r="4"><c r="B4"><f>SUM(B2:B3)</f><v>1254.81</v></c></row>
        </sheetData></worksheet>"#;
        let parts = [
            original[0],
            original[1],
            original[2],
            (original[3].0, first.as_str()),
            (original[4].0, second),
        ];
        let path = write_test_xlsx(&parts);
        let mut request = bounded_test_request(&path);
        request
            .limits
            .insert("excel_max_sheets".into(), serde_json::json!(2));
        request
            .limits
            .insert("excel_max_rows".into(), serde_json::json!(50));
        request
            .limits
            .insert("document_excerpt_max_chars".into(), serde_json::json!(600));
        let parsed = parse_office_file(&request);
        std::fs::remove_file(path).ok();
        assert!(parsed.error.is_none() && parsed.truncated, "{parsed:?}");
        assert!(parsed.content.chars().count() <= 600);
        assert!(parsed.content.contains("## Sheet: Second"));
        assert!(parsed.content.contains("1234.56") && parsed.content.contains("1254.81"));
        let audit = parsed.evidence_audit.unwrap();
        audit.validate(&parsed.content).unwrap();
        assert!(audit.content_units.unwrap().iter().any(|unit| unit.kind
            == ContentUnitKind::Total
            && unit
                .ranges
                .iter()
                .any(|range| range.label.as_deref() == Some("Second") && range.start == 4)));
    }

    #[test]
    fn xlsx_window_summary_states_only_read_rows_and_repeated_values() {
        let original = minimal_xlsx_parts();
        let sheet = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>月份</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>2024年1月</t></is></c><c r="B2"><v>2360</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>2024年2月</t></is></c><c r="B3"><v>2360</v></c></row>
          <row r="4"><c r="A4" t="inlineStr"><is><t>2024年3月</t></is></c><c r="B4"><v>9999</v></c></row>
        </sheetData></worksheet>"#;
        let parts = [
            original[0],
            original[1],
            original[2],
            (original[3].0, sheet),
            original[4],
        ];
        let path = write_test_xlsx(&parts);
        let mut request = bounded_test_request(&path);
        request
            .limits
            .insert("excel_max_rows".into(), serde_json::json!(3));
        let parsed = parse_office_file(&request);
        std::fs::remove_file(path).ok();
        assert!(parsed.error.is_none() && parsed.truncated);
        assert!(parsed.content.contains("已读数据行 2 条"));
        assert!(parsed.content.contains("这些行的共同值：Amount = 2360"));
        assert!(parsed.content.contains("逐行 月份 = 2024年1月、2024年2月"));
        assert!(!parsed.content.contains("9999") && !parsed.content.contains("2024年3月"));
        let audit = parsed.evidence_audit.unwrap();
        audit.validate(&parsed.content).unwrap();
        assert!(audit.content_units.unwrap().iter().any(|unit| unit.kind
            == ContentUnitKind::TableRow
            && unit
                .ranges
                .iter()
                .any(|range| range.start == 2 && range.end == 4)));
    }

    #[test]
    fn byte_windows_keep_only_closed_rows_and_shared_strings() {
        let first = "<worksheet><sheetData><row r=\"1\"><c t=\"str\"><v>完整</v></c></row>";
        let second = "<row r=\"2\"><c t=\"str\"><v>采购&amp;交付</v></c></row>";
        let xml = format!("{first}{second}</sheetData></worksheet>");
        for n in first.len()..xml.len() {
            let mut numbers = Vec::new();
            assert!(visit_sheet_rows(
                &xml.as_bytes()[..n],
                10,
                10,
                false,
                None,
                Some(n as u64),
                |number, _| {
                    numbers.push(number);
                    Ok(true)
                }
            )
            .unwrap_or_else(|e| panic!("row cut {n}: {e}")));
            assert_eq!(
                numbers,
                if n >= first.len() + second.len() {
                    vec![1, 2]
                } else {
                    vec![1]
                }
            );
        }
        let first = "<sst><si><t>完整</t></si>";
        let second = "<si><r><t>采购&amp;交付</t></r></si>";
        let xml = format!("{first}{second}</sst>");
        for n in first.len()..xml.len() {
            let (strings, limited) =
                read_string_table(&xml.as_bytes()[..n], 10, None, Some(n as u64))
                    .unwrap_or_else(|e| panic!("string cut {n}: {e}"));
            assert!(limited);
            assert_eq!(strings[&0], "完整");
            assert_eq!(
                strings.get(&1).map(String::as_str),
                (n >= first.len() + second.len()).then_some("采购&交付")
            );
        }
        assert!(read_string_table(b"<sst><si><t>x".as_slice(), 10, None, None).is_err());
        let invalid = b"<sst><si><t>\xff</t></si></sst>";
        assert!(read_string_table(invalid.as_slice(), 10, None, None).is_err());
        let bad_end = b"<sst><si></bad>";
        assert!(
            read_string_table(bad_end.as_slice(), 10, None, Some(bad_end.len() as u64)).is_err()
        );
        assert!(visit_sheet_rows(
            b"<worksheet><row><c>".as_slice(),
            10,
            10,
            false,
            None,
            None,
            |_, _| Ok(true)
        )
        .is_err());
    }

    #[test]
    fn expired_worksheet_deadline_emits_no_unread_rows() {
        let xml = b"<worksheet><sheetData><row><c><v>7</v></c></row></sheetData></worksheet>";
        let limited = visit_sheet_rows(
            xml.as_slice(),
            10,
            10,
            false,
            Some(Instant::now()),
            None,
            |_, _| panic!("expired worksheet scan emitted a row"),
        )
        .unwrap();
        assert!(limited);
    }

    #[test]
    fn small_workbook_keeps_every_source_row_before_optional_statistics() {
        let mut parts = minimal_xlsx_parts();
        parts[0].1 = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" r:id="rId1"/></sheets></workbook>"#;
        parts[3].1 = r#"<worksheet><sheetData>
          <row><c t="str"><v>Name</v></c><c t="str"><v>Amount</v></c></row>
          <row><c t="str"><v>A</v></c><c><v>10</v></c></row>
          <row><c t="str"><v>B</v></c><c><v>20</v></c></row>
          <row><c t="str"><v>C</v></c><c><v>30</v></c></row>
        </sheetData></worksheet>"#;
        let path = write_test_xlsx(&parts);
        let mut request = bounded_test_request(&path);
        request
            .limits
            .insert("document_excerpt_max_chars".into(), 105.into());
        let parsed = parse_office_file(&request);
        std::fs::remove_file(path).unwrap();
        assert!(parsed.error.is_none() && !parsed.truncated);
        for line in [
            "| Name | Amount |",
            "| A | 10 |",
            "| B | 20 |",
            "| C | 30 |",
        ] {
            assert!(parsed.content.contains(line), "{}", parsed.content);
        }
    }

    #[test]
    fn supported_office_type_is_case_insensitive() {
        assert!(is_supported_office_type(".DOCX"));
        assert!(is_supported_office_type(".xlsx"));
        assert!(is_supported_office_type(".PPT"));
        assert!(!is_supported_office_type(".pdf"));
    }

    #[test]
    fn wrapped_vertical_sum_is_a_total_but_horizontal_sum_is_not() {
        assert!(vertical_sum_formula("ROUND(SUM($B$2:$B$8),2)", 9));
        assert!(vertical_sum_formula("SUBTOTAL(109,B2:B8)", 9));
        assert!(!vertical_sum_formula("ROUND(SUM(B9:H9),2)", 9));
        assert!(!vertical_sum_formula("SUM(B2:B9)", 9));
    }

    #[test]
    fn formula_zero_padding_does_not_hide_months_or_a_named_zero_amount() {
        let original = minimal_xlsx_parts();
        let sheet = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>月份</t></is></c><c r="B1" t="inlineStr"><is><t>Amount</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>2024年1月</t></is></c><c r="B2"><v>2360</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>2024年2月</t></is></c><c r="B3"><v>2360</v></c></row>
          <row r="4"><c r="B4"><f>SUM(C4:H4)</f><v>0</v></c></row>
          <row r="5"><c r="A5" t="inlineStr"><is><t>2024年3月</t></is></c><c r="B5"><v>0</v></c></row>
        </sheetData></worksheet>"#;
        let parts = [
            original[0],
            original[1],
            original[2],
            (original[3].0, sheet),
            original[4],
        ];
        let path = write_test_xlsx(&parts);
        let parsed = parse_office_file(&bounded_test_request(&path));
        std::fs::remove_file(path).ok();
        assert!(parsed.error.is_none());
        assert!(parsed.content.contains("已读数据行 3 条"));
        assert!(parsed.content.contains("| 2024年3月 | 0 |"));
        let audit = parsed.evidence_audit.unwrap();
        audit.validate(&parsed.content).unwrap();
        assert!(audit.read_ranges.unwrap().iter().any(|r| r.start == 4));
        assert!(!audit
            .content_units
            .unwrap()
            .iter()
            .any(|unit| unit.kind == ContentUnitKind::TableRow
                && unit.ranges.iter().any(|r| r.start == 4)));
    }

    #[test]
    fn repeated_names_get_separate_bounded_summaries_with_nonadjacent_ranges() {
        let original = minimal_xlsx_parts();
        let sheet = r#"<worksheet><sheetData>
          <row r="1"><c r="A1" t="inlineStr"><is><t>Name</t></is></c><c r="B1" t="inlineStr"><is><t>月份</t></is></c><c r="C1" t="inlineStr"><is><t>Amount</t></is></c></row>
          <row r="2"><c r="A2" t="inlineStr"><is><t>Alpha</t></is></c><c r="B2" t="inlineStr"><is><t>2024-01</t></is></c><c r="C2"><v>2360</v></c></row>
          <row r="3"><c r="A3" t="inlineStr"><is><t>Beta</t></is></c><c r="B3" t="inlineStr"><is><t>2024-01</t></is></c><c r="C3"><v>5000</v></c></row>
          <row r="4"><c r="A4" t="inlineStr"><is><t>Alpha</t></is></c><c r="B4" t="inlineStr"><is><t>2024-02</t></is></c><c r="C4"><v>2360</v></c></row>
          <row r="5"><c r="A5" t="inlineStr"><is><t>Beta</t></is></c><c r="B5" t="inlineStr"><is><t>2024-02</t></is></c><c r="C5"><v>5000</v></c></row>
          <row r="6"><c r="A6" t="inlineStr"><is><t>Alpha</t></is></c><c r="B6" t="inlineStr"><is><t>2024-03</t></is></c><c r="C6"><v>9999</v></c></row>
        </sheetData></worksheet>"#;
        let parts = [
            original[0],
            original[1],
            original[2],
            (original[3].0, sheet),
            original[4],
        ];
        let path = write_test_xlsx(&parts);
        let mut request = bounded_test_request(&path);
        request
            .limits
            .insert("excel_max_rows".into(), serde_json::json!(5));
        request
            .limits
            .insert("excel_max_columns".into(), serde_json::json!(3));
        let parsed = parse_office_file(&request);
        std::fs::remove_file(path).ok();
        assert!(parsed.error.is_none() && parsed.truncated);
        let audit = parsed.evidence_audit.unwrap();
        audit.validate(&parsed.content).unwrap();
        let chars: Vec<_> = parsed.content.chars().collect();
        let group = audit
            .content_units
            .unwrap()
            .into_iter()
            .find(|unit| {
                unit.kind == ContentUnitKind::TableRow
                    && chars[unit.text_start as usize..unit.text_end as usize]
                        .iter()
                        .collect::<String>()
                        .contains("Name = Alpha")
            })
            .unwrap();
        let text: String = chars[group.text_start as usize..group.text_end as usize]
            .iter()
            .collect();
        assert!(text.contains("已读数据行 2 条"));
        assert!(text.contains("逐行 月份 = 2024-01、2024-02"));
        assert!(text.contains("Amount = 2360"));
        assert!(!text.contains("5000") && !parsed.content.contains("9999"));
        assert_eq!(
            group
                .ranges
                .iter()
                .map(|range| (range.start, range.end))
                .collect::<Vec<_>>(),
            vec![(2, 3), (4, 5)]
        );
    }

    #[test]
    fn amount_series_keeps_each_value_with_its_own_person_months_and_rows() {
        let row = |number, cells: &[&str], kind| XlsxRow {
            text: cells.join(" | "),
            kind,
            ranges: vec![SourceRange::new(
                SourceRangeUnit::SheetRow,
                number,
                number + 1,
                Some("Data".into()),
            )],
            separator: None,
            cells: cells.iter().map(|cell| (*cell).into()).collect(),
        };
        let rows = vec![
            row(
                1,
                &["Name", "Month", "Amount"],
                ContentUnitKind::TableHeader,
            ),
            row(2, &["Alpha", "2024-01", "2360"], ContentUnitKind::TableRow),
            row(3, &["Alpha", "2024-02", "2360"], ContentUnitKind::TableRow),
            row(4, &["Alpha", "2024-03", "30000"], ContentUnitKind::TableRow),
            row(5, &["Alpha", "2024-04", "30000"], ContentUnitKind::TableRow),
            row(6, &["Beta", "2024-01", "90000"], ContentUnitKind::TableRow),
            row(7, &["Beta", "2024-02", "90000"], ContentUnitKind::TableRow),
        ];
        let summaries = xlsx_window_summaries(&rows);
        let first = summaries
            .iter()
            .find(|row| row.text.contains("Name = Alpha；Amount = 2360"))
            .unwrap();
        assert!(first
            .text
            .contains("Amount = 2360（对应Month = 2024-01、2024-02）"));
        assert!(first
            .text
            .contains("Amount = 30000（对应Month = 2024-03、2024-04）"));
        assert!(!first.text.contains("90000"));
        assert_eq!((first.ranges[0].start, first.ranges[0].end), (2, 6));
    }

    #[test]
    fn multirow_headers_keep_identity_period_and_salary_fields_together() {
        let row = |number, cells: &[&str], kind| XlsxRow {
            text: cells.join(" | "),
            kind,
            ranges: vec![SourceRange::new(
                SourceRangeUnit::SheetRow,
                number,
                number + 1,
                Some("Payroll".into()),
            )],
            separator: None,
            cells: cells.iter().map(|cell| (*cell).into()).collect(),
        };
        let continuation = ["", "", "出勤系数（天数）", "应付工资"];
        assert!(is_header_row(
            &continuation.iter().map(|s| (*s).into()).collect::<Vec<_>>()
        ));
        let rows = vec![
            row(1, &["Name", "Month", "", ""], ContentUnitKind::TableHeader),
            row(2, &continuation, ContentUnitKind::TableHeader),
            row(
                3,
                &["Alpha", "2024-01", "1", "100"],
                ContentUnitKind::TableRow,
            ),
            row(
                4,
                &["Alpha", "2024-02", "1", "200"],
                ContentUnitKind::TableRow,
            ),
            row(
                5,
                &["Beta", "2024-01", "1", "999"],
                ContentUnitKind::TableRow,
            ),
            row(6, &["", "", "1", "300"], ContentUnitKind::TableRow),
        ];
        let summaries = xlsx_window_summaries(&rows);
        let series = summaries
            .iter()
            .find(|row| row.text.contains("Name = Alpha；应付工资"))
            .expect("the continuation header must retain the identity and period columns");
        assert!(series
            .text
            .contains("应付工资 = 100（对应Month = 2024-01）"));
        assert!(series
            .text
            .contains("应付工资 = 200（对应Month = 2024-02）"));
        assert!(!series.text.contains("999") && !series.text.contains("300"));
        let projected = xlsx_unidentified_numeric_rows(&rows);
        assert_eq!(projected.len(), 1);
        assert!(projected[0].text.contains("应付工资 = 300"));
    }

    #[test]
    fn unnamed_numeric_rows_keep_header_fields_without_becoming_totals() {
        let row = |number, cells: &[&str], kind| XlsxRow {
            text: cells.join(" | "),
            kind,
            ranges: vec![SourceRange::new(
                SourceRangeUnit::SheetRow,
                number,
                number + 1,
                Some("Payroll".into()),
            )],
            separator: None,
            cells: cells.iter().map(|cell| (*cell).into()).collect(),
        };
        let rows = vec![
            row(1, &["", "应付工资", ""], ContentUnitKind::TableHeader),
            row(
                2,
                &["姓名", "", "代扣代缴合计"],
                ContentUnitKind::TableHeader,
            ),
            row(3, &["Alpha", "10", "2"], ContentUnitKind::TableRow),
            row(4, &["", "13", "5"], ContentUnitKind::TableRow),
            row(5, &["", "0", "0"], ContentUnitKind::Placeholder),
            row(6, &["合计", "13", "5"], ContentUnitKind::Total),
        ];
        let projected = xlsx_unidentified_numeric_rows(&rows);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].kind, ContentUnitKind::TableRow);
        assert!(projected[0]
            .text
            .contains("应付工资 = 13；代扣代缴合计 = 5"));
        assert!(projected[0].text.contains("第 4 行，未标姓名"));
        assert!(!projected[0].text.contains("Alpha") && !projected[0].text.contains("元"));
        assert_eq!(
            (projected[0].ranges[0].start, projected[0].ranges[0].end),
            (4, 5)
        );
    }

    #[test]
    fn amount_bands_keep_their_company_type_and_two_original_rows() {
        let row = |number, cells: &[&str]| XlsxRow {
            text: cells.join(" | "),
            kind: ContentUnitKind::TableRow,
            ranges: vec![SourceRange::new(
                SourceRangeUnit::SheetRow,
                number,
                number + 1,
                Some("Approvals".into()),
            )],
            separator: None,
            cells: cells.iter().map(|cell| (*cell).into()).collect(),
        };
        let rows = vec![
            row(1, &["所属公司", "送审合同类型", "审批流"]),
            row(2, &["", "", "金额 [0, 50000) 元", "金额 [50000, +∞) 元"]),
            row(3, &["Alpha", "标准文本", "Approval A", "Approval B"]),
            row(4, &["", "", "金额 [0, 5000) 元", "金额 [5000, +∞) 元"]),
            row(5, &["Beta", "非标准文本", "Approval C", "Approval D"]),
        ];
        let projected = xlsx_band_rows(&rows);
        assert_eq!(projected.len(), 2);
        assert!(projected[0].text.contains("Alpha；送审合同类型 = 标准文本"));
        assert!(
            projected[0].text.contains("[0, 50000) 元")
                && !projected[0].text.contains("[0, 5000) 元")
        );
        assert_eq!(
            projected[0]
                .ranges
                .iter()
                .map(|range| (range.start, range.end))
                .collect::<Vec<_>>(),
            [(2, 3), (3, 4)]
        );
        let mut nonadjacent = rows;
        nonadjacent[2].ranges[0].start = 9;
        nonadjacent[2].ranges[0].end = 10;
        assert_eq!(xlsx_band_rows(&nonadjacent).len(), 1);
        let single_band = [
            row(1, &["所属公司", "送审合同类型", "审批流"]),
            row(2, &["", "", "金额 [0, +∞) 元"]),
            row(3, &["Alpha", "—（不分类型）", "Director → Seal（协同）"]),
        ];
        let projected = xlsx_band_rows(&single_band);
        assert_eq!(projected.len(), 1);
        assert!(projected[0]
            .text
            .contains("审批流 = Director → Seal（协同）"));
        assert!(projected[0].text.contains("金额 [0, +∞) 元"));
        assert!(projected[0].text.chars().count() <= 480);
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

    #[test]
    fn xlsx_uses_bounded_backend_and_respects_sheet_row_column_limits() {
        let path = write_test_xlsx(&minimal_xlsx_parts());
        let mut limits = BTreeMap::new();
        limits.insert("excel_max_sheets".to_string(), serde_json::json!(1));
        limits.insert("excel_max_rows".to_string(), serde_json::json!(2));
        limits.insert("excel_max_columns".to_string(), serde_json::json!(2));
        limits.insert(
            "document_excerpt_max_chars".to_string(),
            serde_json::json!(2000),
        );
        let request = OfficeParseRequest {
            file_path: path.clone(),
            file_type: ".xlsx".to_string(),
            limits,
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
        };

        let context = parse_office_file(&request);

        std::fs::remove_file(path).ok();
        assert_eq!(context.error, None);
        assert_eq!(context.parser_backend, RUST_XLSX_BOUNDED_BACKEND);
        assert!(context.content.contains("# XLSX preview"));
        assert!(context.content.contains("## Sheet: Data"));
        assert!(context.content.contains("| Name | Amount |"));
        assert!(context.content.contains("| Alpha | 10 |"));
        assert!(!context.content.contains("hidden-over-budget"));
        assert!(!context.content.contains("row-over-budget"));
        assert!(!context.content.contains("second-sheet-over-budget"));
        assert!(context.truncated);
    }

    #[test]
    fn xlsx_decodes_inline_string_numeric_character_references() {
        let mut parts = minimal_xlsx_parts();
        parts[3] = (
            "xl/worksheets/sheet1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><t>&#39033;&#30446;</t></is></c>
      <c r="B1" t="inlineStr"><is><t>&#29366;&#24577;</t></is></c>
    </row>
    <row r="2">
      <c r="A2" t="inlineStr"><is><t>Rust XLSX &#20013;&#25991;</t></is></c>
      <c r="B2" t="inlineStr"><is><t>&#23436;&#25104;</t></is></c>
    </row>
  </sheetData>
</worksheet>"#,
        );
        let path = write_test_xlsx(&parts);
        let mut limits = BTreeMap::new();
        limits.insert("excel_max_sheets".to_string(), serde_json::json!(1));
        limits.insert("excel_max_rows".to_string(), serde_json::json!(2));
        limits.insert("excel_max_columns".to_string(), serde_json::json!(2));
        limits.insert(
            "document_excerpt_max_chars".to_string(),
            serde_json::json!(2000),
        );
        let request = OfficeParseRequest {
            file_path: path.clone(),
            file_type: ".xlsx".to_string(),
            limits,
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
        };

        let context = parse_office_file(&request);

        std::fs::remove_file(path).ok();
        assert_eq!(context.error, None);
        assert_eq!(context.parser_backend, RUST_XLSX_BOUNDED_BACKEND);
        assert!(context.content.contains("| 项目 | 状态 |"));
        assert!(context.content.contains("| Rust XLSX 中文 | 完成 |"));
    }

    #[test]
    fn xlsx_bounded_backend_sets_truncated_when_char_budget_is_exhausted() {
        let path = write_test_xlsx(&minimal_xlsx_parts());
        let mut limits = BTreeMap::new();
        limits.insert("excel_max_sheets".to_string(), serde_json::json!(1));
        limits.insert("excel_max_rows".to_string(), serde_json::json!(2));
        limits.insert("excel_max_columns".to_string(), serde_json::json!(2));
        limits.insert(
            "document_excerpt_max_chars".to_string(),
            serde_json::json!(32),
        );
        let request = OfficeParseRequest {
            file_path: path.clone(),
            file_type: ".xlsx".to_string(),
            limits,
            parser_backend: RUST_OFFICE_BACKEND.to_string(),
        };

        let context = parse_office_file(&request);

        std::fs::remove_file(path).ok();
        assert_eq!(context.error, None);
        assert_eq!(context.parser_backend, RUST_XLSX_BOUNDED_BACKEND);
        assert!(context.content.chars().count() <= 32);
        assert!(context.truncated);
    }
}

#[derive(Debug, Clone)]
struct XlsxBudget {
    deadline: Instant,
    max_sheets: usize,
    max_rows: usize,
    max_columns: usize,
    max_chars: usize,
}

#[derive(Debug)]
enum XlsxParseError {
    CorruptZip(String),
    Other(String),
}

impl XlsxParseError {
    fn is_deterministic(&self) -> bool {
        matches!(self, Self::CorruptZip(_))
    }
}

impl std::fmt::Display for XlsxParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CorruptZip(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl From<String> for XlsxParseError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

#[derive(Debug, Clone)]
struct SheetRef {
    name: String,
    rel_id: String,
}

#[derive(Debug, Clone)]
enum PreviewCell {
    Text(String),
    SharedString(usize),
    Formula(String, Box<PreviewCell>),
}

#[derive(Debug)]
struct PreviewSheet {
    name: String,
    rows: Vec<Vec<String>>,
    columns: Vec<usize>,
    row_numbers: Vec<u64>,
    total_rows: BTreeSet<u64>,
    formula_rows: BTreeSet<u64>,
    priority: u8,
    truncated: bool,
}

struct CellBuilder {
    capture: bool,
    column_index: usize,
    cell_type: String,
    text: String,
    formula: String,
    in_value: bool,
    in_text: bool,
    in_formula: bool,
}

fn parse_bounded_xlsx(
    request: &OfficeParseRequest,
    file_type: &str,
    max_chars: usize,
) -> FileContextOut {
    match parse_bounded_xlsx_inner(request, max_chars) {
        Ok((content, truncated, audit)) => FileContextOut {
            file_path: request.file_path.to_string_lossy().to_string(),
            file_type: file_type.to_string(),
            content,
            evidence_audit: Some(audit),
            error: None,
            parser_backend: RUST_XLSX_BOUNDED_BACKEND.to_string(),
            truncated,
        },
        Err(error) => FileContextOut {
            evidence_audit: None,
            file_path: request.file_path.to_string_lossy().to_string(),
            file_type: file_type.to_string(),
            content: String::new(),
            error: Some(format!("RUST_XLSX_BOUNDED_PARSE_FAILED: {error}")),
            parser_backend: RUST_XLSX_BOUNDED_BACKEND.to_string(),
            truncated: false,
        },
    }
}

fn parse_bounded_xlsx_inner(
    request: &OfficeParseRequest,
    max_chars: usize,
) -> Result<(String, bool, ParsedContentAudit), XlsxParseError> {
    let parts = parts::PartBudget::new(request);
    let budget = XlsxBudget {
        deadline: parts.deadline,
        max_sheets: positive_limit(&request.limits, "excel_max_sheets", 2),
        max_rows: positive_limit(&request.limits, "excel_max_rows", 10),
        max_columns: positive_limit(&request.limits, "excel_max_columns", 12),
        max_chars: max_chars.max(1),
    };
    let file = File::open(&request.file_path)
        .map_err(|error| XlsxParseError::Other(format!("I/O error: {error}")))?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| XlsxParseError::CorruptZip(format!("ZIP error: {error}")))?;
    if !request.limits.contains_key("archive_output") {
        return spreadsheet::daily_xlsx(parts, &mut archive, &budget).map_err(Into::into);
    }
    let workbook_xml = read_zip_text(&mut archive, "xl/workbook.xml")?;
    let (sheets, truncated) = parse_workbook_sheets(&workbook_xml, budget.max_sheets)?;
    let rels_xml = read_zip_text(&mut archive, "xl/_rels/workbook.xml.rels")?;
    let relationships = parse_workbook_relationships(&rels_xml, false)?;
    archive_xlsx_rows(
        request,
        &mut archive,
        &sheets,
        &relationships,
        &budget,
        truncated,
    )
    .map_err(Into::into)
}

fn read_zip_text<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, String> {
    let name = resolve_zip_member(archive, name)?;
    let mut entry = archive
        .by_name(&name)
        .map_err(|error| format!("ZIP entry {name} error: {error}"))?;
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .map_err(|error| format!("ZIP entry {name} read error: {error}"))?;
    Ok(text)
}

fn resolve_zip_member<R: Read + Seek>(
    archive: &ZipArchive<R>,
    requested: &str,
) -> Result<String, String> {
    let mut matches = archive
        .file_names()
        .filter(|name| name.eq_ignore_ascii_case(requested));
    let first = matches
        .next()
        .ok_or_else(|| format!("ZIP entry {requested} missing"))?;
    if matches.next().is_some() {
        return Err(format!("ZIP entry {requested} has ambiguous case"));
    }
    Ok(first.to_string())
}

fn parse_workbook_sheets(xml: &str, max_sheets: usize) -> Result<(Vec<SheetRef>, bool), String> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(xml.as_bytes()));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut sheets = Vec::new();
    let mut truncated = false;

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|error| format!("workbook XML error: {error}"))?
        {
            Event::Start(e) | Event::Empty(e) if is_local_name(e.name().as_ref(), b"sheet") => {
                let name = attr_value(&e, b"name")?.unwrap_or_else(|| "Sheet".to_string());
                let rel_id = attr_value(&e, b"id")?.unwrap_or_default();
                if !rel_id.is_empty() {
                    if sheets.len() >= max_sheets {
                        truncated = true;
                        break;
                    }
                    sheets.push(SheetRef { name, rel_id });
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok((sheets, truncated))
}

fn parse_workbook_relationships(xml: &str, daily: bool) -> Result<HashMap<String, String>, String> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(xml.as_bytes()));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut relationships = HashMap::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|error| format!("relationships XML error: {error}"))?
        {
            Event::Start(e) | Event::Empty(e)
                if is_local_name(e.name().as_ref(), b"Relationship") =>
            {
                if let (Some(id), Some(target)) =
                    (attr_value(&e, b"Id")?, attr_value(&e, b"Target")?)
                {
                    let worksheet = !daily
                        || (!attr_value(&e, b"TargetMode")?
                            .is_some_and(|m| m.eq_ignore_ascii_case("External"))
                            && !target.contains([':', '\\'])
                            && !target.starts_with("//")
                            && attr_value(&e, b"Type")?
                                .is_none_or(|kind| kind.rsplit('/').next() == Some("worksheet")));
                    if worksheet {
                        relationships.insert(id, normalize_sheet_target(&target));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(relationships)
}

fn normalize_sheet_target(target: &str) -> String {
    let target = target.trim_start_matches('/');
    if target.starts_with("xl/") {
        target.to_string()
    } else {
        format!("xl/{target}")
    }
}

fn resolve_sheet_path(sheet: &SheetRef, relationships: &HashMap<String, String>) -> Option<String> {
    relationships.get(&sheet.rel_id).cloned()
}

// quick-xml may yield the last text token before noticing that its enclosing row/string
// is unfinished. Only an actual sampling boundary may discard that incomplete token.
fn worksheet_event<'a>(
    reader: &mut quick_xml::Reader<impl std::io::BufRead>,
    buf: &'a mut Vec<u8>,
    partial_at: Option<u64>,
    depth: &mut usize,
) -> Result<Event<'a>, String> {
    let result = reader.read_event_into(buf);
    // For an unterminated tag quick-xml reports its opening position even after
    // consuming the entire bounded input. Check the actual BufRead boundary too.
    let at_cut = partial_at.is_some_and(|n| {
        reader.buffer_position() >= n
            || std::io::BufRead::fill_buf(reader.get_mut()).is_ok_and(|rest| rest.is_empty())
    });
    match result {
        Err(
            quick_xml::Error::Syntax(_)
            | quick_xml::Error::IllFormed(quick_xml::errors::IllFormedError::UnclosedReference),
        ) if at_cut => Ok(Event::Eof),
        Err(e) => Err(e.to_string()),
        Ok(Event::Text(e))
            if at_cut
                && std::str::from_utf8(e.as_ref()).is_err_and(|err| err.error_len().is_none()) =>
        {
            Ok(Event::Eof)
        }
        Ok(Event::Eof) if partial_at.is_none() && *depth != 0 => {
            Err("unexpected end of worksheet XML".into())
        }
        Ok(event) => {
            match &event {
                Event::Start(_) => *depth += 1,
                Event::End(_) => *depth = depth.saturating_sub(1),
                _ => {}
            }
            Ok(event)
        }
    }
}

fn visit_sheet_rows(
    input: impl std::io::BufRead,
    row_limit: usize,
    col_limit: usize,
    archival: bool,
    deadline: Option<Instant>,
    partial_at: Option<u64>,
    mut sink: impl FnMut(u64, Vec<(usize, PreviewCell)>) -> Result<bool, String>,
) -> Result<bool, String> {
    let mut reader = quick_xml::Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut depth = 0;
    let mut read_count = 0;
    let mut current_row_number = 0;
    let mut row_anchor = None;
    let mut last_column = 0;
    let mut current_row = None;
    let mut current_cell: Option<CellBuilder> = None;
    let mut truncated = false;
    loop {
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Ok(true);
        }
        let event = worksheet_event(&mut reader, &mut buf, partial_at, &mut depth)
            .map_err(|e| format!("worksheet XML error: {e}"))?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(e) | Event::Empty(e)
                if is_local_name(e.name().as_ref(), b"row") && !(archival && empty) =>
            {
                if read_count >= row_limit {
                    truncated = true;
                    break;
                }
                row_anchor = attr_value(&e, b"r")?
                    .map(|v| {
                        v.parse::<u64>()
                            .ok()
                            .filter(|n| (1..=1_048_576).contains(n))
                            .ok_or("invalid worksheet row number")
                    })
                    .transpose()?;
                current_row_number = row_anchor.unwrap_or(current_row_number + 1);
                read_count += 1;
                last_column = 0;
                current_row = (!empty).then(Vec::new);
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"row") => {
                if let Some(row) = current_row.take() {
                    if !row.is_empty() && !sink(current_row_number, row)? {
                        return Ok(true);
                    }
                }
            }
            Event::Start(e) | Event::Empty(e)
                if is_local_name(e.name().as_ref(), b"c") && current_row.is_some() =>
            {
                let column_index = if let Some(address) = attr_value(&e, b"r")? {
                    let (column, row) =
                        cell_position(&address).ok_or("invalid worksheet cell reference")?;
                    if !archival {
                        if row_anchor.is_some_and(|n| n != row) {
                            return Err("worksheet row disagrees with cell reference".into());
                        }
                        current_row_number = row;
                        row_anchor = Some(row);
                    }
                    column
                } else {
                    last_column + 1
                };
                last_column = column_index;
                let capture = column_index <= col_limit;
                truncated |= !capture;
                if !empty {
                    current_cell = Some(CellBuilder {
                        capture,
                        column_index,
                        cell_type: attr_value(&e, b"t")?.unwrap_or_default(),
                        text: String::new(),
                        formula: String::new(),
                        in_value: false,
                        in_text: false,
                        in_formula: false,
                    });
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"c") => {
                if let (Some(mut cell), Some(row)) = (current_cell.take(), current_row.as_mut()) {
                    if cell.capture {
                        let formula = std::mem::take(&mut cell.formula);
                        let col = cell.column_index;
                        let value = finish_cell(cell);
                        if !formula.is_empty() {
                            row.push((
                                col,
                                PreviewCell::Formula(
                                    formula,
                                    Box::new(
                                        value.unwrap_or_else(|| PreviewCell::Text(String::new())),
                                    ),
                                ),
                            ));
                        } else if let Some(value) = value {
                            row.push((col, value));
                        }
                    }
                }
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"v") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_value = true;
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"v") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_value = false;
                }
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"t") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_text = true;
                }
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"f") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_formula = true;
                    if archival {
                        for key in [b"t".as_slice(), b"si", b"ref"] {
                            if let Some(value) = attr_value(&e, key)? {
                                cell.formula.push_str(&format!(
                                    "[{}={value}]",
                                    String::from_utf8_lossy(key)
                                ));
                            }
                        }
                    }
                }
            }
            Event::Empty(e) if is_local_name(e.name().as_ref(), b"f") && archival => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.formula.push_str("[formula]");
                    for key in [b"t".as_slice(), b"si", b"ref"] {
                        if let Some(value) = attr_value(&e, key)? {
                            cell.formula
                                .push_str(&format!("[{}={value}]", String::from_utf8_lossy(key)));
                        }
                    }
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"f") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_formula = false;
                }
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"t") => {
                if let Some(cell) = current_cell.as_mut() {
                    cell.in_text = false;
                }
            }
            Event::Text(e) => {
                if let Some(cell) = current_cell.as_mut() {
                    if cell.capture && (cell.in_value || cell.in_text) {
                        cell.text.push_str(&unescape_text(&e)?);
                    } else if cell.capture && cell.in_formula {
                        cell.formula.push_str(&unescape_text(&e)?);
                    }
                }
            }
            Event::GeneralRef(e) => {
                if let Some(cell) = current_cell.as_mut() {
                    if cell.capture && (cell.in_value || cell.in_text) {
                        cell.text.push_str(&unescape_general_ref(&e)?);
                    } else if cell.capture && cell.in_formula {
                        cell.formula.push_str(&unescape_general_ref(&e)?);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(truncated || partial_at.is_some())
}

fn vertical_sum_formula(formula: &str, row: u64) -> bool {
    let normalized = formula.replace([' ', '$'], "").to_ascii_uppercase();
    let start = normalized
        .find("SUM(")
        .map(|position| position + 4)
        .or_else(|| normalized.find("SUBTOTAL(").map(|position| position + 9));
    let Some(start) = start else {
        return false;
    };
    let Some(end) = normalized[start..].find(')') else {
        return false;
    };
    let range = normalized[start..start + end]
        .rsplit(',')
        .next()
        .unwrap_or("");
    let Some((from, to)) = range.split_once(':') else {
        return false;
    };
    fn reference(value: &str) -> Option<(&str, u64)> {
        let column_len = value.bytes().take_while(u8::is_ascii_alphabetic).count();
        let (column, number) = value.split_at(column_len);
        (!column.is_empty())
            .then(|| number.parse::<u64>().ok().map(|n| (column, n)))
            .flatten()
    }
    match (reference(from), reference(to)) {
        (Some((first_col, first)), Some((last_col, last))) => {
            first_col == last_col && first > 0 && first < last && last < row
        }
        _ => false,
    }
}

fn finish_cell(cell: CellBuilder) -> Option<PreviewCell> {
    let text = cell.text.trim();
    if text.is_empty() {
        return None;
    }
    if cell.cell_type == "s" {
        if let Ok(index) = text.parse() {
            return Some(PreviewCell::SharedString(index));
        }
    }
    Some(PreviewCell::Text(text.to_owned()))
}
fn read_string_table(
    input: impl std::io::BufRead,
    limit: usize,
    deadline: Option<Instant>,
    partial_at: Option<u64>,
) -> Result<(HashMap<usize, String>, bool), String> {
    let mut reader = quick_xml::Reader::from_reader(input);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut result = HashMap::new();
    let mut depth = 0;
    let mut current_index = 0usize;
    let mut in_si = false;
    let mut in_text = false;
    let mut current_text = String::new();
    loop {
        if deadline.is_some_and(|end| Instant::now() >= end) || current_index >= limit {
            return Ok((result, true));
        }
        match worksheet_event(&mut reader, &mut buf, partial_at, &mut depth)
            .map_err(|e| format!("sharedStrings XML error: {e}"))?
        {
            Event::Empty(e) if is_local_name(e.name().as_ref(), b"si") => {
                result.insert(current_index, String::new());
                current_index += 1;
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"si") => {
                in_si = true;
                current_text.clear();
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"si") => {
                result.insert(current_index, std::mem::take(&mut current_text));
                current_index += 1;
                in_si = false;
                current_text.clear();
            }
            Event::Start(e) if is_local_name(e.name().as_ref(), b"t") && in_si => {
                in_text = true;
            }
            Event::End(e) if is_local_name(e.name().as_ref(), b"t") => {
                in_text = false;
            }
            Event::Text(e) if in_si && in_text => {
                current_text.push_str(&unescape_text(&e)?);
            }
            Event::GeneralRef(e) if in_si && in_text => {
                current_text.push_str(&unescape_general_ref(&e)?);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok((result, partial_at.is_some()))
}

struct XlsxRow {
    text: String,
    kind: ContentUnitKind,
    ranges: Vec<SourceRange>,
    separator: Option<String>,
    cells: Vec<String>,
}

fn xlsx_unidentified_numeric_rows(rows: &[XlsxRow]) -> Vec<XlsxRow> {
    let Some((header_index, identity_column)) = rows.iter().enumerate().find_map(|(index, row)| {
        (row.kind == ContentUnitKind::TableHeader)
            .then(|| {
                row.cells
                    .iter()
                    .position(|cell| matches!(cell.trim(), "姓名" | "Name"))
            })
            .flatten()
            .map(|column| (index, column))
    }) else {
        return vec![];
    };
    let mut result = Vec::new();
    for (offset, row) in rows[header_index + 1..].iter().enumerate() {
        if row.kind != ContentUnitKind::TableRow
            || row
                .cells
                .get(identity_column)
                .is_some_and(|value| !value.is_empty())
            || row
                .cells
                .iter()
                .filter(|value| value.parse::<f64>().is_ok())
                .count()
                < 2
        {
            continue;
        }
        let number = row.ranges.first().map_or(0, |range| range.start);
        let prefix = format!("原表数值行（第 {number} 行，未标姓名）：");
        let mut text = prefix.clone();
        for (column, value) in row.cells.iter().enumerate() {
            if value.parse::<f64>().is_err() {
                continue;
            }
            let label = rows[..header_index + 1 + offset]
                .iter()
                .rev()
                .filter(|candidate| candidate.kind == ContentUnitKind::TableHeader)
                .find_map(|candidate| {
                    candidate
                        .cells
                        .get(column)
                        .filter(|label| !label.is_empty())
                })
                .cloned()
                .unwrap_or_else(|| format!("列{}", column + 1));
            let field = format!("{label} = {value}；");
            if text.chars().count() + field.chars().count() > 480 {
                if text != prefix {
                    result.push(XlsxRow {
                        text,
                        kind: ContentUnitKind::TableRow,
                        ranges: row.ranges.clone(),
                        separator: None,
                        cells: vec![],
                    });
                }
                text = prefix.clone();
            }
            if text.chars().count() + field.chars().count() <= 480 {
                text.push_str(&field);
            }
        }
        if text != prefix {
            result.push(XlsxRow {
                text,
                kind: ContentUnitKind::TableRow,
                ranges: row.ranges.clone(),
                separator: None,
                cells: vec![],
            });
        }
    }
    result
}

fn xlsx_band_rows(rows: &[XlsxRow]) -> Vec<XlsxRow> {
    if !rows.iter().any(|row| {
        row.cells.first().is_some_and(|cell| cell == "所属公司")
            && row.cells.get(1).is_some_and(|cell| cell == "送审合同类型")
    }) {
        return vec![];
    }
    let mut result = Vec::new();
    for pair in rows.windows(2) {
        let bands = &pair[0];
        let data = &pair[1];
        if !bands
            .ranges
            .first()
            .zip(data.ranges.first())
            .is_some_and(|(left, right)| left.end == right.start && left.label == right.label)
        {
            continue;
        }
        if data.cells.len() < 3 || data.cells[0].is_empty() || data.cells[1].is_empty() {
            continue;
        }
        let intervals: Vec<_> = bands
            .cells
            .iter()
            .skip(2)
            .filter(|value| value.trim_start().starts_with("金额 [") && value.contains(')'))
            .cloned()
            .collect();
        if intervals.is_empty() {
            continue;
        }
        let mut text = format!(
            "所属公司 = {}；送审合同类型 = {}；金额档 = {}；仅对应原表这两行。",
            data.cells[0],
            data.cells[1],
            intervals.join("、")
        );
        if text.chars().count() > 480 {
            continue;
        }
        if intervals.len() == 1 {
            let flow = bands
                .cells
                .iter()
                .enumerate()
                .skip(2)
                .find_map(|(column, value)| {
                    (value == &intervals[0])
                        .then(|| data.cells.get(column))
                        .flatten()
                        .filter(|value| !value.is_empty())
                });
            if let Some(flow) = flow {
                let detail = format!("审批流 = {flow}；");
                if text.chars().count() + detail.chars().count() <= 480 {
                    text.push_str(&detail);
                }
            }
        }
        result.push(XlsxRow {
            text,
            kind: ContentUnitKind::TableRow,
            ranges: bands.ranges.iter().chain(&data.ranges).cloned().collect(),
            separator: None,
            cells: vec![],
        });
    }
    result
}

fn xlsx_window_summaries(rows: &[XlsxRow]) -> Vec<XlsxRow> {
    let header_index = rows
        .iter()
        .rposition(|row| row.kind == ContentUnitKind::TableHeader);
    let Some(header_index) = header_index else {
        return vec![];
    };
    let last_header = &rows[header_index];
    let columns = rows[..=header_index]
        .iter()
        .filter(|row| row.kind == ContentUnitKind::TableHeader)
        .map(|row| row.cells.len())
        .max()
        .unwrap_or(0);
    let header = XlsxRow {
        text: last_header.text.clone(),
        kind: ContentUnitKind::TableHeader,
        ranges: last_header.ranges.clone(),
        separator: None,
        cells: (0..columns)
            .map(|column| {
                rows[..=header_index]
                    .iter()
                    .rev()
                    .filter(|row| row.kind == ContentUnitKind::TableHeader)
                    .find_map(|row| row.cells.get(column).filter(|cell| !cell.is_empty()))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect(),
    };
    let header = &header;
    let data: Vec<_> = rows[header_index + 1..]
        .iter()
        .filter(|row| {
            row.kind == ContentUnitKind::TableRow
                && row.cells.iter().filter(|cell| !cell.is_empty()).count() >= 2
        })
        .collect();
    if data.len() < 2 {
        return vec![];
    }
    let mut summaries = Vec::new();
    // A repeated identity defines a bounded group. Its common amounts and dates
    // remain attributable to that identity instead of being mixed across people.
    if let Some(column) = header
        .cells
        .iter()
        .position(|label| matches!(label.trim(), "姓名" | "Name"))
    {
        let mut groups: Vec<(&str, Vec<&XlsxRow>)> = Vec::new();
        for row in &data {
            let Some(identity) = row.cells.get(column).filter(|value| !value.is_empty()) else {
                continue;
            };
            if let Some((_, members)) = groups.iter_mut().find(|(key, _)| *key == identity) {
                members.push(row);
            } else {
                groups.push((identity, vec![row]));
            }
        }
        if groups.len() > 1 {
            for (_, members) in groups {
                if members.len() >= 2 {
                    summaries.extend(xlsx_amount_series(header, &members, column));
                    summaries.push(xlsx_rows_summary(header, &members));
                }
            }
        } else {
            summaries.extend(xlsx_amount_series(header, &data, column));
        }
    }
    summaries.push(xlsx_rows_summary(header, &data));
    summaries
}

fn xlsx_amount_series(header: &XlsxRow, data: &[&XlsxRow], identity_column: usize) -> Vec<XlsxRow> {
    let period_column = header
        .cells
        .iter()
        .position(|label| matches!(label.trim(), "月份" | "交易日期" | "Date" | "Month"));
    let Some(period_column) = period_column else {
        return vec![];
    };
    let identity = &data[0].cells[identity_column];
    if identity.is_empty()
        || data
            .iter()
            .any(|row| row.cells.get(identity_column) != Some(identity))
    {
        return vec![];
    }
    let mut result = Vec::new();
    for label in [
        "应付工资",
        "实发工资",
        "代扣代缴合计",
        "基本工资",
        "交易金额",
        "Amount",
        "Total",
    ] {
        let Some(column) = header.cells.iter().position(|value| value.trim() == label) else {
            continue;
        };
        let mut values: Vec<(&str, Vec<&XlsxRow>)> = Vec::new();
        for row in data {
            let Some(value) = row.cells.get(column).filter(|value| !value.is_empty()) else {
                continue;
            };
            if let Some((_, members)) = values.iter_mut().find(|(prior, _)| *prior == value) {
                members.push(row);
            } else {
                values.push((value, vec![row]));
            }
        }
        if values.len() < 2 {
            continue;
        }
        let prefix = format!(
            "读取窗口统计：{} = {identity}；",
            header.cells[identity_column]
        );
        let suffix = "统计仅指本次读取范围。";
        let mut text = prefix.clone();
        let mut used: Vec<&XlsxRow> = Vec::new();
        for (value, members) in values {
            let periods = members
                .iter()
                .filter_map(|row| row.cells.get(period_column))
                .cloned()
                .collect::<Vec<_>>()
                .join("、");
            let field = format!(
                "{label} = {value}（对应{} = {periods}）；",
                header.cells[period_column]
            );
            if text.chars().count() + field.chars().count() + suffix.chars().count() > 480
                && !used.is_empty()
            {
                used.sort_by_key(|row| row.ranges.first().map(|range| range.start));
                let mut summary = xlsx_rows_summary(header, &used);
                summary.text = text + suffix;
                result.push(summary);
                text = prefix.clone();
                used.clear();
            }
            if text.chars().count() + field.chars().count() + suffix.chars().count() <= 480 {
                text.push_str(&field);
                used.extend(members);
            }
        }
        if !used.is_empty() {
            used.sort_by_key(|row| row.ranges.first().map(|range| range.start));
            let mut summary = xlsx_rows_summary(header, &used);
            summary.text = text + suffix;
            result.push(summary);
        }
    }
    result
}

fn xlsx_rows_summary(header: &XlsxRow, data: &[&XlsxRow]) -> XlsxRow {
    let mut text = format!("读取窗口统计：本表头对应的已读数据行 {} 条。", data.len());
    let mut common = Vec::new();
    let mut differing = Vec::new();
    for (column, value) in data[0].cells.iter().enumerate() {
        if data.iter().all(|row| row.cells.get(column) == Some(value)) {
            if value.is_empty() {
                continue;
            }
            let label = header
                .cells
                .get(column)
                .filter(|label| !label.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("列{}", column + 1));
            common.push(format!("{label} = {value}；"));
        } else {
            differing.push(column);
        }
    }
    if differing.len() == 1 {
        let column = differing[0];
        let values = data
            .iter()
            .filter_map(|row| row.cells.get(column))
            .cloned()
            .collect::<Vec<_>>()
            .join("、");
        let label = header
            .cells
            .get(column)
            .filter(|label| !label.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("列{}", column + 1));
        let field = format!("逐行 {label} = {values}；");
        if text.chars().count() + field.chars().count() <= 480 {
            text.push_str(&field);
        }
    }
    if !common.is_empty() {
        text.push_str("这些行的共同值：");
        for field in common {
            if text.chars().count() + field.chars().count() <= 480 {
                text.push_str(&field);
            }
        }
    }
    text.push_str("统计仅指本次读取范围。");
    let mut ranges: Vec<SourceRange> = Vec::new();
    for row in data {
        for range in &row.ranges {
            if let Some(prior) = ranges.last_mut().filter(|prior| {
                prior.end == range.start
                    && prior.column_start == range.column_start
                    && prior.column_end == range.column_end
                    && prior.label == range.label
            }) {
                prior.end = range.end;
            } else {
                ranges.push(range.clone());
            }
        }
    }
    XlsxRow {
        text,
        // A derived window summary describes the cited rows; only a total
        // explicitly present in the workbook receives the Total priority.
        kind: ContentUnitKind::TableRow,
        ranges,
        separator: None,
        cells: vec![],
    }
}

impl XlsxRow {
    fn chars(&self) -> usize {
        self.text.chars().count() + 1 + self.separator.as_ref().map_or(0, |s| s.chars().count() + 1)
    }

    fn useful(&self) -> bool {
        matches!(
            self.kind,
            ContentUnitKind::TableRow | ContentUnitKind::Total
        )
    }
}

struct SheetSelection {
    costs: Vec<usize>,
    // (row index, cumulative cost) for headers and unit notes, in source order.
    structure: Vec<(usize, usize)>,
    covered: usize,
    heading_cost: usize,
    selected: BTreeSet<usize>,
}

impl SheetSelection {
    fn new(heading: &str, rows: &[XlsxRow]) -> Self {
        let costs: Vec<_> = rows.iter().map(XlsxRow::chars).collect();
        let mut structure = Vec::new();
        let mut sum = 0;
        for (i, row) in rows.iter().enumerate() {
            if matches!(
                row.kind,
                ContentUnitKind::TableHeader | ContentUnitKind::Unit | ContentUnitKind::Heading
            ) {
                sum += costs[i];
                structure.push((i, sum));
            }
        }
        Self {
            costs,
            structure,
            covered: 0,
            heading_cost: heading.chars().count() + 1,
            selected: BTreeSet::new(),
        }
    }

    fn admit(&mut self, row: usize, remaining: &mut usize) {
        let n = self.structure.partition_point(|(index, _)| *index < row);
        let prefix_cost = |n: usize| n.checked_sub(1).map_or(0, |i| self.structure[i].1);
        let cost = self.costs[row]
            + prefix_cost(n).saturating_sub(prefix_cost(self.covered))
            + if self.selected.is_empty() {
                self.heading_cost
            } else {
                0
            };
        if cost <= *remaining {
            if n > self.covered {
                self.selected.extend(
                    self.structure[self.covered..n]
                        .iter()
                        .map(|(index, _)| *index),
                );
                self.covered = n;
            }
            self.selected.insert(row);
            *remaining -= cost;
        }
    }
}

fn render_workbook(
    sheets: &[PreviewSheet],
    budget: &XlsxBudget,
    format: &str,
    directory: &str,
) -> (String, bool, ParsedContentAudit) {
    let mut builder = AuditTextBuilder::new(budget.max_chars as u64, format);
    let mut title = format!("# {} preview", format.to_ascii_uppercase());
    if !directory.is_empty() {
        title.push('\n');
        title.push_str(directory);
    }
    builder.push(&title, ContentUnitKind::Heading, vec![]);
    let wall_remaining = budget.deadline.saturating_duration_since(Instant::now());
    let select_until =
        budget.deadline - (wall_remaining / 4).min(std::time::Duration::from_millis(200));
    let prepare_until =
        select_until - (wall_remaining / 8).min(std::time::Duration::from_millis(100));
    let mut prepared = Vec::new();
    let mut selection_truncated = false;
    for sheet in sheets {
        if Instant::now() >= prepare_until {
            selection_truncated = true;
            break;
        }
        if sheet.rows.is_empty() {
            continue;
        }
        let heading = format!("## Sheet: {}", escape_markdown_cell(&sheet.name));
        let col_count = sheet.columns.len();
        let positioned = sheet.columns.last().is_some_and(|col| *col > 12)
            || !sheet.columns.iter().copied().eq(1..=col_count);
        let header_index = sheet.rows.iter().position(|row| is_header_row(row));
        let labels = &sheet.rows[header_index.unwrap_or(0)];
        let identity_columns: Vec<_> = labels
            .iter()
            .enumerate()
            .filter_map(|(index, label)| {
                matches!(label.trim(), "姓名" | "月份" | "Name" | "Date").then_some(index)
            })
            .collect();
        let mut rendered: Vec<XlsxRow> = Vec::new();
        let mut header_at: Option<usize> = None;
        let mut unit_at: Option<usize> = None;
        let mut data_since_unit = false;
        let mut unit_columns = Vec::new();
        for (index, row) in sheet.rows.iter().enumerate() {
            if Instant::now() >= prepare_until {
                selection_truncated = true;
                break;
            }
            let cells: Vec<_> = row.iter().map(|c| escape_markdown_cell(c)).collect();
            let number = sheet.row_numbers[index];
            let ranges = spreadsheet::cell_ranges(&sheet.name, number, &sheet.columns, &cells);
            for range in &ranges {
                builder.read(range.clone());
            }
            let metadata = header_index.is_some_and(|header| index < header);
            let line = if positioned {
                cells
                    .iter()
                    .zip(&sheet.columns)
                    .filter(|(value, _)| !value.is_empty())
                    .map(|(value, col)| {
                        format!("{}{number}={value}", spreadsheet::column_name(*col))
                    })
                    .collect::<Vec<_>>()
                    .join(" | ")
            } else if metadata {
                cells
                    .iter()
                    .filter(|cell| !cell.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" | ")
            } else {
                format!("| {} |", cells.join(" | "))
            };
            let unit_line = cells.iter().any(|c| spreadsheet::unit(c));
            let formula_padding = sheet.formula_rows.contains(&sheet.row_numbers[index])
                && !identity_columns.is_empty()
                && identity_columns.iter().all(|column| {
                    cells
                        .get(*column)
                        .is_none_or(|value| value.is_empty() || value == "0")
                })
                && cells
                    .iter()
                    .filter(|value| !value.is_empty())
                    .all(|value| value.parse::<f64>().is_ok_and(|number| number == 0.0));
            let kind = if formula_padding {
                ContentUnitKind::Placeholder
            } else if unit_line {
                ContentUnitKind::Unit
            } else if is_header_row(&cells)
                || header_index.is_some_and(|header| index < header)
                || (sheet.rows.len() > 1
                    && index
                        == usize::from(
                            sheet
                                .rows
                                .first()
                                .is_some_and(|r| r.iter().any(|c| spreadsheet::unit(c))),
                        ))
            {
                ContentUnitKind::TableHeader
            } else if cells.iter().any(|c| spreadsheet::total(c))
                || sheet.total_rows.contains(&sheet.row_numbers[index])
            {
                ContentUnitKind::Total
            } else {
                ContentUnitKind::TableRow
            };
            if positioned {
                match kind {
                    ContentUnitKind::TableHeader => {
                        if data_since_unit {
                            unit_at = None;
                        }
                        header_at = Some(rendered.len());
                    }
                    ContentUnitKind::Unit => {
                        unit_at = Some(rendered.len());
                        data_since_unit = false;
                    }
                    ContentUnitKind::TableRow | ContentUnitKind::Total => data_since_unit = true,
                    _ => {}
                }
            }
            if positioned
                && (kind == ContentUnitKind::Total
                    || (kind == ContentUnitKind::TableRow
                        && cells.iter().any(|s| spreadsheet::exception(s))))
            {
                let values = spreadsheet::positioned_values(
                    sheet,
                    number,
                    &cells,
                    kind,
                    header_at.map(|i| &rendered[i]),
                    unit_at.map(|i| &rendered[i]),
                    &unit_columns,
                );
                if !values.is_empty() {
                    rendered.extend(values);
                    continue;
                }
            }
            rendered.push(XlsxRow {
                text: line,
                kind,
                ranges,
                cells,
                separator: (kind == ContentUnitKind::TableHeader && !metadata && !positioned)
                    .then(|| format!("| {} |", vec!["---"; col_count].join(" | "))),
            });
            if positioned && matches!(kind, ContentUnitKind::TableHeader | ContentUnitKind::Unit) {
                unit_columns.clear();
                if let (Some(h), Some(u)) = (header_at, unit_at) {
                    unit_columns.extend(
                        rendered[h]
                            .cells
                            .iter()
                            .enumerate()
                            .map(|(i, name)| spreadsheet::applicable_unit(&rendered[u], i, name)),
                    );
                }
            }
        }
        if positioned {
            prepared.push((heading, rendered));
            continue;
        }
        let mut summaries = xlsx_window_summaries(&rendered);
        let whole = summaries.pop();
        let mut projected = xlsx_unidentified_numeric_rows(&rendered);
        projected.extend(xlsx_band_rows(&rendered));
        projected.append(&mut summaries);
        let insert_at = rendered
            .iter()
            .rposition(|row| row.kind == ContentUnitKind::TableHeader)
            .map_or(0, |index| index + 1);
        rendered.splice(insert_at..insert_at, projected);
        if let Some(whole) = whole {
            rendered.push(whole);
        }
        rendered.retain(|row| row.ranges.len() <= 128);
        prepared.push((heading, rendered));
    }
    // All rows are already read within the physical limits. Share the rendering
    // budget across sheets before giving any sheet its remaining bulk rows.
    let mut remaining = budget.max_chars.saturating_sub(title.chars().count());
    let mut choices: Vec<_> = prepared
        .iter()
        .map(|(h, rows)| SheetSelection::new(h, rows))
        .collect();
    let raw_cost: usize = prepared
        .iter()
        .zip(&choices)
        .map(|((_, rows), choice)| {
            choice.heading_cost
                + rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| !r.cells.is_empty() && r.kind != ContentUnitKind::Placeholder)
                    .map(|(i, _)| choice.costs[i])
                    .sum::<usize>()
        })
        .sum();
    let all_raw_fit = raw_cost <= remaining;
    for ((_, rows), choice) in prepared.iter().zip(&mut choices) {
        if all_raw_fit {
            choice.selected.extend((0..rows.len()).filter(|&i| {
                !rows[i].cells.is_empty() && rows[i].kind != ContentUnitKind::Placeholder
            }));
            choice.covered = choice.structure.len();
        } else if let Some(first) = rows.iter().position(|r| r.useful() && !r.cells.is_empty()) {
            choice.admit(first, &mut remaining);
        }
    }
    if all_raw_fit {
        remaining -= raw_cost;
    }
    let mut orders = Vec::new();
    let mut cheapest = usize::MAX;
    for ((_, rows), choice) in prepared.iter().zip(&choices) {
        let mut order = Vec::new();
        if remaining > 0 && Instant::now() < select_until {
            for (i, row) in rows.iter().enumerate() {
                if row.useful() && !choice.selected.contains(&i) && choice.costs[i] <= remaining {
                    cheapest = cheapest.min(choice.costs[i]);
                    order.push(i);
                }
            }
            order.sort_by_cached_key(|&i| {
                (
                    rows[i].kind != ContentUnitKind::Total,
                    !rows[i].cells.iter().any(|s| spreadsheet::exception(s)),
                )
            });
        }
        orders.push(order.into_iter());
    }
    while remaining >= cheapest && Instant::now() < select_until {
        let mut progress = false;
        for (choice, order) in choices.iter_mut().zip(&mut orders) {
            if let Some(row) = order.next() {
                choice.admit(row, &mut remaining);
                progress = true;
                if remaining < cheapest {
                    break;
                }
            }
        }
        if !progress {
            break;
        }
    }
    for ((heading, rows), choice) in prepared.iter().zip(choices) {
        selection_truncated |= rows
            .iter()
            .enumerate()
            .any(|(i, row)| !row.cells.is_empty() && !choice.selected.contains(&i));
        if choice.selected.is_empty() {
            continue;
        }
        builder.push(heading, ContentUnitKind::Heading, vec![]);
        for row in &choice.selected {
            if Instant::now() >= budget.deadline {
                selection_truncated = true;
                break;
            }
            let row = &rows[*row];
            builder.push(&row.text, row.kind, row.ranges.clone());
            if let Some(separator) = &row.separator {
                builder.push(separator, ContentUnitKind::TableHeader, row.ranges.clone());
            }
        }
    }
    let (content, audit, truncated) = builder.finish();
    (content, truncated || selection_truncated, audit)
}

fn is_header_row(cells: &[String]) -> bool {
    is_header_row_values(cells.iter().map(String::as_str))
}

fn is_header_row_values<'a>(cells: impl Iterator<Item = &'a str>) -> bool {
    const LABELS: &[&str] = &[
        "姓名",
        "月份",
        "部门",
        "基本工资",
        "应付工资",
        "实发工资",
        "出勤系数",
        "代扣代缴合计",
        "序号",
        "交易日期",
        "交易币种",
        "交易金额",
        "收款方名称",
        "总计",
        "所属公司",
        "送审合同类型",
        "审批流",
        "项目",
        "项目名称",
        "名称",
        "数量",
        "金额",
        "单位",
        "余额",
        "差异",
        "差异金额",
        "异常说明",
        "状态",
        "结果",
        "负责人",
        "预算额",
        "实付额",
        "实收额",
        "实际金额",
        "Name",
        "Amount",
        "Date",
        "Total",
    ];
    let mut labels = 0;
    for cell in cells {
        if cell.parse::<f64>().is_ok() {
            return false;
        }
        let label = cell.split(['(', '（']).next().unwrap_or(cell).trim();
        labels += usize::from(LABELS.contains(&label));
    }
    labels >= 2
}

fn cell_to_text(cell: &PreviewCell, shared_strings: &HashMap<usize, String>) -> String {
    match cell {
        PreviewCell::Text(value) => value.clone(),
        PreviewCell::Formula(formula, cached) => format!(
            "公式：={formula}；缓存值：{}",
            cell_to_text(cached, shared_strings)
        ),
        PreviewCell::SharedString(index) => shared_strings
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("#SHARED_STRING[{index}]")),
    }
}

fn escape_markdown_cell(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .replace('|', "\\|")
        .trim()
        .to_string()
}

fn cell_position(address: &str) -> Option<(usize, u64)> {
    let width = address.bytes().take_while(u8::is_ascii_alphabetic).count();
    if !(1..=3).contains(&width) {
        return None;
    }
    let (letters, digits) = address.split_at(width);
    let mut column = 0;
    for c in letters.bytes() {
        column = column * 26 + usize::from(c.to_ascii_uppercase() - b'A' + 1);
    }
    let row = digits.parse::<u64>().ok()?;
    (column <= 16_384 && (1..=1_048_576).contains(&row)).then_some((column, row))
}

fn attr_value(e: &BytesStart<'_>, local_name: &[u8]) -> Result<Option<String>, String> {
    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|error| format!("XML attribute error: {error}"))?;
        if is_local_name(attr.key.as_ref(), local_name) {
            return attr
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, e.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|error| format!("XML attribute decode error: {error}"));
        }
    }
    Ok(None)
}

fn is_local_name(name: &[u8], expected: &[u8]) -> bool {
    let local = name
        .iter()
        .rposition(|byte| *byte == b':')
        .map(|index| &name[index + 1..])
        .unwrap_or(name);
    local == expected
}

fn unescape_text(e: &BytesText<'_>) -> Result<String, String> {
    let decoded = e
        .decode()
        .map_err(|error| format!("XML text decode error: {error}"))?;
    quick_xml::escape::unescape(&decoded)
        .map(|value| value.into_owned())
        .map_err(|error| format!("XML text unescape error: {error}"))
}

fn unescape_general_ref(e: &BytesRef<'_>) -> Result<String, String> {
    let decoded = e
        .decode()
        .map_err(|error| format!("XML reference decode error: {error}"))?;
    let wrapped = format!("&{decoded};");
    quick_xml::escape::unescape(&wrapped)
        .map(|value| value.into_owned())
        .map_err(|error| format!("XML reference unescape error: {error}"))
}

fn archive_xlsx_rows<R: Read + Seek>(
    request: &OfficeParseRequest,
    archive: &mut ZipArchive<R>,
    sheets: &[SheetRef],
    relationships: &HashMap<String, String>,
    budget: &XlsxBudget,
    mut truncated: bool,
) -> Result<(String, bool, ParsedContentAudit), String> {
    let strings = if let Ok(name) = resolve_zip_member(archive, "xl/sharedStrings.xml") {
        let entry = archive.by_name(&name).map_err(|e| e.to_string())?;
        read_string_table(BufReader::new(entry), usize::MAX, None, None)?.0
    } else {
        HashMap::new()
    };
    let mut builder = content_builder(request, budget.max_chars, "xlsx");
    builder.push("# XLSX archive", ContentUnitKind::Heading, vec![]);
    for sheet in sheets {
        let Some(path) = resolve_sheet_path(sheet, relationships) else {
            truncated = true;
            continue;
        };
        if !builder.push(
            &format!("## Sheet: {}", escape_markdown_cell(&sheet.name)),
            ContentUnitKind::Heading,
            vec![],
        ) {
            truncated = true;
            break;
        }
        let mut first = true;
        let member = resolve_zip_member(archive, &path)?;
        let entry = archive.by_name(&member).map_err(|e| e.to_string())?;
        let limited = visit_sheet_rows(
            BufReader::new(entry),
            budget.max_rows,
            budget.max_columns,
            true,
            None,
            None,
            |number, row| {
                let count = row.iter().map(|(col, _)| *col).max().unwrap_or(0);
                let mut cells = vec![String::new(); count];
                for (col, cell) in row {
                    cells[col - 1] = escape_markdown_cell(&cell_to_text(&cell, &strings));
                }
                if cells.iter().any(|value| value.contains("#SHARED_STRING[")) {
                    return Err("referenced shared string is missing".into());
                }
                let kind = if first || is_header_row(&cells) {
                    ContentUnitKind::TableHeader
                } else {
                    ContentUnitKind::TableRow
                };
                first = false;
                let mut range = SourceRange::new(
                    SourceRangeUnit::SheetRow,
                    number,
                    number + 1,
                    Some(sheet.name.clone()),
                );
                range.column_start = Some(1);
                range.column_end = Some(cells.len() as u64 + 1);
                Ok(builder.push(&format!("| {} |", cells.join(" | ")), kind, vec![range]))
            },
        )?;
        truncated |= limited;
        if limited {
            break;
        }
    }
    let (content, audit, bounded) = builder.finish();
    Ok((content, truncated || bounded, audit))
}
