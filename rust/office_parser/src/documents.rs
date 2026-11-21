use crate::{captions, charts, escape_markdown_cell, parts, OfficeContent, OfficeInput};
use ai_daily_worker_contract::{ContentUnitKind, ParserLimits, SourceRange, SourceRangeUnit};
use std::collections::BTreeMap;
use std::fs::File;
use std::time::Instant;

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

pub(super) fn parse(
    request: &OfficeInput<'_>,
    mut parts: parts::PartBudget,
) -> Result<OfficeContent, String> {
    use office_oxide::docx::BlockElement;
    let format =
        office_oxide::DocumentFormat::from_extension(request.format.trim_start_matches('.'))
            .expect("supported extension");
    let file = File::open(request.path).map_err(|error| error.to_string())?;
    let document = open_office(file, format)?;
    let ParserLimits::Office {
        docx_max_paragraphs,
        docx_max_tables,
        docx_table_max_rows,
        docx_table_max_cols,
        pptx_max_slides,
        pptx_include_notes: include_notes,
        ..
    } = *request.limits
    else {
        unreachable!("validated Office limits")
    };
    let deadline = parts.deadline;
    let archival = request.archival;
    let mut builder = request.builder();
    let mut bounded = !archival && Instant::now() >= deadline;
    match document {
        OpenOffice::Word(mut docx) => {
            // office_oxide opens the document once. Record all logical positions it parsed;
            // output units obey the existing paragraph/table limits independently.

            let captions = if !archival
                && docx
                    .body
                    .elements
                    .iter()
                    .any(|e| matches!(e, BlockElement::Table(_)))
            {
                let (captions, incomplete) = captions::read(request.path, &docx, &mut parts)
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
                        if paragraph_number > docx_max_paragraphs as usize {
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
                        if table_number <= docx_max_tables as usize {
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
                            if table_number > docx_max_tables as usize
                                || index >= docx_table_max_rows as usize
                            {
                                bounded = true;
                                continue;
                            }
                            if row.cells.len() > docx_table_max_cols as usize {
                                bounded = true;
                            }
                            range.column_end =
                                Some(row.cells.len().min(docx_table_max_cols as usize) as u64 + 1);
                            let cells: Vec<String> = row
                                .cells
                                .into_iter()
                                .take(docx_table_max_cols as usize)
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
            let daily = !archival;
            let mut charts = (daily
                && pptx
                    .slides
                    .iter()
                    .take(pptx_max_slides as usize)
                    .any(|s| charts::may_have_chart(&s.shapes)))
            .then(|| charts::SlideCharts::open(request.path, parts))
            .flatten();
            if !include_notes {
                for slide in pptx.slides.iter_mut().take(pptx_max_slides as usize) {
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
                if index >= pptx_max_slides as usize {
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
                        if index < docx_max_paragraphs as usize {
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
                for (index, slide) in ppt.slides.iter().enumerate() {
                    let range = SourceRange::new(
                        SourceRangeUnit::Slide,
                        index as u64 + 1,
                        index as u64 + 2,
                        None,
                    );
                    builder.read(range.clone());
                    if index >= pptx_max_slides as usize {
                        bounded = true;
                        continue;
                    }
                    for run in &slide.text_runs {
                        if (include_notes || run.text_type != office_oxide::ppt::TextType::Notes)
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
    Ok((content, audit, bounded || truncated))
}
