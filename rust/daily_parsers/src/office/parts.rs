use super::{check_time, ParseFailure, INVALID_OFFICE, OFFICE_CONTENT_MISSING_MESSAGE};
use quick_xml::events::{BytesRef, BytesStart, BytesText, Event};
use std::cell::Cell;
use std::io::{self, Read, Seek, SeekFrom};
use std::rc::Rc;
use std::time::{Duration, Instant};
use zip::read::ZipArchive;

const MAX_ENTRIES: u64 = 4096;
const MAX_DIRECTORY_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const MAX_XML_DEPTH: usize = 96;
const MAX_XML_TOKEN: usize = 1024 * 1024;
const MAX_CELL_BYTES: usize = 1024 * 1024;
const MAX_ROW_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct CheckedReader<R> {
    inner: R,
    deadline: Instant,
    remaining: Rc<Cell<usize>>,
}

impl<R> CheckedReader<R> {
    pub fn new(inner: R, deadline: Instant) -> Self {
        Self {
            inner,
            deadline,
            remaining: Rc::new(Cell::new(64 * 1024 * 1024)),
        }
    }

    pub fn read_budget(&self) -> Rc<Cell<usize>> {
        Rc::clone(&self.remaining)
    }
}

impl<R: Read> Read for CheckedReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        check_time(self.deadline)?;
        let remaining = self.remaining.get();
        if remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Office read limit",
            ));
        }
        let size = bytes.len().min(remaining);
        let read = self.inner.read(&mut bytes[..size])?;
        self.remaining.set(remaining - read);
        check_time(self.deadline)?;
        Ok(read)
    }
}

impl<R: Seek> Seek for CheckedReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        check_time(self.deadline)?;
        let remaining = self
            .remaining
            .get()
            .checked_sub(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Office seek limit"))?;
        self.remaining.set(remaining);
        let position = self.inner.seek(position)?;
        check_time(self.deadline)?;
        Ok(position)
    }
}

pub(super) fn open_package<R: Read + Seek>(
    mut reader: R,
    size: u64,
) -> Result<ZipArchive<PackageReader<R>>, String> {
    let (entries, directory_start, directory_bytes) =
        zip_directory_info(&mut reader, size).map_err(|e| e.to_string())?;
    reader
        .seek(SeekFrom::Start(directory_start))
        .map_err(|e| e.to_string())?;
    let mut directory = Vec::with_capacity(directory_bytes as usize + 22);
    directory.resize(directory_bytes as usize, 0);
    reader
        .read_exact(&mut directory)
        .map_err(|e| e.to_string())?;
    // The ZIP reader retries alternate end records when a directory is broken.
    // Give it only this bounded directory and one verified count.
    if directory
        .windows(4)
        .any(|v| matches!(v, b"PK\x05\x06" | b"PK\x06\x06" | b"PK\x06\x07"))
    {
        return Err("ambiguous ZIP directory".into());
    }
    directory.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    directory.extend_from_slice(&(entries as u16).to_le_bytes());
    directory.extend_from_slice(&(entries as u16).to_le_bytes());
    directory.extend_from_slice(&(directory_bytes as u32).to_le_bytes());
    let start = u32::try_from(directory_start).map_err(|_| "ZIP directory offset limit")?;
    directory.extend_from_slice(&start.to_le_bytes());
    directory.extend_from_slice(&[0, 0]);
    let read_members = Rc::new(Cell::new(false));
    let archive = ZipArchive::new(PackageReader {
        source: reader,
        position: 0,
        directory_start,
        directory,
        read_members: Rc::clone(&read_members),
    })
    .map_err(|e| e.to_string())?;
    if archive.len() as u64 != entries || archive.offset() != 0 {
        return Err("inconsistent ZIP directory".into());
    }
    read_members.set(true);
    Ok(archive)
}

pub(super) struct PackageReader<R> {
    source: R,
    position: u64,
    directory_start: u64,
    directory: Vec<u8>,
    read_members: Rc<Cell<bool>>,
}

impl<R: Read + Seek> Read for PackageReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let read = if self.position >= self.directory_start {
            let offset = self.position - self.directory_start;
            self.directory
                .get(offset as usize..)
                .unwrap_or_default()
                .read(bytes)?
        } else {
            let distance = self.directory_start - self.position;
            let chunk = bytes.len().min(distance as usize);
            if self.read_members.get() {
                self.source.read(&mut bytes[..chunk])?
            } else {
                if distance.saturating_sub(chunk as u64) > 65_557 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ZIP directory search limit",
                    ));
                }
                bytes[..chunk].fill(0);
                chunk
            }
        };
        self.position += read as u64;
        Ok(read)
    }
}

