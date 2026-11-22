use super::worker_contract::{
    fixture_path, request_for_route, rich_fixture, source_version, write_minimal_xlsx,
};
use daily_parsers::contract::{ParseResult, ParserBackend};
use daily_parsers::office;
use office_oxide::cfb::CfbReader;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

#[path = "../../src/office/compound.rs"]
mod compound;

fn parse(path: &Path, extension: &str, backend: ParserBackend) -> ParseResult {
    let request = request_for_route(
        path,
        extension,
        backend,
        source_version(path),
        64 * 1024 * 1024,
    );
    let result =
        office::parse(&request).unwrap_or_else(|error| panic!("{}: {error:?}", path.display()));
    result.validate().unwrap();
    result
}

fn projection(key: &str, parsed: &ParseResult) {
    let stable = serde_json::to_string(&(
        parsed.parser_backend,
        parsed.truncated,
        &parsed.content,
        &parsed.evidence_audit,
    ))
    .unwrap();
    println!("projection {key} {stable}");
}

#[test]
fn legacy_containers_larger_than_eight_mib_keep_their_original_evidence() {
    let dir = tempfile::tempdir().unwrap();
    for (file, extension, backend, names) in [
        (
            "legacy_sample.xls",
            ".xls",
            ParserBackend::RustXlsCalamineV3,
            &["Workbook", "Book"][..],
        ),
        (
            "legacy_sample.doc",
            ".doc",
            ParserBackend::RustLegacyOfficeV3,
            &["WordDocument", "0Table", "1Table", "Data"][..],
        ),
        (
            "legacy_sample.ppt",
            ".ppt",
            ParserBackend::RustLegacyOfficeV3,
            &["PowerPoint Document", "PP97_DUALSTORAGE", "Pictures"][..],
        ),
    ] {
        let original = fixture_path(file);
        let small = parse(&original, extension, backend);
        let mut container = CfbReader::new(File::open(original).unwrap()).unwrap();
        let mut streams = vec![("A", vec![0x5a; 9 * 1024 * 1024]), ("Small", vec![1])];
        streams.extend(names.iter().filter_map(|&name| {
            container
                .open_stream(name)
                .ok()
                .map(|contents| (name, contents))
        }));
        let expanded = dir.path().join(file);
        let bytes = compound::pack(streams).unwrap();
        assert!(bytes.len() > 8 * 1024 * 1024);
        fs::write(&expanded, bytes).unwrap();
        let expanded = parse(&expanded, extension, backend);
        assert!(small.evidence_audit.as_ref().unwrap().usable_chars.unwrap() > 0);
        assert_eq!(expanded.content, small.content, "{file}");
        assert_eq!(expanded.evidence_audit, small.evidence_audit, "{file}");
        assert_eq!(expanded.truncated, small.truncated, "{file}");
        projection(&format!("large_{file}"), &expanded);
    }
}

fn with_ooxml_parts(
    original: &Path,
    output: &Path,
    mut edit: impl FnMut(&str, &str) -> Option<String>,
    media: Option<(&str, CompressionMethod)>,
) {
    let mut source = ZipArchive::new(File::open(original).unwrap()).unwrap();
    let mut writer = ZipWriter::new(File::create(output).unwrap());
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for index in 0..source.len() {
        let mut member = source.by_index(index).unwrap();
        if !member.name().ends_with(".xml") && !member.name().ends_with(".rels") {
            writer.raw_copy_file(member).unwrap();
            continue;
        }
        let part = member.name().to_owned();
        let mut xml = String::new();
        member.read_to_string(&mut xml).unwrap();
        writer.start_file(part.clone(), options).unwrap();
        writer
            .write_all(edit(&part, &xml).as_deref().unwrap_or(&xml).as_bytes())
            .unwrap();
    }
    if let Some((media, method)) = media {
        for number in 1..=3 {
            writer
                .start_file(
                    format!("{media}/large{number}.bmp"),
                    options.compression_method(method),
                )
                .unwrap();
            write_bmp(&mut writer);
        }
    }
    writer.finish().unwrap();
}

