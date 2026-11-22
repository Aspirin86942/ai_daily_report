use crate::contract::{ContentUnitKind, SourceRange, SourceRangeUnit};
use crate::office::{
    captions, charts, escape_markdown_cell, ooxml, parts, OfficeContent, OfficeInput,
};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek};
use std::time::Instant;
use zip::ZipArchive;

pub(super) fn parse<R: Read + Seek>(
    request: &OfficeInput<'_>,
    mut parts: parts::PartBudget,
    format: &str,
    zip: &mut ZipArchive<R>,
) -> Result<OfficeContent, String> {
    use office_oxide::docx::BlockElement;
    let limits = request.limits;
    let include_notes = limits.pptx_include_notes;
    let word = format == ".docx";
    let package = ooxml::bounded_document(zip, &mut parts, word, include_notes)?;
    // The stored XML makes the native reader charge its decoded bytes on
    // repeated visits to a part.
    let reader = parts::CheckedReader::new(package, request.deadline);
    let deadline = parts.deadline;
    let mut builder = request.builder(format);
    let mut bounded;
    if word {
        let mut docx = office_oxide::docx::DocxDocument::from_reader(reader)
            .map_err(|error| error.to_string())?;
        bounded = Instant::now() >= deadline;
        // office_oxide opens the document once. Record all logical positions it parsed;
        // output units obey the existing paragraph/table limits independently.

        let captions = if docx
            .body
            .elements
            .iter()
            .any(|e| matches!(e, BlockElement::Table(_)))
        {
            let (captions, incomplete) =
                captions::read(zip, &docx, &mut parts).unwrap_or_else(|| (BTreeMap::new(), true));
            bounded |= incomplete;
            captions
        } else {
            BTreeMap::new()
        };
        let mut paragraph_number = 0;
        let mut table_number = 0;
        for element in std::mem::take(&mut docx.body.elements) {
            if Instant::now() >= deadline {
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
                        None,
                    );
                    builder.read(range.clone());
                    if paragraph_number > limits.docx_max_paragraphs as usize {
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
                        bounded |= !builder.push(part, kind, vec![range.clone()]);
                    }
                }
                BlockElement::Table(table) => {
                    table_number += 1;
                    let label = format!("Table {table_number}");
                    let caption = captions.get(&table_number);
                    let caption_range = caption
                        .and_then(|c| c.paragraph)
                        .map(|n| SourceRange::new(SourceRangeUnit::Paragraph, n, n + 1, None));
                    if table_number <= limits.docx_max_tables as usize {
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
                        if Instant::now() >= deadline {
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
                        if table_number > limits.docx_max_tables as usize
                            || index >= limits.docx_table_max_rows as usize
                        {
                            bounded = true;
                            continue;
                        }
                        bounded |= row.cells.len() > limits.docx_table_max_cols as usize;
                        range.column_end = Some(
                            row.cells.len().min(limits.docx_table_max_cols as usize) as u64 + 1,
                        );
                        let cells: Vec<String> = row
                            .cells
                            .into_iter()
                            .take(limits.docx_table_max_cols as usize)
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
                        bounded |= !builder.push(&line, kind, ranges);
                    }
                }
            }
        }
    } else {
        let mut pptx = office_oxide::pptx::PptxDocument::from_reader(reader)
            .map_err(|error| error.to_string())?;
        bounded = Instant::now() >= deadline;
        let mut charts = pptx
            .slides
            .iter()
            .take(limits.pptx_max_slides as usize)
            .any(|s| charts::may_have_chart(&s.shapes))
            .then(|| charts::SlideCharts::open(zip, parts))
            .flatten();
        if !include_notes {
            for slide in pptx.slides.iter_mut().take(limits.pptx_max_slides as usize) {
                slide.notes = None;
            }
        }
        for index in 0..pptx.slides.len() {
            if Instant::now() >= deadline {
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
            if index >= limits.pptx_max_slides as usize {
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
                bounded |= !builder.push(part, kind, vec![range.clone()]);
            }
            if charts::may_have_chart(&pptx.slides[index].shapes) {
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
    let (content, audit, truncated) = builder.finish();
    Ok((content, audit, bounded || truncated))
}

pub(super) fn parse_legacy(
    request: &OfficeInput<'_>,
    bytes: Vec<u8>,
    format: &str,
) -> Result<OfficeContent, String> {
    let reader = parts::CheckedReader::new(Cursor::new(bytes), request.deadline);
    let limits = request.limits;
    let include_notes = limits.pptx_include_notes;
    let mut builder = request.builder(format);
    let mut bounded = false;
    if format == ".doc" {
        let doc = office_oxide::doc::DocDocument::from_reader(reader)
            .map_err(|error| error.to_string())?;
        let text = doc.plain_text_ref();
        let mut offset = 0u64;
        for (index, part) in text.split_inclusive('\n').enumerate() {
            super::check_time(request.deadline).map_err(|error| error.to_string())?;
            let end = offset + part.chars().count() as u64;
            if end > offset {
                let range = SourceRange::new(
                    SourceRangeUnit::TextChar,
                    offset,
                    end,
                    Some("extracted_main_text".into()),
                );
                builder.read(range.clone());
                bounded |= index >= limits.docx_max_paragraphs as usize
                    || !builder.push(part, ContentUnitKind::Paragraph, vec![range]);
            }
            offset = end;
        }
    } else {
        let ppt = office_oxide::ppt::PptDocument::from_reader(reader)
            .map_err(|error| error.to_string())?;
        for (index, slide) in ppt.slides.iter().enumerate() {
            super::check_time(request.deadline).map_err(|error| error.to_string())?;
            let range = SourceRange::new(
                SourceRangeUnit::Slide,
                index as u64 + 1,
                index as u64 + 2,
                None,
            );
            builder.read(range.clone());
            if index >= limits.pptx_max_slides as usize {
                bounded = true;
                continue;
            }
            for run in &slide.text_runs {
                if include_notes || run.text_type != office_oxide::ppt::TextType::Notes {
                    bounded |=
                        !builder.push(&run.text, ContentUnitKind::Slide, vec![range.clone()]);
                }
            }
        }
    }
    let (content, audit, truncated) = builder.finish();
    Ok((content, audit, bounded || truncated))
}
