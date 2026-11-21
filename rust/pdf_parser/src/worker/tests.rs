use super::*;
use std::{
    fs::{self, File},
    path::PathBuf,
    sync::{Mutex, MutexGuard, OnceLock},
};

fn reader() -> MutexGuard<'static, PdfReader> {
    static READER: OnceLock<Mutex<PdfReader>> = OnceLock::new();
    READER
        .get_or_init(|| {
            let program = std::env::var_os("AI_DAILY_TEST_PDFIUM_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/pdfium/bin")
                });
            Mutex::new(PdfReader::open(&program).expect("prepare pinned PDFium for tests"))
        })
        .lock()
        .unwrap()
}

fn corpus(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/pdf_classifier")
        .join(name)
}

fn classify(path: &Path, max_pages: u64) -> ClassifyRequest {
    let path = path.to_str().unwrap();
    ClassifyRequest {
        file_path: path.to_owned(),
        source_version: source(path).unwrap().0,
        max_pages,
        policy_version: "pdf_text_presence_v2".into(),
    }
}

fn parse(classify: &ClassifyRequest, selected_pages: Vec<u64>) -> ParseRequest {
    ParseRequest {
        file_path: classify.file_path.clone(),
        file_type: ".pdf".into(),
        backend: ParserBackend::RustPdfiumV3,
        remaining_timeout_ms: 30_000,
        max_file_size_bytes: 10_000_000,
        parser_limits: ParserLimits::Pdf {
            max_pages: selected_pages.len() as u64,
            excerpt_max_chars: 3_200,
            selected_pages: Some(selected_pages),
        },
        expected_source_version: classify.source_version.clone(),
    }
}

fn parsed(session: &mut PdfSession<'_>, request: &ParseRequest) -> ParseResult {
    let mut result = session.parse_request(request).unwrap();
    result.validate().unwrap();
    result.duration_ms = 0;
    result
}

#[test]
fn classifier_remembers_short_reads_and_failed_seeks() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("input.pdf");
    fs::write(&path, b"x").unwrap();
    let io_failed = Arc::new(AtomicBool::new(false));
    let mut input = ClassifierInput::new(File::open(path).unwrap(), &io_failed);
    assert_eq!(input.read(&mut [0; 2]).unwrap(), 1);
    assert!(io_failed.swap(false, Ordering::Relaxed));
    assert!(input.seek(SeekFrom::Current(-2)).is_err());
    assert!(io_failed.load(Ordering::Relaxed));
}

#[test]
fn reused_page_layout_and_uncovered_pages_match_independent_parse() {
    let reader = reader();
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/settlement-pages.pdf");
    let classify = classify(&path, 5);
    let mut worker = reader.session();
    let mut independent = PdfSession::new(&reader, false);
    let audit = worker.classify_request(&classify);
    assert_eq!(audit, independent.classify_request(&classify));
    let checked = audit.page_audit.unwrap().checked_pages;
    let selected = pdf_sample_pages(8, 5, &classify.policy_version);
    assert!(!checked.is_empty() && selected.starts_with(&checked));
    assert!(checked.len() < selected.len());

    let request = parse(&classify, selected.clone());
    let output = parsed(&mut worker, &request);
    assert_eq!(output, parsed(&mut independent, &request));
    assert!(output.content.contains("Final settlement USD 4817.25"));
    assert!(output.content.contains("Effective only after review"));
    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: 1,
            page_reads: selected.len()
        }
    );
    assert_eq!(
        independent.work,
        WorkCounts {
            document_opens: 2,
            page_reads: selected.len() + checked.len()
        }
    );

    // Each classification contributes to at most one parse.
    assert_eq!(parsed(&mut worker, &request), output);
    assert_eq!(worker.work.document_opens, 2);
    assert_eq!(worker.work.page_reads, 2 * selected.len());
}