fn write_bmp(writer: &mut impl Write) {
    // Uncompressed 24-bit BMP, with scan lines already aligned at 2048 pixels.
    const SIDE: u32 = 2048;
    const BYTES: u32 = SIDE * SIDE * 3;
    let mut header = [0; 54];
    header[..2].copy_from_slice(b"BM");
    for (offset, value) in [
        (2, BYTES + 54),
        (10, 54),
        (14, 40),
        (18, SIDE),
        (22, SIDE),
        (34, BYTES),
    ] {
        header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    header[26] = 1;
    header[28] = 24;
    writer.write_all(&header).unwrap();
    io::copy(&mut io::repeat(0x74).take(u64::from(BYTES)), writer).unwrap();
}

#[test]
fn legacy_content_streams_keep_evidence_past_eight_mib() {
    const EIGHT_MIB: usize = 8 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let mut doc = CfbReader::new(File::open(fixture_path("legacy_sample.doc")).unwrap()).unwrap();
    let mut word = doc.open_stream("WordDocument").unwrap();
    let mut table = doc.open_stream("1Table").unwrap();
    let start = u32::from_le_bytes(word[24..28].try_into().unwrap()) as usize;
    let end = u32::from_le_bytes(word[28..32].try_into().unwrap()) as usize;
    let text = word[start..end].to_vec();
    let late = EIGHT_MIB as u32 + 512;
    word.resize(late as usize, 0);
    word.extend_from_slice(&text);
    word[24..28].copy_from_slice(&late.to_le_bytes());
    word[28..32].copy_from_slice(&(late + text.len() as u32).to_le_bytes());
    // The fixture has one compressed Word piece. Keep its original text and
    // update the piece's file offset so reading the first 8 MiB loses it.
    let clx = u32::from_le_bytes(word[0x1a2..0x1a6].try_into().unwrap()) as usize;
    table[clx + 15..clx + 19].copy_from_slice(&((late * 2) | 0x4000_0000).to_le_bytes());

    let mut slide = Vec::new();
    ppt_record(&mut slide, 0, 0x03f3, &[0; 20]); // SlidePersistAtom
    ppt_record(&mut slide, 0, 0x0f9f, &1u32.to_le_bytes()); // TextHeaderAtom
    for _ in 0..1025 {
        ppt_record(&mut slide, 0, 0x0fa8, &[b'x'; 8192]); // TextBytesAtom
    }
    ppt_record(&mut slide, 0, 0x0fa8, b"Late slide evidence");
    let mut list = Vec::new();
    ppt_record(&mut list, 15, 0x0ff0, &slide); // SlideListWithText
    let mut presentation = Vec::new();
    ppt_record(&mut presentation, 15, 0x03e8, &list); // Document

    for (extension, backend, streams, evidence) in [
        (
            ".doc",
            ParserBackend::RustLegacyOfficeV3,
            vec![("WordDocument", word), ("1Table", table)],
            "Legacy DOC worker content",
        ),
        (
            ".ppt",
            ParserBackend::RustLegacyOfficeV3,
            vec![("PowerPoint Document", presentation)],
            "Late slide evidence",
        ),
        (
            ".xls",
            ParserBackend::RustXlsCalamineV3,
            vec![("Workbook", large_biff8(0)), ("Contents", vec![0])],
            "Late workbook evidence",
        ),
    ] {
        let path = dir.path().join(format!("content-stream{extension}"));
        let stream_bytes = streams[0].1.len();
        assert!(stream_bytes > EIGHT_MIB);
        fs::write(&path, compound::pack(streams).unwrap()).unwrap();
        let parsed = parse(&path, extension, backend);
        assert!(parsed.content.contains(evidence), "{}", parsed.content);
        println!("{extension} content stream: {stream_bytes} bytes");
        projection(path.file_name().unwrap().to_str().unwrap(), &parsed);
    }
}

fn ppt_record(out: &mut Vec<u8>, version: u16, kind: u16, data: &[u8]) {
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

fn large_biff8(repeated_long_cells: u16) -> Vec<u8> {
    fn record(out: &mut Vec<u8>, kind: u16, data: &[u8]) {
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(data);
    }
    fn sst_string(out: &mut Vec<u8>, text: &[u8]) {
        out.extend_from_slice(&(text.len() as u16).to_le_bytes());
        out.push(0); // Compressed Unicode.
        out.extend_from_slice(text);
    }
    let globals_bof = [0, 6, 5, 0, 0x5a, 0x4f, 0xcd, 7, 0xc1, 0, 2, 0, 6, 8, 0, 0];
    let mut workbook = Vec::new();
    record(&mut workbook, 0x0809, &globals_bof);
    record(&mut workbook, 0x0042, &1200u16.to_le_bytes());
    let sheet_offset = workbook.len() + 4;
    record(&mut workbook, 0x0085, b"\0\0\0\0\0\0\x04\0Data");
    const STRINGS: u16 = 1100;
    let uses = STRINGS + 81 + repeated_long_cells;
    let mut sst = u32::from(uses).to_le_bytes().to_vec();
    sst.extend_from_slice(&(u32::from(STRINGS) + 1).to_le_bytes());
    // Each string below spans two records with an explicit encoding flag,
    // plus the next string header in the continuation.
    let mut text = vec![b'q'; 7900];
    sst_string(&mut sst, &text);
    record(&mut workbook, 0x00fc, &sst[..sst.len() - 40]);
    let mut continuation = sst[sst.len() - 40..].to_vec();
    continuation.insert(0, 0);
    for row in 1..STRINGS {
        text[..4].copy_from_slice(format!("{row:04}").as_bytes());
        sst_string(&mut continuation, &text);
        let end = continuation.len() - 40;
        record(&mut workbook, 0x003c, &continuation[..end]);
        continuation.drain(1..end);
    }
    sst_string(&mut continuation, b"Late workbook evidence");
    record(&mut workbook, 0x003c, &continuation);
    record(&mut workbook, 0x000a, &[]);
    let sheet_start = workbook.len() as u32;
    workbook[sheet_offset..sheet_offset + 4].copy_from_slice(&sheet_start.to_le_bytes());
    let mut worksheet_bof = globals_bof;
    worksheet_bof[2] = 0x10;
    record(&mut workbook, 0x0809, &worksheet_bof);
    for row in 0..uses {
        let mut cell = [0; 10];
        cell[..2].copy_from_slice(&row.to_le_bytes());
        let index = match row {
            0..=80 => STRINGS,
            row if row < STRINGS + 81 => row - 81,
            _ => 0,
        };
        cell[6..].copy_from_slice(&u32::from(index).to_le_bytes());
        record(&mut workbook, 0x00fd, &cell);
    }
    record(&mut workbook, 0x000a, &[]);
    workbook
}

#[test]
fn repeated_large_xls_cells_exhaust_the_string_copy_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large-cell-copies.xls");
    let file = compound::pack(vec![("Workbook", large_biff8(600)), ("Contents", vec![0])]).unwrap();
    fs::write(&path, file).unwrap();
    let request = request_for_route(
        &path,
        ".xls",
        ParserBackend::RustXlsCalamineV3,
        source_version(&path),
        64 * 1024 * 1024,
    );
    let error = office::parse(&request).unwrap_err();
    assert_eq!(error.error_code, "PARSER_FAILED");
    assert!(!error.retryable);
}

#[test]
fn docx_and_pptx_skip_media_payloads_and_keep_image_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    for (name, path, extension, body, rels, media, target, close, drawing) in [
        (
            "media.docx",
            fixture_path("modern_sample.docx"),
            ".docx",
            "word/document.xml",
            "word/_rels/document.xml.rels",
            "word/media",
            "media/large1.bmp",
            "<w:sectPr",
            r#"<w:p><w:r><w:drawing><wp:inline><wp:extent cx="914400" cy="914400"/><wp:docPr id="900" name="Example" descr="Source picture description"/><a:graphic xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:nvPicPr><pic:cNvPr id="0" name="Example"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="rIdMedia"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"#,
        ),
        (
            "media.pptx",
            rich_fixture("delivery.pptx"),
            ".pptx",
            "ppt/slides/slide1.xml",
            "ppt/slides/_rels/slide1.xml.rels",
            "ppt/media",
            "../media/large1.bmp",
            "</p:spTree>",
            r#"<p:pic><p:nvPicPr><p:cNvPr id="900" name="Example" descr="Source picture description"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rIdMedia"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>"#,
        ),
    ] {
        let output = dir.path().join(name);
        with_ooxml_parts(
            &path,
            &output,
            |part, xml| {
                if part == "[Content_Types].xml" {
                    return Some(xml.replace(
                        "</Types>",
                        r#"<Default Extension="bmp" ContentType="image/bmp"/></Types>"#,
                    ));
                }
                if part == rels {
                    let link = format!(
                        r#"<Relationship Id="rIdMedia" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="{target}"/>"#
                    );
                    return Some(
                        xml.replace("</Relationships>", &format!("{link}</Relationships>")),
                    );
                }
                (part == body).then(|| xml.replacen(close, &format!("{drawing}{close}"), 1))
            },
            Some((
                media,
                if extension == ".docx" {
                    CompressionMethod::Stored
                } else {
                    CompressionMethod::Deflated
                },
            )),
        );
        let archive = ZipArchive::new(File::open(&output).unwrap()).unwrap();
        assert!(archive.decompressed_size().unwrap() > 32 * 1024 * 1024);
        if extension == ".docx" {
            assert!(fs::metadata(&output).unwrap().len() > 32 * 1024 * 1024);
        }
        let result = parse(&output, extension, ParserBackend::RustOfficeOxideV2);
        assert!(
            result.content.contains("Source picture description"),
            "{}",
            result.content
        );
        projection(name, &result);
    }
}

