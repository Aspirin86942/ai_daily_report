#![cfg(windows)]

use daily_parsers::{
    contract::{ClassifyStatus, ParseRequest, ParserBackend, ParserLimits},
    discovery::build_source_version,
    pdf_wire::{
        file_identity_sha256, pinned_pdfium_sha256, read_frame, write_frame, InspectAndExtract,
        PdfCommand, PdfNegativeKind, PdfReply, PdfRequestFrame, PdfResponse, PdfResponseFrame,
        PdfSourceGuard, MAX_REQUEST_FRAME, PDF_PROTOCOL_VERSION,
    },
    PARSER_CONTENT_IDENTITY,
};
use std::{
    fs::{self, File},
    io::Write,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::UNIX_EPOCH,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn worker_program() -> PathBuf {
    std::env::var_os("AIDR_PDF_WORKER_TEST_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_ai-daily-pdf-worker").into())
}

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    next_id: u64,
}

impl Worker {
    fn spawn(program: &Path) -> Self {
        let mut child = Command::new(program)
            .creation_flags(CREATE_NO_WINDOW)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take(),
            output: child.stdout.take().unwrap(),
            child,
            next_id: 0,
        }
    }

    fn exchange(&mut self, command: PdfCommand) -> PdfReply {
        let response_limit = command.response_frame_limit();
        let id = self.next_id;
        self.next_id += 1;
        let request = PdfRequestFrame {
            version: PDF_PROTOCOL_VERSION,
            id,
            command,
        };
        write_frame(self.input.as_mut().unwrap(), &request, MAX_REQUEST_FRAME).unwrap();
        let response: PdfResponseFrame = read_frame(&mut self.output, response_limit)
            .unwrap()
            .expect("worker closed its pipe");
        assert_eq!((response.version, response.id), (PDF_PROTOCOL_VERSION, id));
        response.reply
    }

    fn open() -> Self {
        let mut worker = Self::spawn(&worker_program());
        let PdfReply::Hello(hello) = worker.exchange(PdfCommand::Hello) else {
            panic!("worker could not load the pinned DLL beside the debug executable");
        };
        assert_eq!(hello.pid, worker.child.id());
        assert_eq!(hello.parser_identity, PARSER_CONTENT_IDENTITY);
        assert_eq!(hello.pdfium_sha256, pinned_pdfium_sha256());
        assert_eq!(hello.ocr, cfg!(feature = "ocr"));
        worker
    }

    fn read(&mut self, request: InspectAndExtract) -> PdfResponse {
        let PdfReply::InspectAndExtract(response) =
            self.exchange(PdfCommand::InspectAndExtract(Box::new(request.clone())))
        else {
            panic!("expected a business reply");
        };
        response.validate_for(&request).unwrap();
        *response
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn request(path: &Path, window: u64, chars: u64) -> InspectAndExtract {
    let file = File::open(path).unwrap();
    let metadata = file.metadata().unwrap();
    let mtime = metadata
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    let mut request = InspectAndExtract::new(
        ParseRequest {
            file_path: path.to_string_lossy().into_owned(),
            file_type: ".pdf".to_owned(),
            backend: ParserBackend::RustPdfiumV3,
            remaining_timeout_ms: 30_000,
            max_file_size_bytes: 40 * 1024 * 1024,
            parser_limits: ParserLimits::Pdf {
                max_pages: window,
                excerpt_max_chars: chars,
                selected_pages: None,
            },
            expected_source_version: build_source_version(mtime.as_nanos(), metadata.len()),
        },
        window,
        "pdf_text_presence_v2".to_owned(),
        true,
    );
    request.expected_source_guard =
        file_identity_sha256(&file).map(|(kind, sha256)| PdfSourceGuard { kind, sha256 });
    request
}

#[test]
fn one_business_exchange_and_one_native_open_for_each_of_15_pdfs() {
    let corpus = classifier_corpus();
    let mut worker = Worker::open();
    let names = (1..=12)
        .map(|i| format!("text_plain_{i:02}.pdf"))
        .chain((1..=3).map(|i| format!("text_cjk_{i:02}.pdf")));
    for name in names {
        let response = worker.read(request(&corpus.join(&name), 1, 3200));
        assert_eq!(
            response.classification.status,
            ClassifyStatus::TextInParseWindow,
            "{name}"
        );
        assert_eq!(
            (response.native_document_opens, response.native_page_reads),
            (1, 1),
            "{name}"
        );
        assert!(
            !response.extraction.unwrap().unwrap().content.is_empty(),
            "{name}"
        );
    }
    assert_eq!(worker.next_id, 16);
}

fn classifier_corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/pdf_classifier")
}

