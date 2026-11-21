use ai_daily_pdf_parser::{PdfError, PdfReader};
use std::{
    path::PathBuf,
    sync::{Mutex, MutexGuard, OnceLock},
};

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

fn pdfium_dir() -> PathBuf {
    std::env::var_os("AI_DAILY_TEST_PDFIUM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/pdfium/bin"))
}

fn reader() -> MutexGuard<'static, PdfReader> {
    static READER: OnceLock<Mutex<PdfReader>> = OnceLock::new();
    READER
        .get_or_init(|| {
            Mutex::new(
                PdfReader::open(&pdfium_dir())
                    .expect("prepare pinned PDFium using scripts/prepare_rust_dependencies.ps1"),
            )
        })
        .lock()
        .unwrap()
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

#[test]
fn archive_pdf_reads_beyond_excerpt_page_window() {
    use ai_daily_worker_contract::*;
    let path = corpus("pdf_classifier/no_text_beyond_max_pages_01.pdf");
    let directory = tempfile::tempdir().unwrap();
    let request = archive::ArchiveRequest {
        source: ParseRequest {
            file_path: path.to_string_lossy().into_owned(),
            file_type: ".pdf".into(),
            backend: ParserBackend::RustPdfiumV3,
            remaining_timeout_ms: 30000,
            max_file_size_bytes: 512 * 1024 * 1024,
            parser_limits: ParserLimits::Pdf {
                max_pages: 5,
                excerpt_max_chars: 1000,
                selected_pages: None,
            },
            expected_source_version: source(&path),
        },
        raw_sha256: "a".repeat(64),
        output_dir: directory.path().to_owned(),
        max_output_bytes: 16 * 1024 * 1024,
    };
    let result = reader().archive_request(&request).unwrap();
    assert!(!result.truncated);
    assert!(result.usable_chars > 0);
    let units = std::fs::read_to_string(directory.path().join("units.jsonl")).unwrap();
    assert!(units
        .lines()
        .any(|line| serde_json::from_str::<archive::ArchiveUnit>(line)
            .unwrap()
            .ranges
            .iter()
            .any(|range| range.start > 5)));
}

// Each stream uses real PDF text objects at point coordinates. The last page
// draws alternating columns, so stream order alone associates the wrong terms.
fn settlement_pdf(path: &std::path::Path) {
    let row = |x, y, value: &str| format!("BT /F1 11 Tf {x} {y} Td ({value}) Tj ET\n");
    let first: String = (0..42)
        .map(|i| {
            row(
                35,
                740 - 16 * i,
                &format!("Cover item {i:02} with detailed history."),
            )
        })
        .collect();
    let pages = [
        first,
        row(35, 730, "Receipt approved."),
        row(35, 730, "UNREAD PAGE THREE"),
        row(35, 730, "UNREAD PAGE FOUR"),
        IMAGE_STREAM.into(),
        [
            row(35, 720, "Settlement: USD 12,340.00"),
            row(330, 720, "Retention: USD 780.00"),
            row(35, 696, "After written approval"),
            row(330, 696, "Due after inspection"),
        ]
        .concat(),
    ];
    write_pdf(path, &pages);
}

const IMAGE_STREAM: &str =
    "q 200 0 0 100 20 400 cm BI /W 1 /H 1 /BPC 8 /CS /G /F /AHx ID 80> EI Q\n";

