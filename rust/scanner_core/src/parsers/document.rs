use super::WorkerCommand;
use crate::admission::PdfClassificationResult;
use ai_daily_scanner_contract::{pdf_sample_pages, AdapterPaths, PdfParseProfile};
use ai_daily_worker_contract::WorkerKind;
use std::path::PathBuf;

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