impl<R: Read + Seek> Seek for PackageReader<R> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let size = self.directory_start + self.directory.len() as u64;
        let position = match to {
            SeekFrom::Start(to) => Some(to),
            SeekFrom::End(to) => size.checked_add_signed(to),
            SeekFrom::Current(to) => self.position.checked_add_signed(to),
        };
        let position = position.ok_or(io::ErrorKind::InvalidInput)?;
        if self.read_members.get() && position < self.directory_start && position != self.position {
            self.source.seek(SeekFrom::Start(position))?;
        }
        self.position = position;
        Ok(position)
    }
}

fn zip_directory_info(reader: &mut (impl Read + Seek), size: u64) -> io::Result<(u64, u64, u64)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid ZIP directory");
    let tail_len = size.min(22 + u64::from(u16::MAX));
    reader.seek(SeekFrom::Start(size - tail_len))?;
    let mut tail = vec![0; tail_len as usize];
    reader.read_exact(&mut tail)?;
    let offset = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            tail[i..].starts_with(b"PK\x05\x06")
                && tail.len() - i == 22 + u16_at(&tail[i..], 20) as usize
        })
        .ok_or_else(invalid)?;
    let end = &tail[offset..];
    let mut count = u16_at(end, 10) as u64;
    let mut directory_bytes = u64::from(u32_at(end, 12));
    let mut directory_start = u64::from(u32_at(end, 16));
    let mut directory_end = size - tail_len + offset as u64;
    if u32_at(end, 4) != 0 || u16_at(end, 8) != u16_at(end, 10) {
        return Err(invalid());
    }
    // ZIP64 may be used even by small packages, so honor its locator when present.
    if directory_end >= 20 {
        reader.seek(SeekFrom::Start(directory_end - 20))?;
        let mut locator = [0; 20];
        reader.read_exact(&mut locator)?;
        if locator.starts_with(b"PK\x06\x07") {
            let zip64 = u64_at(&locator, 8);
            if u32_at(&locator, 4) != 0
                || u32_at(&locator, 16) != 1
                || zip64
                    .checked_add(56)
                    .is_none_or(|end| end > directory_end - 20)
            {
                return Err(invalid());
            }
            reader.seek(SeekFrom::Start(zip64))?;
            let mut record = [0; 56];
            reader.read_exact(&mut record)?;
            if !record.starts_with(b"PK\x06\x06")
                || u64_at(&record, 4) < 44
                || u64_at(&record, 16) != 0
                || u64_at(&record, 24) != u64_at(&record, 32)
            {
                return Err(invalid());
            }
            count = u64_at(&record, 32);
            directory_bytes = u64_at(&record, 40);
            directory_start = u64_at(&record, 48);
            directory_end = zip64;
        }
    }
    if count > MAX_ENTRIES
        || directory_bytes > MAX_DIRECTORY_BYTES
        || count * 46 > directory_bytes
        || directory_start.checked_add(directory_bytes) != Some(directory_end)
    {
        return Err(invalid());
    }
    Ok((count, directory_start, directory_bytes))
}

fn u16_at(bytes: &[u8], start: usize) -> u16 {
    u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], start: usize) -> u32 {
    u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], start: usize) -> u64 {
    u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap())
}

pub(super) fn detect_format<R: Read + Seek>(
    zip: &ZipArchive<R>,
) -> Result<&'static str, ParseFailure> {
    let mut formats = [
        ("word/document.xml", ".docx"),
        ("ppt/presentation.xml", ".pptx"),
        ("xl/workbook.xml", ".xlsx"),
    ]
    .into_iter()
    .filter(|(entry, _)| {
        zip.file_names()
            .any(|name| name.eq_ignore_ascii_case(entry))
    });
    if let (Some((_, format)), None) = (formats.next(), formats.next()) {
        return Ok(format);
    }
    if zip.index_for_name("[Content_Types].xml").is_some()
        && zip.file_names().all(|name| {
            name == "[Content_Types].xml"
                || name.ends_with('/')
                || name.starts_with("_rels/")
                || name.starts_with("docProps/")
        })
    {
        return Err(ParseFailure {
            message: OFFICE_CONTENT_MISSING_MESSAGE,
            ..INVALID_OFFICE
        });
    }
    Err(INVALID_OFFICE)
}