#[test]
fn legitimate_long_xml_and_nested_content_keep_later_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let word = dir.path().join("long.docx");
    with_ooxml_parts(
        &fixture_path("modern_sample.docx"),
        &word,
        |name, _| {
            (name == "word/document.xml").then(|| {
            let paragraph = "<w:p><w:r><w:t>";
            let close = "</w:t></w:r></w:p>";
            let cell_text = format!("{paragraph}{}{close}", "&amp;".repeat(30_000));
            let row = format!("<w:tr>{}</w:tr>", format!("<w:tc>{cell_text}</w:tc>").repeat(4));
            let nested = format!("{}{paragraph}Nested{close}{}", "<w:sdt><w:sdtContent>".repeat(25), "</w:sdtContent></w:sdt>".repeat(25));
            format!(r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{cell_text}<w:tbl><w:tblPr/><w:tblGrid><w:gridCol/><w:gridCol/><w:gridCol/><w:gridCol/></w:tblGrid>{row}<w:tr><w:tc>{paragraph}Later table evidence{close}</w:tc></w:tr></w:tbl>{nested}{paragraph}Later Word evidence{close}<w:sectPr/></w:body></w:document>"#)
        })
        },
        None,
    );
    let result = parse(&word, ".docx", ParserBackend::RustOfficeOxideV2);
    assert!(result.content.contains("Later table evidence"));
    assert!(result.content.contains("Later Word evidence"));
    projection("long.docx", &result);

    let sheet = dir.path().join("long.xlsx");
    let source = dir.path().join("single-sheet.xlsx");
    write_minimal_xlsx(&source);
    with_ooxml_parts(
        &source,
        &sheet,
        |name, _| {
            (name == "xl/worksheets/sheet1.xml").then(|| format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c t="str"><v>Field</v></c></row><row r="2"><c t="inlineStr"><is><t>{}</t></is></c></row><row r="3"><c t="str"><v>Later Excel evidence</v></c></row></sheetData></worksheet>"#,
            "&amp;".repeat(30_000),
        ))
        },
        None,
    );
    let result = parse(&sheet, ".xlsx", ParserBackend::RustXlsxBoundedV2);
    assert!(
        result.content.contains("Later Excel evidence"),
        "{}",
        result.content
    );
    projection("long.xlsx", &result);
}