#[test]
fn only_complete_no_text_and_proven_open_failures_are_cacheable() {
    use ClassifyStatus::{Error, NoTextInParseWindow, TextInParseWindow};

    let corpus = classifier_corpus();
    let mut worker = Worker::open();
    for (name, status, negative) in [
        ("text_mixed_01", TextInParseWindow, None),
        ("text_image_mixed_01", TextInParseWindow, None),
        ("text_ocr_hidden_01", TextInParseWindow, None),
        ("text_sparse_01", TextInParseWindow, None),
        ("no_text_beyond_max_pages_01", TextInParseWindow, None),
        (
            "no_text_blank_01",
            NoTextInParseWindow,
            Some(PdfNegativeKind::NoText),
        ),
        (
            "no_text_image_01",
            NoTextInParseWindow,
            Some(PdfNegativeKind::NoText),
        ),
        (
            "no_text_image_41",
            NoTextInParseWindow,
            Some(PdfNegativeKind::NoText),
        ),
        (
            "error_encrypted_01",
            Error,
            Some(PdfNegativeKind::Protected),
        ),
        (
            "error_encrypted_02",
            Error,
            Some(PdfNegativeKind::Protected),
        ),
        ("error_corrupt_01", Error, Some(PdfNegativeKind::InvalidPdf)),
        ("error_corrupt_02", Error, Some(PdfNegativeKind::InvalidPdf)),
    ] {
        let path = corpus.join(format!("{name}.pdf"));
        let response = worker.read(request(&path, 5, 3200));
        assert_eq!(
            response.classification.status, status,
            "{name}: {response:?}"
        );
        assert_eq!(response.negative_kind(), negative, "{name}");
        assert_eq!(response.native_document_opens, 1, "{name}");
        if status == TextInParseWindow {
            assert!(
                !response.extraction.unwrap().unwrap().content.is_empty(),
                "{name}"
            );
        }
    }
}

#[test]
fn thirty_mib_pdf_reuses_the_text_page_and_keeps_the_tail_and_ranges() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("long.pdf");
    let page2 = "BT /F1 11 Tf 35 700 Td (Second page: accepted quantity 1936 units) Tj ET";
    let page3 = "BT /F1 11 Tf 35 700 Td (Last page: final settlement USD 4817.25) Tj ET";
    write_pdf(&path, &["", page2, page3], 30 * 1024 * 1024, 0);
    assert!(path.metadata().unwrap().len() > 30 * 1024 * 1024);

    let mut worker = Worker::open();
    let requested = request(&path, 2, 200);
    let response = worker.read(requested.clone());
    assert_eq!(response.native_document_opens, 1);
    assert_eq!(response.native_page_reads, 3);
    let audit = response.classification.page_audit.unwrap();
    assert_eq!(audit.checked_pages, [1, 2]);
    assert_eq!(
        audit
            .observations
            .unwrap()
            .into_iter()
            .map(|page| page.has_text)
            .collect::<Vec<_>>(),
        [false, true]
    );
    let parsed = response.extraction.unwrap().unwrap();
    assert!(parsed.truncated);
    assert!(parsed.content.contains("1936 units") && parsed.content.contains("USD 4817.25"));
    assert_eq!(
        parsed
            .evidence_audit
            .unwrap()
            .content_units
            .unwrap()
            .iter()
            .map(|unit| (unit.ranges[0].start, unit.ranges[0].end))
            .collect::<Vec<_>>(),
        [(2, 3), (3, 4)]
    );

    let mut inspect_only = requested.clone();
    inspect_only.extract = false;
    let response = worker.read(inspect_only);
    assert_eq!(
        response.classification.status,
        ClassifyStatus::TextInParseWindow
    );
    assert_eq!(response.native_page_reads, 2);
    assert!(response.extraction.is_none());

    let mut no_text = requested.clone();
    no_text.inspect_max_pages = 1;
    let response = worker.read(no_text);
    assert_eq!(response.negative_kind(), Some(PdfNegativeKind::NoText));
    assert_eq!(response.classification.page_count, Some(3));
    assert_eq!(response.classification.result_examined_pages, Some(1));
    assert!(response.validate_for(&requested).is_err());

    let mut missing = requested;
    missing.parse.file_path = tmp
        .path()
        .join("missing.pdf")
        .to_string_lossy()
        .into_owned();
    let response = worker.read(missing);
    assert_eq!(response.classification.status, ClassifyStatus::Unknown);
    assert_eq!(response.native_document_opens, 0);
    assert!(response.negative_kind().is_none());
}