// Daily inspection shares one decompressed-byte allowance across all inspected parts.
// A partial XML part may supply complete worksheet rows, never a partial cell or chart.
pub(super) struct PartBudget {
    pub remaining: usize,
    pub deadline: Instant,
}

impl PartBudget {
    pub fn new(deadline: Instant) -> Self {
        Self {
            remaining: 32 * 1024 * 1024,
            deadline,
        }
    }

    pub fn reserve_render_time(&mut self) {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        self.deadline -= (remaining / 5).min(Duration::from_millis(500));
    }

    pub fn main_part<R: Read + Seek>(
        &mut self,
        zip: &mut ZipArchive<R>,
    ) -> Option<office_oxide::core::opc::PartName> {
        use office_oxide::core::relationships::{rel_types, Relationships};
        let xml = self.complete(zip, "_rels/.rels", 1024 * 1024)?;
        let links = Relationships::parse(&xml).ok()?;
        let rel = links.first_by_type(rel_types::OFFICE_DOCUMENT)?;
        links.resolve_target_from_root(&rel.id).ok()
    }

    pub fn complete<R: Read + Seek>(
        &mut self,
        zip: &mut ZipArchive<R>,
        name: &str,
        limit: usize,
    ) -> Option<Vec<u8>> {
        let (xml, partial) = self.read(zip, name.trim_start_matches('/'), limit).ok()?;
        (!partial).then_some(xml)
    }

    pub fn read<R: Read + Seek>(
        &mut self,
        zip: &mut ZipArchive<R>,
        name: &str,
        limit: usize,
    ) -> Result<(Vec<u8>, bool), String> {
        let index = resolve_zip_member(zip, name)?;
        self.read_index(zip, index, limit)
    }

    pub(super) fn read_index<R: Read + Seek>(
        &mut self,
        zip: &mut ZipArchive<R>,
        index: usize,
        limit: usize,
    ) -> Result<(Vec<u8>, bool), String> {
        let mut member = zip.by_index(index).map_err(|e| e.to_string())?;
        let size = usize::try_from(member.size()).unwrap_or(usize::MAX);
        let ratio_limit = member
            .compressed_size()
            .saturating_mul(512)
            .saturating_add(1024 * 1024);
        let ratio_limit = usize::try_from(ratio_limit).unwrap_or(usize::MAX);
        let allowance = self.remaining.min(limit).min(size).min(ratio_limit);
        let mut bytes = Vec::new();
        let mut chunk = [0; 16 * 1024];
        while bytes.len() < allowance && Instant::now() < self.deadline {
            let len = chunk.len().min(allowance - bytes.len());
            let n = member.read(&mut chunk[..len]).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("ZIP entry ended before its declared size".into());
            }
            bytes.extend_from_slice(&chunk[..n]);
            self.remaining -= n;
        }
        if bytes.len() < size {
            return Ok((bytes, true));
        }
        // Check EOF and the checksum. A forged size must not supply usable data.
        if member.read(&mut chunk[..1]).map_err(|e| e.to_string())? != 0 {
            return Err("ZIP entry exceeds its declared size".into());
        }
        Ok((bytes, false))
    }
}

fn resolve_zip_member<R: Read + Seek>(
    archive: &ZipArchive<R>,
    requested: &str,
) -> Result<usize, String> {
    let mut matches = archive
        .file_names()
        .enumerate()
        .filter(|(_, name)| name.eq_ignore_ascii_case(requested));
    let (index, _) = matches
        .next()
        .ok_or_else(|| format!("ZIP entry {requested} missing"))?;
    if matches.next().is_some() {
        return Err(format!("ZIP entry {requested} has ambiguous case"));
    }
    Ok(index)
}