#[test]
fn cold_distinct_pdfs_reuse_across_the_classification_and_parse_phases() {
    const FILES: usize = 15;
    let reader = reader();
    let mut worker = reader.session();
    let mut independent = PdfSession::new(&reader, false);
    let requests: Vec<_> = (1..=12)
        .map(|i| corpus(&format!("text_plain_{i:02}.pdf")))
        .chain((1..=3).map(|i| corpus(&format!("text_cjk_{i:02}.pdf"))))
        .map(|path| classify(&path, 1))
        .collect();

    for request in &requests {
        let result = worker.classify_request(request);
        assert_eq!(result.status, ClassifyStatus::TextInParseWindow);
        assert_eq!(result, independent.classify_request(request));
    }
    for request in &requests {
        let request = parse(request, vec![1]);
        assert_eq!(
            parsed(&mut worker, &request),
            parsed(&mut independent, &request)
        );
    }

    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: FILES,
            page_reads: FILES
        }
    );
    assert_eq!(
        independent.work,
        WorkCounts {
            document_opens: FILES * 2,
            page_reads: FILES * 2
        }
    );
}

#[test]
fn replacement_and_in_place_edit_with_unchanged_wire_version_miss_reuse() {
    let reader = reader();
    let mut worker = reader.session();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.pdf");
    let replacement = temp.path().join("replacement.pdf");
    let original = fs::read(corpus("text_plain_01.pdf")).unwrap();
    fs::write(&path, &original).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let classify = classify(&path, 1);
    let parse = parse(&classify, vec![1]);
    let initial = reader.parse_request(&parse).unwrap().content;

    assert_eq!(
        worker.classify_request(&classify).status,
        ClassifyStatus::TextInParseWindow
    );
    fs::copy(corpus("text_plain_02.pdf"), &replacement).unwrap();
    let file = File::options().write(true).open(&replacement).unwrap();
    file.set_len(original.len() as u64).unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    fs::rename(replacement, &path).unwrap();

    assert_eq!(
        source(&classify.file_path).unwrap().0,
        classify.source_version
    );
    let updated = parsed(&mut worker, &parse);
    assert_ne!(updated.content, initial);
    assert_eq!(
        updated.content,
        reader.parse_request(&parse).unwrap().content
    );
    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: 2,
            page_reads: 2
        }
    );

    assert_eq!(
        worker.classify_request(&classify).status,
        ClassifyStatus::TextInParseWindow
    );
    let mut file = File::options().write(true).open(&path).unwrap();
    std::io::Write::write_all(&mut file, &original).unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    assert_eq!(
        source(&classify.file_path).unwrap().0,
        classify.source_version
    );
    assert_eq!(parsed(&mut worker, &parse).content, initial);
    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: 4,
            page_reads: 4
        }
    );
}

#[test]
fn oldest_document_is_evicted_without_disturbing_later_hits() {
    let reader = reader();
    let mut worker = reader.session();
    let temp = tempfile::tempdir().unwrap();
    let inputs: Vec<_> = (0..=MAX_CACHED_DOCUMENTS)
        .map(|i| {
            let path = temp.path().join(format!("{i}.pdf"));
            fs::copy(corpus("text_plain_01.pdf"), &path).unwrap();
            classify(&path, 1)
        })
        .collect();

    for request in &inputs {
        assert_eq!(
            worker.classify_request(request).status,
            ClassifyStatus::TextInParseWindow
        );
    }
    assert_eq!(worker.pending.as_ref().unwrap().len(), MAX_CACHED_DOCUMENTS);

    for request in [inputs.first().unwrap(), inputs.last().unwrap()] {
        assert!(!parsed(&mut worker, &parse(request, vec![1]))
            .content
            .is_empty());
    }
    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: inputs.len() + 1,
            page_reads: inputs.len() + 1
        }
    );
}