#[test]
fn explicit_file_size_page_selection_and_character_limits_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limits.pdf");
    write_pdf(
        &path,
        &[
            "BT /F1 11 Tf 35 700 Td (Inspection text only) Tj ET",
            "BT /F1 11 Tf 35 700 Td (Unselected page evidence) Tj ET",
            "BT /F1 11 Tf 35 700 Td (Approved 1936 units) Tj 0 -30 Td (This longer line exceeds the explicit excerpt character cap) Tj ET",
        ],
        0,
        0,
    );
    let mut worker = Worker::open();
    let mut valid = request(&path, 1, 25);
    valid.parse.max_file_size_bytes = path.metadata().unwrap().len();
    valid.parse.parser_limits = ParserLimits::Pdf {
        max_pages: 1,
        excerpt_max_chars: 25,
        selected_pages: Some(vec![3]),
    };
    let reply = worker.read(valid.clone());
    assert_eq!(
        (reply.native_document_opens, reply.native_page_reads),
        (1, 2)
    );
    assert_eq!(reply.classification.page_audit.unwrap().checked_pages, [1]);
    let result = reply.extraction.unwrap().unwrap();
    assert_eq!(result.content, "Approved 1936 units");
    assert!(result.truncated);
    let ranges = result.evidence_audit.unwrap().read_ranges.unwrap();
    assert_eq!(
        ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect::<Vec<_>>(),
        [(3, 4)]
    );

    let mut invalid_page = valid.clone();
    let ParserLimits::Pdf { selected_pages, .. } = &mut invalid_page.parse.parser_limits else {
        unreachable!();
    };
    *selected_pages = Some(vec![4]);
    let reply = worker.read(invalid_page);
    assert_eq!(
        (reply.native_document_opens, reply.native_page_reads),
        (1, 1)
    );
    assert_eq!(
        reply.extraction.unwrap().unwrap_err().error_code,
        "PARSER_INVALID_PAYLOAD"
    );

    valid.parse.max_file_size_bytes -= 1;
    let reply = worker.read(valid);
    assert_eq!(
        (reply.native_document_opens, reply.native_page_reads),
        (0, 0)
    );
    assert_eq!(
        reply.classification.diagnostic.unwrap().error_code,
        "FILE_TOO_LARGE"
    );
}

#[test]
fn wrong_source_hash_and_changed_same_size_file_have_no_cacheable_result() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("replaced.pdf");
    let original = "BT /F1 11 Tf 35 700 Td (Source A) Tj ET";
    let replacement = "BT /F1 11 Tf 35 700 Td (Source B) Tj ET";
    write_pdf(&path, &[original], 0, 0);
    let before = request(&path, 1, 200);
    let last_write = path.metadata().unwrap().modified().unwrap();
    fs::rename(&path, tmp.path().join("source-a.pdf")).unwrap();
    write_pdf(&path, &[replacement], 0, 0);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(last_write)
        .unwrap();
    assert_eq!(
        request(&path, 1, 200).parse.expected_source_version,
        before.parse.expected_source_version
    );

    let mut worker = Worker::open();
    if before.expected_source_guard.is_some() {
        let response = worker.read(before);
        assert_eq!(response.classification.status, ClassifyStatus::Unknown);
        assert_eq!(
            response.classification.diagnostic.unwrap().error_code,
            "SOURCE_VERSION_CHANGED"
        );
        assert_eq!(response.native_document_opens, 0);
        assert!(response.extraction.is_none());
    }
    let mut bad = request(&path, 1, 200);
    bad.expected_source_guard = Some(PdfSourceGuard {
        kind: daily_parsers::settings::SourceGuardKind::ContentSha256V1,
        sha256: "0".repeat(64),
    });
    let response = worker.read(bad.clone());
    assert!(response.negative_kind().is_none());
    assert_eq!(response.native_document_opens, 0);

    bad.expected_source_guard.as_mut().unwrap().sha256 =
        daily_parsers::pdf_wire::reader_sha256(File::open(&path).unwrap()).unwrap();
    let response = worker.read(bad);
    assert_eq!(response.extraction.unwrap().unwrap().content, "Source B");
}