fn write_pdf(path: &std::path::Path, pages: &[String]) {
    use std::fmt::Write;
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!(
            "<< /Type /Pages /Count {} /Kids [{}] >>",
            pages.len(),
            (0..pages.len())
                .map(|i| format!("{} 0 R", 4 + i * 2))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    for stream in pages {
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", objects.len() + 2));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        writeln!(pdf, "{} 0 obj\n{}\nendobj", i + 1, object).unwrap();
    }
    let xref = pdf.len();
    writeln!(pdf, "xref\n0 {}\n0000000000 65535 f ", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
        objects.len() + 1
    )
    .unwrap();
    std::fs::write(path, pdf).unwrap();
}

#[test]
fn long_cover_cannot_spend_other_pages_lines_or_mix_settlement_terms() {
    use ai_daily_worker_contract::*;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settlement.pdf");
    settlement_pdf(&path);
    let request = ParseRequest {
        file_path: path.to_string_lossy().into_owned(),
        file_type: ".pdf".into(),
        backend: ParserBackend::RustPdfiumV3,
        remaining_timeout_ms: 30_000,
        max_file_size_bytes: 1_000_000,
        expected_source_version: source(&path),
        parser_limits: ParserLimits::Pdf {
            max_pages: 4,
            excerpt_max_chars: 201,
            selected_pages: Some(vec![1, 2, 5, 6]),
        },
    };
    let result = reader().parse_request(&request).unwrap();
    result.validate().unwrap();
    assert!(result.truncated);
    assert!(result.content.chars().count() <= 201);
    assert!(!result.content.contains("UNREAD"));
    let audit = result.evidence_audit.unwrap();
    let read: Vec<_> = audit
        .read_ranges
        .unwrap()
        .into_iter()
        .map(|r| (r.start, r.end))
        .collect();
    assert_eq!(read, [(1, 2), (2, 3), (5, 6), (6, 7)]);
    let mut final_lines = vec![];
    let mut pages = vec![];
    for unit in audit.content_units.unwrap() {
        assert_eq!(unit.ranges.len(), 1);
        let page = unit.ranges[0].start;
        let line: String = result
            .content
            .chars()
            .skip(unit.text_start as usize)
            .take((unit.text_end - unit.text_start) as usize)
            .collect();
        match page {
            1 => {
                assert!(line.starts_with("Cover item ") && line.ends_with("with detailed history."))
            }
            2 => assert_eq!(line, "Receipt approved."),
            6 => final_lines.push(line),
            _ => panic!("unexpected text on page {page}"),
        }
        if pages.last() != Some(&page) {
            pages.push(page);
        }
    }
    assert_eq!(pages, [1, 2, 6]);
    assert_eq!(
        final_lines,
        [
            "Settlement: USD 12,340.00",
            "After written approval",
            "Retention: USD 780.00",
            "Due after inspection"
        ]
    );
}

#[test]
fn image_only_pages_have_observed_ranges_and_zero_text() {
    use ai_daily_worker_contract::*;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("image-only.pdf");
    write_pdf(&path, &[IMAGE_STREAM.into()]);
    let reader = reader();
    let result = reader.classify_request(&ClassifyRequest {
        file_path: path.to_string_lossy().into_owned(),
        source_version: source(&path),
        max_pages: 5,
        policy_version: "pdf_text_presence_v2".into(),
    });
    assert_eq!(result.status, ClassifyStatus::NoTextInParseWindow);
    let observed = result.page_audit.unwrap().observations.unwrap();
    assert!(observed.iter().all(|page| !page.has_text));
    assert!(observed.iter().any(|page| page.has_image == Some(true)));
    let parsed = reader
        .parse_request(&ParseRequest {
            file_path: path.to_string_lossy().into_owned(),
            file_type: ".pdf".into(),
            backend: ParserBackend::RustPdfiumV3,
            remaining_timeout_ms: 30_000,
            max_file_size_bytes: 1_000_000,
            expected_source_version: source(&path),
            parser_limits: ParserLimits::Pdf {
                max_pages: 1,
                excerpt_max_chars: 100,
                selected_pages: Some(vec![1]),
            },
        })
        .unwrap();
    assert!(parsed.content.is_empty());
    let audit = parsed.evidence_audit.unwrap();
    assert_eq!(audit.usable_chars, Some(0));
    assert!(audit.content_units.unwrap().is_empty());
    assert_eq!(audit.read_ranges.unwrap()[0].start, 1);
}

#[test]
fn offline_ocr_worker_reads_bilingual_image_and_labels_the_original_page() {
    use ai_daily_worker_contract::ParseRequest;
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ocr-image-pages.pdf");
    let request = |kind: &str| {
        serde_json::from_value::<ParseRequest>(serde_json::json!({
        "file_path":path,"file_type":".pdf","backend":"rust_pdfium_v3",
        "expected_source_version":source(&path),"remaining_timeout_ms":15_000,"max_file_size_bytes":1_000_000,
        "parser_limits":{"kind":kind,"max_pages":1,"selected_pages":[2],"excerpt_max_chars":256}
    })).unwrap()
    };
    let reader = reader();
    assert!(reader
        .parse_request(&request("pdf"))
        .unwrap()
        .content
        .is_empty());
    let result = reader.parse_request(&request("pdf_image_ocr"));
    if ["det.mnn", "rec.mnn", "dict.txt"]
        .iter()
        .any(|name| !pdfium_dir().join("ocr-models").join(name).is_file())
    {
        let error = result.unwrap_err();
        assert_eq!(error.error_code, "PARSER_FAILED");
        assert_eq!(error.message, "PDF_OCR_MODEL_MISSING");
        return;
    }
    let parsed = result.unwrap();
    parsed.validate().unwrap();
    assert!(parsed.truncated && parsed.content.chars().count() <= 256);
    let text: String = parsed.content.split_whitespace().collect();
    for expected in ["验收", "1936", "YEAR", "2064"] {
        assert!(
            text.to_uppercase().contains(expected),
            "missing synthetic anchor {expected}"
        );
    }
    let audit = parsed.evidence_audit.unwrap();
    let ranges = audit.read_ranges.unwrap();
    assert_eq!(ranges.len(), 1);
    assert_eq!((ranges[0].start, ranges[0].end), (2, 3));
    assert_eq!(
        ranges[0].label.as_deref(),
        Some(ai_daily_pdf_parser::OCR_RANGE_LABEL)
    );
    for unit in audit.content_units.unwrap() {
        assert_eq!(unit.ranges, ranges);
        let line: String = parsed
            .content
            .chars()
            .skip(unit.text_start as usize)
            .take((unit.text_end - unit.text_start) as usize)
            .collect();
        assert!(line.starts_with("【离线 OCR，第 2 页，待核对原文】"));
    }
}

#[test]
fn five_page_sample_preserves_settlement_and_review_condition_at_3200_chars() {
    use ai_daily_worker_contract::*;
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/settlement-pages.pdf");
    let reader = reader();
    let classification = reader.classify_request(&ClassifyRequest {
        file_path: path.to_string_lossy().into_owned(),
        source_version: source(&path),
        max_pages: 5,
        policy_version: "pdf_text_presence_v2".into(),
    });
    assert_eq!(classification.page_count, Some(8));
    assert_eq!(classification.status, ClassifyStatus::TextInParseWindow);
    let selected = pdf_sample_pages(8, 5, "pdf_text_presence_v2");
    let parsed = reader
        .parse_request(&ParseRequest {
            file_path: path.to_string_lossy().into_owned(),
            file_type: ".pdf".into(),
            backend: ParserBackend::RustPdfiumV3,
            remaining_timeout_ms: 30_000,
            max_file_size_bytes: 1_000_000,
            expected_source_version: source(&path),
            parser_limits: ParserLimits::Pdf {
                max_pages: 5,
                excerpt_max_chars: 3_200,
                selected_pages: Some(selected.clone()),
            },
        })
        .unwrap();
    parsed.validate().unwrap();
    assert!(parsed.truncated && parsed.content.chars().count() <= 3200);
    let audit = parsed.evidence_audit.unwrap();
    assert_eq!(
        audit
            .read_ranges
            .unwrap()
            .iter()
            .map(|r| r.start)
            .collect::<Vec<_>>(),
        selected
    );
    let units = audit.content_units.unwrap();
    for (page, fact) in [
        (5, "Effective only after review"),
        (8, "Final settlement USD 4817.25"),
    ] {
        assert!(
            units.iter().any(|u| {
                u.ranges
                    == [SourceRange::new(
                        SourceRangeUnit::PdfPage,
                        page,
                        page + 1,
                        None,
                    )]
                    && parsed
                        .content
                        .chars()
                        .skip(u.text_start as usize)
                        .take((u.text_end - u.text_start) as usize)
                        .eq(fact.chars())
            }),
            "missing original fact on page {page}"
        );
    }
}

#[test]
fn full_width_heading_stays_above_two_interleaved_columns() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("headed-columns.pdf");
    let heading = "PAYMENT AGREEMENT - SUPPLIER OBLIGATIONS AND SETTLEMENT CONDITIONS";
    let body = [
        (90, 760, heading),
        (35, 700, "Settlement"),
        (330, 700, "Retention"),
        (35, 675, "Pay upon delivery"),
        (330, 675, "Release after review"),
    ];
    let stream = body
        .iter()
        .map(|(x, y, s)| format!("BT /F1 11 Tf {x} {y} Td ({s}) Tj ET\n"))
        .collect();
    write_pdf(&path, &[stream]);
    let parsed = reader().read(&path, Some(&[1]), 1, 1000).unwrap();
    assert!(!parsed.truncated);
    assert_eq!(
        parsed.pages[0].text.lines().collect::<Vec<_>>(),
        [
            heading,
            "Settlement",
            "Pay upon delivery",
            "Retention",
            "Release after review"
        ]
    );
}

