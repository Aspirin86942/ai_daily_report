use super::*;
use std::time::Duration;

// Daily inspection shares one decompressed-byte allowance across all inspected parts.
// A partial XML part may supply complete worksheet rows, never a partial cell or chart.
pub(super) struct PartBudget {
    pub remaining: usize,
    pub deadline: Instant,
}

impl PartBudget {
    pub fn new(request: &OfficeParseRequest) -> Self {
        let timeout = positive_limit(&request.limits, "remaining_timeout_ms", 30_000);
        Self {
            remaining: 32 * 1024 * 1024,
            deadline: Instant::now() + Duration::from_millis(timeout.min(3_600_000) as u64),
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
