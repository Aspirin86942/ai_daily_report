use crate::{check_deadline, is_valid_text_char, PdfError};
use daily_parsers::contract::{
    AuditTextBuilder, ContentUnitKind, SourceRange, SourceRangeUnit, OCR_CHARS_PER_FILE,
    OCR_PAGES_PER_FILE, OCR_RANGE_LABEL,
};
use pdfium_render::prelude::*;
use std::{path::Path, time::Instant};
mod inference;

const MAX_SIDE: i32 = 1920;

pub(super) fn extract<'a>(
    program_dir: &Path,
    total: u64,
    pages: &[u64],
    max_chars: u64,
    deadline: Instant,
    builder: &mut AuditTextBuilder,
    mut load: impl FnMut(u64) -> Result<PdfPage<'a>, PdfError>,
) -> Result<bool, PdfError> {
    if pages.is_empty() || pages.len() as u64 > OCR_PAGES_PER_FILE {
        return Err(PdfError::OcrLimits);
    }
    check_deadline(deadline)?;
    let mut truncated = pages.len() < total as usize;
    let mut remaining = max_chars.min(OCR_CHARS_PER_FILE);
    let mut engine = None;
    for &number in pages {
        check_deadline(deadline)?;
        let page = load(number)?;
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
        check_deadline(deadline)?;
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
            slot => slot.insert(inference::load(&program_dir.join("ocr-models"))?),
        };
        check_deadline(deadline)?;
        let lines = inference::recognize(engine, &pixels, width, height)?;
        check_deadline(deadline)?;
        let range = SourceRange::new(
            SourceRangeUnit::PdfPage,
            number,
            number + 1,
            Some(OCR_RANGE_LABEL.into()),
        );
        builder.read(range.clone());
        truncated |= lines.len() >= 128;
        let mut recognized_chars = remaining - prefix_chars;
        for line in lines {
            let line = line.text.trim();
            let chars = line.chars().count() as u64;
            // Keep the original recognition window even when a line's label
            // cannot fit in the separate output budget.
            if chars > recognized_chars {
                truncated = true;
                continue;
            }
            recognized_chars -= chars;
            if !line.chars().any(is_valid_text_char) {
                continue;
            }
            let cost = prefix_chars + chars;
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
