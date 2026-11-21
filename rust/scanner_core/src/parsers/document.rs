use super::WorkerCommand;
use crate::admission::PdfClassificationResult;
use ai_daily_scanner_contract::{pdf_sample_pages, AdapterPaths, PdfParseProfile};
use ai_daily_worker_contract::{ParserLimits, WorkerKind, OCR_CHARS_PER_FILE};
use std::path::PathBuf;

pub(crate) use ai_daily_scanner_contract::{OCR_BACKEND, OCR_RANGE_LABEL};

pub(crate) fn valid_ocr_ranges(
    content: &str,
    audit: &ai_daily_worker_contract::ParsedContentAudit,
    requested: Option<&[u64]>,
) -> bool {
    let (Some(read), Some(units)) = (&audit.read_ranges, &audit.content_units) else {
        return false;
    };
    read.len() as u64 <= ai_daily_worker_contract::OCR_PAGES_PER_FILE
        && read.iter().all(|range| {
            range.unit == ai_daily_worker_contract::SourceRangeUnit::PdfPage
                && range.label.as_deref() == Some(OCR_RANGE_LABEL)
                && requested.is_none_or(|pages| pages.contains(&range.start))
                && range.start.checked_add(1) == Some(range.end)
        })
        && (content.is_empty() || !units.is_empty())
        && units.iter().all(|unit| {
            !unit.ranges.is_empty() && unit.ranges.iter().all(|range| read.contains(range))
        })
}

/// The cache and the worker serialize these same limits. Spread a small OCR
/// allowance over physical pages, independent of the native text sampling order.
pub(crate) fn image_ocr_limits(
    page_count: u64,
    max_pages: u64,
    profile: &PdfParseProfile,
) -> ParserLimits {
    let selected_pages = match max_pages {
        1 => vec![1],
        2 => vec![1, page_count],
        3 => vec![1, page_count.div_ceil(2), page_count],
        _ => Vec::new(),
    };
    ParserLimits::PdfImageOcr {
        max_pages,
        excerpt_max_chars: profile.excerpt_max_chars.min(OCR_CHARS_PER_FILE),
        selected_pages,
    }
}

pub(super) fn selected_pages(
    profile: &PdfParseProfile,
    result: &PdfClassificationResult,
) -> Option<Vec<u64>> {
    let count = result.page_count?;
    if profile.selection_policy_version == "pdf_text_presence_v2"
        && profile.max_pages == 2
        && count > 2
    {
        // The two-page classifier stops at its first text page. Keep that page
        // and spend the other slot on the end of the document.
        return Some(vec![
            result.result_examined_pages.unwrap_or(1).clamp(1, 2),
            count,
        ]);
    }
    Some(pdf_sample_pages(
        count,
        profile.max_pages,
        &profile.selection_policy_version,
    ))
}

pub fn worker_command(adapters: &AdapterPaths) -> WorkerCommand {
    WorkerCommand {
        program: PathBuf::from(&adapters.pdf_worker_path),
        base_args: Vec::new(),
        current_dir: None,
        expected_kind: WorkerKind::Pdf,
        required_backends: vec!["rust_pdfium_v3".into()],
        required_extensions: vec![".pdf".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_daily_scanner_contract::PdfClassificationStatus;

    #[test]
    fn pdf_request_keeps_confirmed_text_and_last_page_with_two_slots() {
        let mut profile = PdfParseProfile {
            image_ocr: None,
            backend: "rust_pdfium_v3".into(),
            max_pages: 2,
            excerpt_max_chars: 300,
            selection_policy_version: "pdf_text_presence_v2".into(),
        };
        let mut result = PdfClassificationResult {
            file_identity: "sample".into(),
            status: PdfClassificationStatus::TextInParseWindow,
            page_count: Some(8),
            result_examined_pages: Some(1),
            error_code: None,
        };
        assert_eq!(selected_pages(&profile, &result).unwrap(), [1, 8]);
        result.result_examined_pages = Some(2);
        assert_eq!(selected_pages(&profile, &result).unwrap(), [2, 8]);
        profile.selection_policy_version = "pdf_text_presence_v1".into();
        assert_eq!(selected_pages(&profile, &result).unwrap(), [1, 2]);
        result.page_count = None;
        assert!(selected_pages(&profile, &result).is_none());
    }
}
