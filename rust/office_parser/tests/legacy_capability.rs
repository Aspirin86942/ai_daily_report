use calamine::{open_workbook, Reader, Xls};
use std::path::Path;

#[test]
fn calamine_reads_real_binary_xls() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/worker_documents/legacy_sample.xls");
    let mut book: Xls<_> = open_workbook(path).unwrap();
    let first = book.worksheet_range_at(0).unwrap().unwrap();
    assert!(first
        .rows()
        .flatten()
        .any(|cell| cell.to_string().contains("Legacy XLS worker content")));
}

#[test]
fn office_oxide_reads_real_binary_doc_and_ppt() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/worker_documents");
    for (name, expected) in [
        ("legacy_sample.doc", "Legacy DOC worker content"),
        ("legacy_sample.ppt", "Legacy PPT worker content"),
    ] {
        let bytes = std::fs::read(root.join(name)).unwrap();
        assert_eq!(
            &bytes[..8],
            &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]
        );
        let document = office_oxide::Document::open(root.join(name)).unwrap();
        assert!(document.plain_text().contains(expected), "{name}");
    }
}