#[test]
fn committed_office_fixtures_keep_their_content_and_source_ranges() {
    let sources: [(PathBuf, _, _); 10] = [
        (
            fixture_path("legacy_sample.xls"),
            ".xls",
            ParserBackend::RustXlsCalamineV3,
        ),
        (
            fixture_path("legacy_unicode_formula.xls"),
            ".xls",
            ParserBackend::RustXlsCalamineV3,
        ),
        (
            fixture_path("legacy_sample.doc"),
            ".doc",
            ParserBackend::RustLegacyOfficeV3,
        ),
        (
            fixture_path("legacy_sample.ppt"),
            ".ppt",
            ParserBackend::RustLegacyOfficeV3,
        ),
        (
            fixture_path("modern_sample.docx"),
            ".docx",
            ParserBackend::RustOfficeOxideV2,
        ),
        (
            fixture_path("modern_sample.pptx"),
            ".pptx",
            ParserBackend::RustOfficeOxideV2,
        ),
        (
            rich_fixture("settlement.xlsx"),
            ".xlsx",
            ParserBackend::RustXlsxBoundedV2,
        ),
        (
            rich_fixture("settlement.xls"),
            ".xls",
            ParserBackend::RustXlsCalamineV3,
        ),
        (
            rich_fixture("scopes.docx"),
            ".docx",
            ParserBackend::RustOfficeOxideV2,
        ),
        (
            rich_fixture("delivery.pptx"),
            ".pptx",
            ParserBackend::RustOfficeOxideV2,
        ),
    ];
    for (path, extension, backend) in sources {
        let result = parse(&path, extension, backend);
        assert_eq!(result.parser_backend, backend);
        assert!(!result.content.is_empty());
        projection(path.file_name().unwrap().to_str().unwrap(), &result);
    }
}