#[test]
fn physical_table_rows_keep_their_units_under_the_excerpt_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("table.pdf");
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
    let stream: String = cells
        .into_iter()
        .map(|(x, y, text)| format!("BT /F1 11 Tf {x} {y} Td ({text}) Tj ET\n"))
        .collect();
    write_pdf(&path, &[&stream], 0, 0);
    let mut worker = Worker::open();
    let response = worker.read(request(&path, 1, 200));
    assert_eq!(response.native_page_reads, 1);
    let parsed = response.extraction.unwrap().unwrap();
    assert!(!parsed.truncated);
    assert_eq!(
        parsed.content,
        "Item\tAmount\nFreight\tUSD 12,345.60\nPackaging\t18 units\nPending\nSpares\t5 kg"
    );
    let small = worker
        .read(request(&path, 1, 12))
        .extraction
        .unwrap()
        .unwrap();
    assert!(small.truncated && small.content.chars().count() <= 12);
    assert!(small
        .content
        .lines()
        .all(|line| parsed.content.lines().any(|whole| whole == line)));
}

#[test]
fn bad_dll_and_partial_frames_fail_without_loading_an_alternative_library() {
    let tmp = tempfile::tempdir().unwrap();
    let program = tmp.path().join("ai-daily-pdf-worker.exe");
    fs::copy(worker_program(), &program).unwrap();
    fs::write(tmp.path().join("pdfium.dll"), b"wrong DLL").unwrap();
    let mut worker = Worker::spawn(&program);
    let PdfReply::PdfiumUnavailable(error) = worker.exchange(PdfCommand::Hello) else {
        panic!("unpinned DLL was accepted");
    };
    assert!(error.contains("PDFIUM_VERSION_MISMATCH"));

    for partial in [vec![7, 0], vec![7, 0, 0, 0, b'{']] {
        let mut worker = Worker::spawn(&program);
        let mut stdin = worker.input.take().unwrap();
        stdin.write_all(&partial).unwrap();
        drop(stdin);
        assert!(!worker.child.wait().unwrap().success());
    }
}

#[test]
fn build_info_requires_no_pdfium_and_reports_the_feature_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let program = tmp.path().join("ai-daily-pdf-worker.exe");
    fs::copy(worker_program(), &program).unwrap();
    let output = Command::new(&program)
        .creation_flags(CREATE_NO_WINDOW)
        .arg("--build-info")
        .output()
        .unwrap();
    assert!(output.status.success() && output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({
            "parser_content_identity": PARSER_CONTENT_IDENTITY,
            "protocol_version": PDF_PROTOCOL_VERSION,
            "ocr_enabled": cfg!(feature = "ocr"),
        })
    );
}

#[test]
fn rotated_page_uses_the_visible_pdf_coordinates_for_inspection_and_extraction() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rotated.pdf");
    let text = "Rotation retains the signed quantity";
    let stream = format!("BT /F1 11 Tf 35 750 Td ({text}) Tj ET");
    write_pdf(&path, &[&stream], 0, 90);
    let response = Worker::open().read(request(&path, 1, 200));
    assert_eq!(
        response.classification.status,
        ClassifyStatus::TextInParseWindow
    );
    assert_eq!(response.extraction.unwrap().unwrap().content, text);
    assert_eq!(response.native_page_reads, 1);
}

fn image_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ocr-image-pages.pdf")
}

#[test]
fn image_only_pages_are_observed_without_extracting_text() {
    let response = Worker::open().read(request(&image_fixture(), 2, 256));
    assert_eq!(response.negative_kind(), Some(PdfNegativeKind::NoText));
    assert_eq!(response.native_page_reads, 2);
    let audit = response.classification.page_audit.unwrap();
    assert!(audit.all_pages_are_images(2));
}