#[test]
fn two_column_table_binds_amounts_and_quantities_at_their_physical_rows() {
    use ai_daily_worker_contract::*;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tabular-amounts.pdf");
    // Draw columns separately, reverse the value objects, and leave a label without a value.
    let cells = [
        (40, 740, "Item"),
        (40, 710, "Freight"),
        (40, 676, "Packaging"),
        (40, 650, "Pending"),
        (40, 620, "Spares"),
        (350, 620, "5 kg"),
        (350, 677, "18"),
        (366, 677, "units"),
        (350, 711, "USD"),
        (379, 711, "12,345.60"),
        (350, 740, "Amount"),
    ];
    let stream = cells
        .iter()
        .map(|(x, y, s)| format!("BT /F1 11 Tf {x} {y} Td ({s}) Tj ET\n"))
        .collect();
    write_pdf(&path, &[stream]);
    let mut request = ParseRequest {
        file_path: path.to_string_lossy().into_owned(),
        file_type: ".pdf".into(),
        backend: ParserBackend::RustPdfiumV3,
        remaining_timeout_ms: 30_000,
        max_file_size_bytes: 1_000_000,
        expected_source_version: source(&path),
        parser_limits: ParserLimits::Pdf {
            max_pages: 1,
            excerpt_max_chars: 200,
            selected_pages: Some(vec![1]),
        },
    };
    let reader = reader();
    let result = reader.parse_request(&request).unwrap();
    assert!(!result.truncated);
    assert_eq!(
        result.content.lines().collect::<Vec<_>>(),
        [
            "Item\tAmount",
            "Freight\tUSD 12,345.60",
            "Packaging\t18 units",
            "Pending",
            "Spares\t5 kg"
        ]
    );
    let audit = result.evidence_audit.unwrap();
    assert_eq!(audit.content_units.unwrap().len(), 5);
    request.parser_limits = ParserLimits::Pdf {
        max_pages: 1,
        excerpt_max_chars: 12,
        selected_pages: Some(vec![1]),
    };
    let small = reader.parse_request(&request).unwrap();
    assert!(small.truncated && small.content.chars().count() <= 12);
    assert!(small
        .content
        .lines()
        .all(|line| result.content.lines().any(|whole| whole == line)));
}

