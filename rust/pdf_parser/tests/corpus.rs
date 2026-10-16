use ai_daily_pdf_parser::{PdfError, PdfReader};
use std::path::PathBuf;

fn source(path: &std::path::Path) -> String {
    let meta = std::fs::metadata(path).unwrap();
    ai_daily_discovery::build_source_version(
        meta.modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        meta.len(),
    )
}

#[test]
fn native_classification_preserves_sampling_and_failure_contracts() {
    use ai_daily_worker_contract::{ClassifyRequest, ClassifyStatus};
    let reader = reader();
    for (name, status) in [
        ("text_cjk_01.pdf", ClassifyStatus::TextInParseWindow),
        (
            "no_text_beyond_max_pages_01.pdf",
            ClassifyStatus::TextInParseWindow,
        ),
    ] {
        let path = corpus(&format!("pdf_classifier/{name}"));
        let request = ClassifyRequest {
            file_path: path.to_string_lossy().into_owned(),
            source_version: source(&path),
            max_pages: 5,
            policy_version: "pdf_text_presence_v2".into(),
        };
        let result = reader.classify_request(&request);
        result.validate_for_max_pages(5).unwrap();
        assert_eq!(result.status, status, "{name}");
        let audit = result.page_audit.unwrap();
        assert!(audit.observations.unwrap().last().unwrap().has_text);
        let mut stale = request;
        stale.source_version = "mtime_ns=1:size=1".into();
        assert_eq!(
            reader
                .classify_request(&stale)
                .diagnostic
                .unwrap()
                .error_code,
            "SOURCE_VERSION_CHANGED"
        );
    }
    for character in ['\0', '\u{200b}', '\u{e000}', '\u{fffd}', ' '] {
        assert!(!ai_daily_pdf_parser::is_valid_text_char(character));
    }
    for character in ['中', 'A', '1', '𠀀'] {
        assert!(ai_daily_pdf_parser::is_valid_text_char(character));
    }
}

#[test]
fn native_parse_preserves_page_ranges_and_character_limits() {
    use ai_daily_worker_contract::*;
    let path = corpus("pdf_classifier/text_cjk_01.pdf");
    let request = ParseRequest {
        file_path: path.to_string_lossy().into_owned(),
        file_type: ".pdf".into(),
        backend: ParserBackend::RustPdfiumV3,
        remaining_timeout_ms: 30_000,
        max_file_size_bytes: 10_000_000,
        parser_limits: ParserLimits::Pdf {
            max_pages: 4,
            excerpt_max_chars: 20,
            selected_pages: Some(vec![1]),
        },
        expected_source_version: source(&path),
    };
    let result = reader().parse_request(&request).unwrap();
    result.validate().unwrap();
    assert!(result.content.chars().count() <= 20);
    assert!(!result.content.is_empty());
    assert_eq!(result.worker_lane, WorkerLane::RustPdfProcessV3);
    assert_eq!(
        result.evidence_audit.unwrap().read_ranges.unwrap()[0].start,
        1
    );
}

fn reader() -> PdfReader {
    let path = std::env::var_os("AI_DAILY_TEST_PDFIUM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/pdfium/bin"));
    PdfReader::open(&path)
        .expect("prepare pinned PDFium using scripts/prepare_rust_dependencies.ps1")
}

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

#[test]
fn real_corpus_covers_chinese_english_blank_encrypted_and_corrupt() {
    let reader = reader();
    for name in [
        "text_plain_01.pdf",
        "text_cjk_01.pdf",
        "text_mixed_01.pdf",
        "text_image_mixed_01.pdf",
    ] {
        let result = reader
            .read(&corpus(&format!("pdf_classifier/{name}")), None, 20, 6000)
            .unwrap();
        assert!(
            result.pages.iter().any(|page| !page.text.trim().is_empty()),
            "{name}"
        );
        if name.contains("cjk") {
            assert!(result
                .pages
                .iter()
                .flat_map(|page| page.text.chars())
                .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch)));
        }
    }
    let blank = reader
        .read(
            &corpus("pdf_classifier/no_text_blank_01.pdf"),
            None,
            20,
            6000,
        )
        .unwrap();
    assert!(blank.pages.iter().all(|page| page.text.trim().is_empty()));
    for name in ["error_encrypted_01.pdf", "error_corrupt_01.pdf"] {
        assert!(matches!(
            reader.read(&corpus(&format!("pdf_classifier/{name}")), None, 20, 6000),
            Err(PdfError::Parse(_))
        ));
    }
}

#[test]
fn selection_preserves_requested_page_order_and_bounds_unicode_characters() {
    let reader = reader();
    let path = corpus("pdf_classifier/no_text_beyond_max_pages_01.pdf");
    let all = reader.read(&path, None, 100, 100000).unwrap();
    assert!(all.pages_total >= 2);
    let last = all.pages_total;
    let selected = reader.read(&path, Some(&[last, 1]), 2, 100000).unwrap();
    assert_eq!(
        selected
            .pages
            .iter()
            .map(|page| page.page)
            .collect::<Vec<_>>(),
        [last, 1]
    );
    assert_eq!(selected.pages[0].text, all.pages[last as usize - 1].text);
    let bounded = reader.read(&path, Some(&[last]), 1, 5).unwrap();
    assert_eq!(bounded.pages[0].text.chars().count(), 5);
    assert!(bounded.truncated && bounded.pages[0].truncated);
    for pages in [&[0][..], &[all.pages_total + 1][..], &[1, 1][..]] {
        assert!(matches!(
            reader.read(&path, Some(pages), 5, 100),
            Err(PdfError::Pages)
        ));
    }
}

#[test]
fn missing_or_different_dll_fails_without_system_lookup() {
    let temp = tempfile::tempdir().unwrap();
    assert!(matches!(
        PdfReader::open(temp.path()),
        Err(PdfError::LibraryMissing(_))
    ));
    std::fs::write(temp.path().join("pdfium.dll"), b"different version").unwrap();
    assert!(matches!(
        PdfReader::open(temp.path()),
        Err(PdfError::LibraryIdentity)
    ));
}
