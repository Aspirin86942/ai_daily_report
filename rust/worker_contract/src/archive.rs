//! Streamed archive artifacts. Only their small manifest crosses the worker pipe.
use crate::{ContentUnitKind, ParseRequest, SourceRange};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveTextRequest {
    pub file_path: PathBuf,
    pub file_type: String,
    pub raw_sha256: String,
    pub output_dir: PathBuf,
    pub max_input_bytes: u64,
    pub max_output_bytes: u64,
    pub timeout_ms: u64,
    pub csv_encodings: Vec<String>,
}
impl ArchiveTextRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !self.file_path.is_absolute()
            || !self.output_dir.is_absolute()
            || !matches!(
                self.file_type.as_str(),
                ".txt" | ".md" | ".csv" | ".json" | ".log"
            )
            || self.raw_sha256.len() != 64
            || !(1..=536870912).contains(&self.max_input_bytes)
            || !(1..=268435456).contains(&self.max_output_bytes)
            || !(1..=300000).contains(&self.timeout_ms)
        {
            return Err("archive text request is invalid".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveRequest {
    pub source: ParseRequest,
    pub raw_sha256: String,
    pub output_dir: PathBuf,
    pub max_output_bytes: u64,
}
impl ArchiveRequest {
    pub fn validate(&self) -> Result<(), String> {
        self.source.validate()?;
        if !self.output_dir.is_absolute()
            || self.raw_sha256.len() != 64
            || !self
                .raw_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || !(1..=268435456).contains(&self.max_output_bytes)
        {
            return Err("archive request is invalid".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveUnit {
    pub unit_id: String,
    pub kind: ContentUnitKind,
    pub byte_start: u64,
    pub byte_end: u64,
    pub text_start: u64,
    pub text_end: u64,
    pub sha256: String,
    pub ranges: Vec<SourceRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArchiveArtifact {
    pub version: u32,
    #[serde(default)]
    pub coverage: serde_json::Value,
    pub body_sha256: String,
    pub units_sha256: String,
    pub index_sha256: String,
    pub body_bytes: u64,
    pub units_bytes: u64,
    pub units: u64,
    pub chars: u64,
    pub usable_chars: u64,
    pub truncated: bool,
    pub gaps: Vec<String>,
    pub error: Option<String>,
    pub parser_backend: String,
    pub worker_lane: String,
    pub duration_ms: u64,
    pub raw_sha256: String,
}

pub struct ArchiveWriter {
    body: BufWriter<File>,
    units: BufWriter<File>,
    index: BufWriter<File>,
    body_hash: Sha256,
    units_hash: Sha256,
    index_hash: Sha256,
    max_bytes: u64,
    bytes: u64,
    unit_bytes: u64,
    count: u64,
    chars: u64,
    usable: u64,
    truncated: bool,
    error: Option<String>,
    gaps: Vec<String>,
    headers: std::collections::BTreeMap<String, u64>,
}
impl ArchiveWriter {
    pub fn new(directory: &Path, max_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let open = |name: &str| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))
        };
        Ok(Self {
            body: BufWriter::new(open("body.md")?),
            units: BufWriter::new(open("units.jsonl")?),
            index: BufWriter::new(open("units.idx")?),
            body_hash: Sha256::new(),
            units_hash: Sha256::new(),
            index_hash: Sha256::new(),
            max_bytes,
            bytes: 0,
            unit_bytes: 0,
            count: 0,
            chars: 0,
            usable: 0,
            truncated: false,
            error: None,
            gaps: vec![],
            headers: Default::default(),
        })
    }
    pub fn push(&mut self, text: &str, kind: ContentUnitKind, ranges: Vec<SourceRange>) -> bool {
        if self.error.is_some() || self.truncated {
            return false;
        }
        let text = text.trim();
        if text.is_empty() {
            return true;
        }
        // Bound each unit without dropping the rest of a long paragraph or page.
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(256 * 1024);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let part = &rest[..end];
            match self.write_unit(part, kind, ranges.clone()) {
                Ok(true) => {}
                Ok(false) => {
                    self.truncated = true;
                    return false;
                }
                Err(e) => {
                    self.error = Some(e.to_string());
                    return false;
                }
            }
            rest = &rest[end..];
        }
        true
    }
    fn write_unit(
        &mut self,
        text: &str,
        kind: ContentUnitKind,
        ranges: Vec<SourceRange>,
    ) -> io::Result<bool> {
        let separator = u64::from(self.count > 0);
        let size = text.len() as u64;
        let chars = text.chars().count() as u64;
        let unit = ArchiveUnit {
            unit_id: format!("u{}", self.count + 1),
            kind,
            byte_start: self.bytes + separator,
            byte_end: self.bytes + separator + size,
            text_start: self.chars + separator,
            text_end: self.chars + separator + chars,
            sha256: hex(&Sha256::digest(text.as_bytes())),
            ranges,
        };
        let mut line = serde_json::to_vec(&unit)?;
        line.push(b'\n');
        if self.bytes
            + self.unit_bytes
            + (self.count + 1) * 8
            + separator
            + size
            + line.len() as u64
            > self.max_bytes
        {
            return Ok(false);
        }
        if separator != 0 {
            self.body.write_all(b"\n")?;
            self.body_hash.update(b"\n");
        }
        self.body.write_all(text.as_bytes())?;
        self.body_hash.update(text.as_bytes());
        self.units.write_all(&line)?;
        self.units_hash.update(&line);
        let index = self.unit_bytes.to_le_bytes();
        self.index.write_all(&index)?;
        self.index_hash.update(index);
        self.bytes = unit.byte_end;
        self.chars = unit.text_end;
        self.unit_bytes += line.len() as u64;
        self.count += 1;
        if kind == ContentUnitKind::TableHeader {
            let label = unit
                .ranges
                .first()
                .and_then(|r| r.label.clone())
                .unwrap_or_else(|| "table".into());
            self.headers.entry(label).or_insert(self.count);
        }
        if !matches!(
            kind,
            ContentUnitKind::Heading
                | ContentUnitKind::TableHeader
                | ContentUnitKind::Unit
                | ContentUnitKind::Placeholder
        ) {
            self.usable += chars;
        }
        Ok(true)
    }
    pub fn gap(&mut self, code: &str) {
        if !self.gaps.iter().any(|v| v == code) {
            self.gaps.push(code.to_owned());
        }
    }
    pub fn finish(mut self) -> ArchiveArtifact {
        let flushed = (|| -> io::Result<()> {
            for writer in [&mut self.body, &mut self.units, &mut self.index] {
                writer.flush()?;
                writer.get_ref().sync_all()?;
            }
            Ok(())
        })();
        if let Err(e) = flushed {
            self.error = Some(e.to_string());
        }
        if self.truncated {
            self.gaps.push("ARCHIVE_OUTPUT_LIMIT".into());
        }
        ArchiveArtifact {
            coverage: serde_json::json!({"table_headers": self.headers}),
            version: 1,
            body_sha256: hex(&self.body_hash.finalize()),
            units_sha256: hex(&self.units_hash.finalize()),
            index_sha256: hex(&self.index_hash.finalize()),
            body_bytes: self.bytes,
            units_bytes: self.unit_bytes,
            units: self.count,
            chars: self.chars,
            usable_chars: self.usable,
            truncated: self.truncated,
            gaps: self.gaps,
            error: self.error,
            parser_backend: String::new(),
            worker_lane: String::new(),
            duration_ms: 0,
            raw_sha256: String::new(),
        }
    }
}