pub(super) fn validate_xml(
    bytes: &[u8],
    deadline: Instant,
    events_left: &mut usize,
) -> Result<(), String> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut open = Vec::new();
    let mut references = 0usize;
    let mut next_clock_check = 0;
    loop {
        let start = reader.buffer_position() as usize;
        if start >= next_clock_check {
            check_time(deadline).map_err(|e| e.to_string())?;
            next_clock_check = start + 16 * 1024;
        }
        let event = reader.read_event().map_err(|e| e.to_string())?;
        let end = reader.buffer_position() as usize;
        // Escaped text is split into one event per reference. The byte and
        // paragraph limits already bound text, while this budget bounds structure.
        if !matches!(
            event,
            Event::Text(_) | Event::GeneralRef(_) | Event::CData(_)
        ) {
            *events_left = events_left.checked_sub(1).ok_or("Office XML event limit")?;
        }
        if (end - start > MAX_XML_TOKEN && !matches!(event, Event::Comment(_)))
            || open.len() > MAX_XML_DEPTH
        {
            return Err("Office XML token or nesting limit".into());
        }
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                let local = tag.local_name();
                let name = local.as_ref();
                if matches!(
                    name,
                    b"headerReference" | b"footerReference" | b"sldId" | b"Relationship"
                ) {
                    references += 1;
                    if references > MAX_ENTRIES as usize {
                        return Err("Office XML relationship limit".into());
                    }
                }
                let limit = match name {
                    b"tr" => MAX_ROW_BYTES,
                    b"tc" | b"p" => MAX_CELL_BYTES,
                    _ => usize::MAX,
                };
                // Empty elements end at the current depth.
                if !empty {
                    open.push((start, limit));
                }
            }
            Event::End(_) => {
                let (opening, limit) = open.pop().ok_or("unexpected Office XML end tag")?;
                if end - opening > limit {
                    return Err("Office row, cell or paragraph limit".into());
                }
            }
            Event::DocType(_) => return Err("Office XML document type is unsupported".into()),
            Event::Eof if open.is_empty() => return Ok(()),
            Event::Eof => return Err("incomplete Office XML part".into()),
            _ => {}
        }
    }
}

pub(super) fn attr_value(e: &BytesStart<'_>, local_name: &[u8]) -> Result<Option<String>, String> {
    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|error| format!("XML attribute error: {error}"))?;
        if is_local_name(attr.key.as_ref(), local_name) {
            return attr
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, e.decoder())
                .map(|value| Some(value.into_owned()))
                .map_err(|error| format!("XML attribute decode error: {error}"));
        }
    }
    Ok(None)
}

pub(super) fn is_local_name(name: &[u8], expected: &[u8]) -> bool {
    let local = name
        .iter()
        .rposition(|byte| *byte == b':')
        .map(|index| &name[index + 1..])
        .unwrap_or(name);
    local == expected
}

pub(super) fn unescape_text(e: &BytesText<'_>) -> Result<String, String> {
    let decoded = e
        .decode()
        .map_err(|error| format!("XML text decode error: {error}"))?;
    quick_xml::escape::unescape(&decoded)
        .map(|value| value.into_owned())
        .map_err(|error| format!("XML text unescape error: {error}"))
}

pub(super) fn unescape_general_ref(e: &BytesRef<'_>) -> Result<String, String> {
    let decoded = e
        .decode()
        .map_err(|error| format!("XML reference decode error: {error}"))?;
    let wrapped = format!("&{decoded};");
    quick_xml::escape::unescape(&wrapped)
        .map(|value| value.into_owned())
        .map_err(|error| format!("XML reference unescape error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_opened_zip_members_consume_the_shared_decompression_budget() {
        use std::io::{Cursor, Write};
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in [
            ("word/media/example.bmp", 64),
            ("one.xml", 24),
            ("two.xml", 16),
        ] {
            zip.start_file(name, stored).unwrap();
            zip.write_all(&vec![b'x'; bytes]).unwrap();
        }
        let zip = zip.finish().unwrap();
        let size = zip.get_ref().len() as u64;
        let mut zip = open_package(zip, size).unwrap();
        let mut budget = PartBudget::new(Instant::now() + Duration::from_secs(5));
        budget.remaining = 32;
        assert_eq!(
            budget.read(&mut zip, "one.xml", 32).unwrap(),
            (vec![b'x'; 24], false)
        );
        assert_eq!(
            budget.read(&mut zip, "two.xml", 32).unwrap(),
            (vec![b'x'; 8], true)
        );
        assert_eq!(budget.remaining, 0);
    }

    #[test]
    fn a_slow_read_returns_timeout_before_any_followup_io() {
        struct SlowRead {
            until: Instant,
            reads: usize,
        }
        impl Read for SlowRead {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                std::thread::sleep(self.until.saturating_duration_since(Instant::now()));
                bytes[0] = 1;
                Ok(1)
            }
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut reader = CheckedReader::new(
            SlowRead {
                until: deadline,
                reads: 0,
            },
            deadline,
        );
        let mut byte = [0];
        for _ in 0..2 {
            assert_eq!(
                reader.read(&mut byte).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
        }
        assert_eq!(reader.inner.reads, 1);
    }
}