#[test]
fn deterministic_native_markers_survive_the_session_open_path() {
    let reader = reader();
    let mut worker = reader.session();
    for (name, code, marker) in [
        ("error_corrupt_01.pdf", "PARSER_FAILED", "FPDF_ERR_FORMAT"),
        (
            "error_encrypted_01.pdf",
            "PDF_PROTECTED",
            "FPDF_ERR_PASSWORD",
        ),
    ] {
        let request = classify(&corpus(name), 1);
        let result = worker.classify_request(&request);
        assert_eq!(result, reader.classify_request(&request));
        assert_eq!(result.status, ClassifyStatus::Error);
        let error = result.diagnostic.unwrap();
        assert_eq!(
            (error.error_code.as_str(), error.message.as_str()),
            (code, marker)
        );
    }
    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: 2,
            page_reads: 0
        }
    );
    assert!(worker.pending.as_ref().unwrap().is_empty());
}

#[cfg(windows)]
#[test]
fn an_open_writer_keeps_original_sharing_and_changes_invalidate_reuse() {
    let reader = reader();
    let mut worker = reader.session();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("input.pdf");
    let original = corpus("text_plain_01.pdf");
    fs::copy(&original, &path).unwrap();
    let classify = classify(&path, 1);
    let request = parse(&classify, vec![1]);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut writer = File::options().write(true).open(&path).unwrap();
    assert_eq!(
        worker.classify_request(&classify).status,
        ClassifyStatus::TextInParseWindow
    );

    let mut replacement = fs::read(corpus("text_plain_02.pdf")).unwrap();
    replacement.resize(fs::metadata(original).unwrap().len() as usize, 0);
    std::io::Write::write_all(&mut writer, &replacement).unwrap();
    writer.set_modified(modified).unwrap();
    assert_eq!(
        source(&classify.file_path).unwrap().0,
        classify.source_version
    );
    let result = parsed(&mut worker, &request);
    assert_eq!(
        result.content,
        reader.parse_request(&request).unwrap().content
    );
    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: 2,
            page_reads: 2
        }
    );
}

#[test]
fn byte_mismatch_with_equal_stamp_reopens_the_current_pdf() {
    let reader = reader();
    let mut worker = reader.session();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("input.pdf");
    fs::copy(corpus("text_plain_01.pdf"), &path).unwrap();
    let original = classify(&path, 1);
    let before = reader.parse_request(&parse(&original, vec![1])).unwrap();
    assert_eq!(
        worker.classify_request(&original).status,
        ClassifyStatus::TextInParseWindow
    );

    let mut changed = fs::read(corpus("text_plain_02.pdf")).unwrap();
    changed.resize(fs::metadata(&path).unwrap().len() as usize, 0);
    fs::write(&path, &changed).unwrap();
    let new_request = classify(&path, 1);
    let cached = worker.pending.as_mut().unwrap().front_mut().unwrap();
    // Model a filesystem reporting the same identity and timestamps for different bytes.
    cached.stamp = OpenSource::open(&new_request.file_path).unwrap().stamp;
    let backing = Arc::downgrade(&cached.snapshot.0);

    let request = parse(&new_request, vec![1]);
    let actual = parsed(&mut worker, &request);
    assert_ne!(actual.content, before.content);
    assert_eq!(
        actual.content,
        reader.parse_request(&request).unwrap().content
    );
    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: 2,
            page_reads: 2
        }
    );
    assert!(backing.upgrade().is_none());
}

#[test]
fn pdfium_owns_readable_snapshot_until_document_close() {
    let reader = reader();
    let mut worker = reader.session();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("settlement.pdf");
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/settlement-pages.pdf");
    fs::copy(fixture, &path).unwrap();
    let request = classify(&path, 1);
    assert_eq!(
        worker.classify_request(&request).status,
        ClassifyStatus::TextInParseWindow
    );
    let ClassifiedPdf {
        document, snapshot, ..
    } = worker.take_classified(&request.file_path).unwrap();
    let backing = Arc::downgrade(&snapshot.0);
    drop(snapshot);
    fs::remove_file(&path).unwrap();

    assert!(backing.upgrade().is_some());
    {
        let page = document.pages().get(7).unwrap();
        let text = excerpt::read_page_text(&page).unwrap();
        assert!(text.contains("Final settlement USD 4817.25"));
    }
    drop(document);
    assert!(backing.upgrade().is_none());
}

