use quick_xml::events::{BytesRef, BytesStart, BytesText};
use std::fs::File;
use std::io::{Read, Seek};
use std::time::{Duration, Instant};
use zip::read::ZipArchive;

// Daily inspection shares one decompressed-byte allowance across all inspected parts.
// A partial XML part may supply complete worksheet rows, never a partial cell or chart.
pub(super) struct PartBudget {
    pub remaining: usize,
    pub deadline: Instant,
}

impl PartBudget {
    pub fn new(timeout: u64) -> Self {
        Self {
            remaining: 32 * 1024 * 1024,
            deadline: Instant::now() + Duration::from_millis(timeout.min(3_600_000)),
        }
    }

    pub fn reserve_render_time(&mut self) {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        self.deadline -= (remaining / 5).min(Duration::from_millis(500));
    }

    pub fn main_part(
        &mut self,
        zip: &mut ZipArchive<File>,
    ) -> Option<office_oxide::core::opc::PartName> {
        use office_oxide::core::relationships::{rel_types, Relationships};
        let xml = self.complete(zip, "_rels/.rels", 1024 * 1024)?;
        let links = Relationships::parse(&xml).ok()?;
        let rel = links.first_by_type(rel_types::OFFICE_DOCUMENT)?;
        links.resolve_target_from_root(&rel.id).ok()
    }

    pub fn complete(
        &mut self,
        zip: &mut ZipArchive<File>,
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
        let name = resolve_zip_member(zip, name)?;
        let mut member = zip.by_name(&name).map_err(|e| e.to_string())?;
        let size = usize::try_from(member.size()).unwrap_or(usize::MAX);
        let allowance = self.remaining.min(limit).min(size);
        let mut bytes = Vec::new();
        let mut chunk = [0; 16 * 1024];
        while bytes.len() < allowance && Instant::now() < self.deadline {
            let len = chunk.len().min(allowance - bytes.len());
            let n = member.read(&mut chunk[..len]).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err(format!("ZIP entry {name} ended before its declared size"));
            }
            bytes.extend_from_slice(&chunk[..n]);
            self.remaining -= n;
        }
        let partial = bytes.len() < size;
        Ok((bytes, partial))
    }
}

pub(super) fn resolve_zip_member<R: Read + Seek>(
    archive: &ZipArchive<R>,
    requested: &str,
) -> Result<String, String> {
    let mut matches = archive
        .file_names()
        .filter(|name| name.eq_ignore_ascii_case(requested));
    let first = matches
        .next()
        .ok_or_else(|| format!("ZIP entry {requested} missing"))?;
    if matches.next().is_some() {
        return Err(format!("ZIP entry {requested} has ambiguous case"));
    }
    Ok(first.to_string())
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
