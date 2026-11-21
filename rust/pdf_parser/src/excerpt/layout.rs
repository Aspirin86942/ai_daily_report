use super::lines;
use crate::{is_valid_text_char, PdfError};
use pdfium_render::prelude::*;

struct TextRun {
    object: usize,
    rect: PdfRect,
}

pub(crate) fn read_page_text(page: &PdfPage<'_>) -> Result<String, PdfError> {
    let text = page.text()?;
    Ok(format_page_text(page, &text, text.all()))
}

pub(crate) fn format_page_text(page: &PdfPage<'_>, text: &PdfPageText<'_>, raw: String) -> String {
    if lines(&raw).nth(1).is_none() {
        return raw;
    }
    columns(page, text, &raw).unwrap_or(raw)
}

fn columns(page: &PdfPage<'_>, text: &PdfPageText<'_>, raw: &str) -> Option<String> {
    // Positions are cheap compared with repeated full-height text queries. Use
    // native text order when a highly fragmented page cannot be checked within this bound.
    if !(4..=2048).contains(&page.objects().len()) {
        return None;
    }
    let mut runs = Vec::new();
    for (object, item) in page.objects().iter().enumerate() {
        if item.as_text_object().is_some() {
            runs.push(TextRun {
                object,
                rect: item.bounds().ok()?.to_rect(),
            });
        }
    }
    let bounds = page.page_size();
    let left = bounds.left().value;
    let right = bounds.right().value;
    let width = right - left;
    let region = |bottom, left, top, right| {
        text.inside_rect(PdfRect::new_from_values(bottom, left, top, right))
    };
    for fraction in [0.5, 0.4, 0.6] {
        let middle = left + width * fraction;
        let gutter = width * 0.02;
        let Some((bottom, top)) = column_band(&runs, middle, gutter) else {
            continue;
        };
        let bottom = bottom.max(bounds.bottom().value);
        let top = top.min(bounds.top().value);
        let second = region(bottom, middle, top, right);
        let right_count = lines(&second).count();
        if right_count < 2 {
            continue;
        }
        let values = lines(&second).filter(|line| is_value_cell(line)).count();
        let table = if values > 0 {
            // Do not zip the two text streams: objects can be drawn in reverse
            // order or a row can be missing a cell. Match the actual baselines.
            let (rows, paired) = table_rows(page, text, &runs, middle, bottom, top)?;
            (paired >= 2 || (paired == 1 && values * 2 >= right_count)).then_some(rows)
        } else {
            None
        };
        let body = if let Some(rows) = table {
            rows
        } else {
            let first = region(bottom, left, top, middle);
            if lines(&first).nth(1).is_none() {
                continue;
            }
            format!("{}\n{}", first.trim_end(), second.trim_start())
        };
        let significant = |s: &str| s.chars().filter(|&c| is_valid_text_char(c)).count();
        let raw_count = significant(raw);
        let body_count = significant(&body);
        if body_count == raw_count {
            return Some(body);
        }
        let header = region(top, left, bounds.top().value, right);
        let footer = region(bounds.bottom().value, left, bottom, right);
        if significant(&header) + body_count + significant(&footer) == raw_count {
            return Some(
                [header.trim(), body.trim(), footer.trim()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }
    None
}

fn column_band(runs: &[TextRun], middle: f32, gutter: f32) -> Option<(f32, f32)> {
    let mut sides = [(f32::INFINITY, f32::NEG_INFINITY, 0usize); 2];
    for run in runs {
        let b = run.rect;
        let side = if b.right().value <= middle - gutter {
            0
        } else if b.left().value >= middle + gutter {
            1
        } else {
            continue;
        };
        sides[side].0 = sides[side].0.min(b.bottom().value);
        sides[side].1 = sides[side].1.max(b.top().value);
        sides[side].2 += 1;
    }
    let [(lb, lt, lc), (rb, rt, rc)] = sides;
    if lc < 2 || rc < 2 || lb.max(rb) >= lt.min(rt) {
        return None;
    }
    let bottom = lb.min(rb) - 1.0;
    let top = lt.max(rt) + 1.0;
    let crosses = runs.iter().any(|run| {
        let b = run.rect;
        b.top().value > bottom
            && b.bottom().value < top
            && b.right().value > middle - gutter
            && b.left().value < middle + gutter
    });
    (!crosses).then_some((bottom, top))
}

fn is_value_cell(value: &str) -> bool {
    let Some(number) = value.find(char::is_numeric) else {
        return false;
    };
    let prefix = value[..number]
        .trim_matches(|c: char| c.is_whitespace() || "+-−~≈<>=≤≥($¥￥€£₹".contains(c));
    let currency = prefix.is_empty()
        || prefix == "人民币"
        || (prefix.len() == 3 && prefix.bytes().all(|b| b.is_ascii_uppercase()));
    // A unit may be joined to the number, spaced, or a ratio such as kg / m².
    let suffix =
        value[number..].trim_start_matches(|c: char| c.is_numeric() || ".,，'’+-− ".contains(c));
    currency
        && suffix.chars().count() <= 16
        && suffix.split_whitespace().count() <= 3
        && !suffix.contains([':', '：'])
}

fn table_rows(
    page: &PdfPage<'_>,
    text: &PdfPageText<'_>,
    runs: &[TextRun],
    middle: f32,
    bottom: f32,
    top: f32,
) -> Option<(String, usize)> {
    let center = |r: &TextRun| (r.rect.bottom().value + r.rect.top().value) / 2.0;
    let mut remaining: Vec<_> = runs
        .iter()
        .filter(|r| (bottom..=top).contains(&center(r)))
        .collect();
    remaining.sort_by(|a, b| center(b).total_cmp(&center(a)));
    let mut rows = Vec::new();
    let mut paired = 0;
    let mut rest = remaining.as_mut_slice();
    while let Some(first) = rest.first().copied() {
        let count = rest
            .iter()
            .take_while(|r| {
                (center(first) - center(r)).abs()
                    <= 0.45 * first.rect.height().value.min(r.rect.height().value)
            })
            .count();
        let (row, tail) = rest.split_at_mut(count.max(1));
        rest = tail;
        row.sort_by(|a, b| a.rect.left().value.total_cmp(&b.rect.left().value));
        let split = row.partition_point(|r| r.rect.right().value <= middle);
        let left = cell_text(page, text, &row[..split])?;
        let right = cell_text(page, text, &row[split..])?;
        if !left.trim().is_empty() && is_value_cell(right.trim()) {
            paired += 1;
        }
        rows.push(match (left.trim(), right.trim()) {
            ("", value) => value.to_owned(),
            (label, "") => label.to_owned(),
            (label, value) => format!("{label}\t{value}"),
        });
    }
    Some((rows.join("\n"), paired))
}

fn cell_text(page: &PdfPage<'_>, text: &PdfPageText<'_>, runs: &[&TextRun]) -> Option<String> {
    let Some((first, others)) = runs.split_first() else {
        return Some(String::new());
    };
    let value = if others.is_empty() {
        let object = page.objects().get(first.object).ok()?;
        text.for_object(object.as_text_object()?)
    } else {
        // A number and its unit can be separate draw operations. Ask the same
        // PDFium text handle for this cell, preserving native spaces and decimal points.
        let mut bounds = first.rect;
        for run in others {
            bounds = PdfRect::new_from_values(
                bounds.bottom().value.min(run.rect.bottom().value),
                bounds.left().value.min(run.rect.left().value),
                bounds.top().value.max(run.rect.top().value),
                bounds.right().value.max(run.rect.right().value),
            );
        }
        let native = text.inside_rect(bounds);
        return Some(lines(&native).collect::<Vec<_>>().join(" "));
    };
    (lines(&value).count() <= 1).then_some(value)
}