fn padded_pdf(path: &Path, bytes: u64) {
    use std::io::Write;
    let original = fs::read(corpus("text_plain_01.pdf")).unwrap();
    let marker = original
        .windows(10)
        .rposition(|part| part == b"startxref\n")
        .unwrap();
    let xref: usize = std::str::from_utf8(&original[marker..])
        .unwrap()
        .lines()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let table = &original[xref..marker];
    let offset = bytes - table.len() as u64 - b"startxref\n0000000000\n%%EOF\n".len() as u64;
    let mut file = File::create(path).unwrap();
    file.write_all(&original[..xref]).unwrap();
    file.seek(SeekFrom::Start(offset - 1)).unwrap();
    file.write_all(b"\n").unwrap();
    file.write_all(table).unwrap();
    write!(file, "startxref\n{offset:010}\n%%EOF\n").unwrap();
    assert_eq!(file.metadata().unwrap().len(), bytes);
}

#[test]
fn size_budget_evicts_unparsed_documents_and_session_drop_closes_remaining() {
    let reader = reader();
    let mut worker = reader.session();
    let temp = tempfile::tempdir().unwrap();
    let limit = (MAX_CACHED_SOURCE_BYTES / MAX_CACHED_FILE_BYTES) as usize;
    let mut first = None;
    let mut latest = None;
    for index in 0..=limit {
        let path = temp.path().join(format!("{index}.pdf"));
        padded_pdf(&path, MAX_CACHED_FILE_BYTES);
        assert_eq!(
            worker.classify_request(&classify(&path, 1)).status,
            ClassifyStatus::TextInParseWindow
        );
        let entries = worker.pending.as_ref().unwrap();
        assert!(
            entries.iter().map(|entry| entry.stamp.bytes).sum::<u64>() <= MAX_CACHED_SOURCE_BYTES
        );
        latest = Some(Arc::downgrade(&entries.back().unwrap().snapshot.0));
        if index == 0 {
            first = latest.clone();
        }
    }
    assert!(first.unwrap().upgrade().is_none());
    assert_eq!(worker.pending.as_ref().unwrap().len(), limit);
    let remaining = latest.unwrap();
    assert!(remaining.upgrade().is_some());
    drop(worker);
    assert!(remaining.upgrade().is_none());
}

#[test]
fn large_pdf_classifies_and_parses_live_without_retaining_it() {
    let reader = reader();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("large.pdf");
    padded_pdf(&path, MAX_CACHED_FILE_BYTES + 1);
    let classification = classify(&path, 1);
    let mut worker = reader.session();
    let result = worker.classify_request(&classification);
    assert_eq!(result.status, ClassifyStatus::TextInParseWindow);
    assert_eq!(result, reader.classify_request(&classification));
    assert!(worker.pending.as_ref().unwrap().is_empty());
    let request = parse(&classification, vec![1]);
    assert!(!parsed(&mut worker, &request).content.is_empty());
    assert_eq!(
        worker.work,
        WorkCounts {
            document_opens: 2,
            page_reads: 2
        }
    );
}

#[test]
fn snapshot_requires_exact_length_and_bytes() {
    let mut source = Cursor::new(b"abc");
    let snapshot = PdfSnapshot::read(&mut source, 3).unwrap();
    for (actual, matches) in [
        (b"abc".as_slice(), true),
        (b"abd", false),
        (b"ab", false),
        (b"abcd", false),
    ] {
        assert_eq!(snapshot.matches(&mut Cursor::new(actual)), matches);
    }
    assert!(PdfSnapshot::read(&mut Cursor::new(b"ab"), 3).is_none());
    assert!(PdfSnapshot::read(&mut Cursor::new(b"abcd"), 3).is_none());
}