#[test]
fn oversized_quantity_is_a_read_but_omitted_page() {
    use ai_daily_worker_contract::*;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("amount.pdf");
    write_pdf(
        &path,
        &["BT /F1 12 Tf 40 720 Td (12345.67 kg) Tj ET\n".into()],
    );
    let reader = reader();
    for (limit, expected) in [
        (4, ""),
        (6, ""),
        (7, ""),
        (8, ""),
        (9, ""),
        (10, ""),
        (11, "12345.67 kg"),
    ] {
        let result = reader
            .parse_request(&ParseRequest {
                file_path: path.to_string_lossy().into_owned(),
                file_type: ".pdf".into(),
                backend: ParserBackend::RustPdfiumV3,
                remaining_timeout_ms: 30_000,
                max_file_size_bytes: 1_000_000,
                expected_source_version: source(&path),
                parser_limits: ParserLimits::Pdf {
                    max_pages: 1,
                    excerpt_max_chars: limit,
                    selected_pages: Some(vec![1]),
                },
            })
            .unwrap();
        assert_eq!(result.content, expected, "budget {limit}");
        assert_eq!(result.truncated, expected.is_empty());
        let audit = result.evidence_audit.unwrap();
        assert_eq!(
            audit.read_ranges.unwrap(),
            [SourceRange::new(SourceRangeUnit::PdfPage, 1, 2, None)]
        );
        assert_eq!(audit.content_units.unwrap().is_empty(), expected.is_empty());
    }
}

#[test]
fn dense_pdf_keeps_complete_lines_with_a_small_budget() {
    use std::fmt::Write;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("dense.pdf");
    let row = "checked ".repeat(120);
    let mut cover = String::new();
    for index in 0..80 {
        writeln!(
            cover,
            "BT /F1 0.4 Tf 30 {} Td ({row}) Tj ET",
            740 - 7 * index
        )
        .unwrap();
    }
    cover.push_str("BT /F1 11 Tf 30 60 Td (Cover) Tj ET\n");
    write_pdf(
        &path,
        &[
            cover,
            "BT /F1 11 Tf 30 700 Td (Need approval) Tj ET\n".into(),
        ],
    );
    let parsed = reader().read(&path, Some(&[1, 2]), 2, 24).unwrap();
    assert!(parsed.pages[0].chars_total > 65_536);
    assert!(parsed.truncated && parsed.pages[0].truncated && !parsed.pages[1].truncated);
    assert_eq!(parsed.pages[0].text, "Cover");
    assert_eq!(parsed.pages[1].text, "Need approval");
}