#[test]
fn ocr_requires_proof_for_every_page_before_admitting_a_selected_page() {
    let mut input = request(&image_fixture(), 1, 256);
    input.parse.parser_limits = ParserLimits::PdfImageOcr {
        max_pages: 1,
        excerpt_max_chars: 256,
        selected_pages: vec![1],
    };
    let reply = Worker::open().read(input);
    assert_eq!(
        (reply.native_document_opens, reply.native_page_reads),
        (1, 1)
    );
    assert_eq!(
        reply.extraction.unwrap().unwrap_err().message,
        "PDF_OCR_PAGE_NOT_IMAGE_ONLY"
    );
}

#[cfg(feature = "ocr")]
#[test]
fn optional_ocr_reuses_the_document_and_labels_source_ranges() {
    use daily_parsers::contract::OCR_RANGE_LABEL;

    let mut request = request(&image_fixture(), 2, 256);
    request.parse.parser_limits = ParserLimits::PdfImageOcr {
        max_pages: 1,
        excerpt_max_chars: 256,
        selected_pages: vec![2],
    };
    let response = Worker::open().read(request);
    assert_eq!(
        response.classification.status,
        ClassifyStatus::NoTextInParseWindow
    );
    assert_eq!(response.native_document_opens, 1);
    assert_eq!(response.native_page_reads, 2);
    assert!(response.negative_kind().is_none());
    let extracted = response.extraction.unwrap();
    let models = worker_program().parent().unwrap().join("ocr-models");
    if ["det.mnn", "rec.mnn", "dict.txt"]
        .iter()
        .any(|name| !models.join(name).is_file())
    {
        assert_eq!(extracted.unwrap_err().message, "PDF_OCR_MODEL_MISSING");
        return;
    }
    let parsed = extracted.unwrap();
    assert!(parsed.content.chars().count() <= 256);
    let text = parsed
        .content
        .split_whitespace()
        .collect::<String>()
        .to_uppercase();
    for fact in ["验收", "1936", "YEAR", "2064"] {
        assert!(text.contains(fact), "{fact}");
    }
    let audit = parsed.evidence_audit.unwrap();
    let ranges = audit.read_ranges.unwrap();
    assert_eq!(ranges.len(), 1);
    assert_eq!((ranges[0].start, ranges[0].end), (2, 3));
    assert_eq!(ranges[0].label.as_deref(), Some(OCR_RANGE_LABEL));
}

fn write_pdf(path: &Path, pages: &[&str], padding: usize, rotation: u16) {
    let file = File::create(path).unwrap();
    let mut out = std::io::BufWriter::new(file);
    let children = (0..pages.len())
        .map(|n| format!("{} 0 R", 4 + 2 * n))
        .collect::<Vec<_>>();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        format!(
            "<< /Type /Pages /Count {} /Kids [{}] >>",
            pages.len(),
            children.join(" ")
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    for (n, stream) in pages.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Rotate {rotation} /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * n
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            stream.len(),
            stream
        ));
    }
    let mut offset = b"%PDF-1.7\n".len();
    out.write_all(b"%PDF-1.7\n").unwrap();
    let mut offsets = vec![0];
    for (n, object) in objects.iter().enumerate() {
        offsets.push(offset);
        let bytes = format!("{} 0 obj\n{object}\nendobj\n", n + 1);
        out.write_all(bytes.as_bytes()).unwrap();
        offset += bytes.len();
    }
    if padding > 0 {
        offsets.push(offset);
        let header = format!(
            "{} 0 obj\n<< /Length {padding} >>\nstream\n",
            offsets.len() - 1
        );
        let footer = b"\nendstream\nendobj\n";
        out.write_all(header.as_bytes()).unwrap();
        let block = [b' '; 64 * 1024];
        for start in (0..padding).step_by(block.len()) {
            out.write_all(&block[..(padding - start).min(block.len())])
                .unwrap();
        }
        out.write_all(footer).unwrap();
        offset += header.len() + padding + footer.len();
    }
    writeln!(out, "xref\n0 {}\n0000000000 65535 f ", offsets.len()).unwrap();
    for start in &offsets[1..] {
        writeln!(out, "{start:010} 00000 n ").unwrap();
    }
    writeln!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{offset}\n%%EOF",
        offsets.len()
    )
    .unwrap();
    out.flush().unwrap();
}
