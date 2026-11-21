use crate::{is_valid_text_char, page_images, page_numbers, PdfError, PdfReader};
use ai_daily_worker_contract::{
    AuditTextBuilder, ContentUnitKind, SourceRange, SourceRangeUnit, OCR_CHARS_PER_FILE,
    OCR_PAGES_PER_FILE,
};
use pdfium_render::prelude::*;
use std::{path::Path, time::Instant};
mod inference;

const MAX_SIDE: i32 = 1920;

pub(super) fn extract(
    reader: &PdfReader,
    path: &Path,
    pages: &[u32],
    max_chars: u64,
    deadline: Instant,
    builder: &mut AuditTextBuilder,
) -> Result<bool, PdfError> {
    if pages.is_empty() || pages.len() as u64 > OCR_PAGES_PER_FILE {
        return Err(PdfError::OcrLimits);
    }
    check_time(deadline)?;
    let document = reader.pdfium.load_pdf_from_file(path, None)?;
    let (total, _) = page_numbers(&document, Some(pages), OCR_PAGES_PER_FILE as u32)?;
    let mut truncated = pages.len() < total as usize;
    let mut remaining = max_chars.min(OCR_CHARS_PER_FILE);
    let mut engine = None;
    for &number in pages {
        check_time(deadline)?;
        let page = document.pages().get((number - 1) as PdfPageIndex)?;
        if page.text()?.all().chars().any(is_valid_text_char) || page_images(&page) != Some(true) {
            return Err(PdfError::OcrIneligible);
        }
        let prefix = format!("【离线 OCR，第 {number} 页，待核对原文】");
        let prefix_chars = prefix.chars().count() as u64;
        if remaining <= prefix_chars {
            truncated = true;
            break;
        }
        let (width, height) = render_dimensions(page.width().value, page.height().value)?;
        let config = PdfRenderConfig::new()
            .set_target_size(width, height)
            .set_maximum_width(width)
            .set_maximum_height(height)
            .set_format(PdfBitmapFormat::BGRA)
            .set_reverse_byte_order(false);
        let bitmap = page.render_with_config(&config)?;
        check_time(deadline)?;
        let width = bitmap.width();
        let height = bitmap.height();
        if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) {
            return Err(PdfError::OcrFailed);
        }
        let pixels = bitmap.as_raw_bytes();
        drop(bitmap);
        if pixels.len() != width as usize * height as usize * 4 {
            return Err(PdfError::OcrFailed);
        }
        let engine = match &mut engine {
            Some(engine) => engine,
            slot => slot.insert(inference::load(&reader.program_dir.join("ocr-models"))?),
        };
        check_time(deadline)?;
        let result =
            inference::recognize(engine, &pixels, width, height, remaining - prefix_chars)?;
        check_time(deadline)?;
        let range = SourceRange::new(
            SourceRangeUnit::PdfPage,
            u64::from(number),
            u64::from(number) + 1,
            Some(ai_daily_worker_contract::OCR_RANGE_LABEL.into()),
        );
        builder.read(range.clone());
        truncated |= result.truncated;
        for line in result.lines {
            let line = line.trim();
            if !line.chars().any(is_valid_text_char) {
                continue;
            }
            let cost = prefix_chars + line.chars().count() as u64;
            if cost > remaining
                || !builder.push(
                    &format!("{prefix}{line}"),
                    ContentUnitKind::Text,
                    vec![range.clone()],
                )
            {
                truncated = true;
                continue;
            }
            // The next admitted line has a newline separator.
            remaining = remaining.saturating_sub(cost + 1);
        }
    }
    Ok(truncated)
}

fn render_dimensions(width: f32, height: f32) -> Result<(i32, i32), PdfError> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(PdfError::Pages);
    }
    // 180 dpi on ordinary pages. Cap the longer side before allocating a bitmap.
    let scale = (MAX_SIDE as f32 / width.max(height)).min(2.5);
    let pixels = |points: f32| ((points * scale).round() as i32).clamp(1, MAX_SIDE);
    Ok((pixels(width), pixels(height)))
}

fn check_time(deadline: Instant) -> Result<(), PdfError> {
    if Instant::now() >= deadline {
        return Err(PdfError::OcrTimeout);
    }
    Ok(())
}

struct OcrPage {
    lines: Vec<String>,
    truncated: bool,
}
