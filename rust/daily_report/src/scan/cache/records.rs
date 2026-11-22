use super::*;
use daily_parsers::{
    contract::{ContentUnitKind, ParserBackend, SourceRangeUnit, OCR_RANGE_LABEL},
    hex,
    settings::pdf_sample_pages,
};
use rusqlite::Row;
use sha2::{Digest, Sha256};

pub(super) const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILE_CHARS: usize = 60_000;
pub(super) const MAX_CONTENT_BYTES: usize = MAX_FILE_CHARS * 4;

impl CacheKey {
    fn valid(&self) -> bool {
        [&self.file_id, &self.source_guard]
            .into_iter()
            .all(|value| !value.is_empty() && value.len() <= 16_384 && !value.contains('\0'))
            && valid_hash(&self.profile_hash)
    }
}

impl CacheQuery {
    pub(super) fn valid(&self) -> bool {
        self.key.valid() && self.pdf_window.as_ref().is_none_or(PdfWindow::valid)
    }
}

impl PdfWindow {
    fn valid(&self) -> bool {
        (1..=10_000).contains(&self.max_pages)
            && (1..=10_000).contains(&self.extract_max_pages)
            && matches!(
                self.policy_version.as_str(),
                "pdf_text_presence_v1" | "pdf_text_presence_v2"
            )
            && self.selected_pages.as_ref().is_none_or(|pages| {
                !pages.is_empty()
                    && pages.len() as u64 <= self.extract_max_pages
                    && !pages.contains(&0)
                    && pages.iter().collect::<HashSet<_>>().len() == pages.len()
            })
    }

    fn inspection_pages(&self, page_count: u64) -> Vec<u64> {
        pdf_sample_pages(page_count, self.max_pages, &self.policy_version)
    }

    fn extraction_pages(&self, page_count: u64, first_text: Option<u64>) -> Vec<u64> {
        daily_parsers::settings::pdf_extraction_pages(
            page_count,
            self.extract_max_pages,
            first_text,
            self.selected_pages.as_deref(),
            &self.policy_version,
        )
    }
}

impl PdfObservation {
    fn valid_for(&self, window: &PdfWindow, require_no_text: bool) -> bool {
        let Some(observations) = &self.pages.observations else {
            return false;
        };
        if self.page_count == 0
            || observations.is_empty()
            || self.pages.policy_version != window.policy_version
            || self.pages.validate().is_err()
        {
            return false;
        }
        let expected = window.inspection_pages(self.page_count);
        if observations.last().is_some_and(|page| page.has_text) {
            !require_no_text
                && expected.starts_with(&self.pages.checked_pages)
                && observations[..observations.len() - 1]
                    .iter()
                    .all(|page| !page.has_text)
        } else {
            expected == self.pages.checked_pages && observations.iter().all(|page| !page.has_text)
        }
    }
}

impl PdfNegative {
    fn valid_for(&self, window: &PdfWindow) -> bool {
        match self {
            Self::NoText(observation) => observation.valid_for(window, true),
            Self::Damaged(error) => valid_native_error(error, "PARSER_FAILED", "FPDF_ERR_FORMAT"),
            Self::PasswordProtected(error) => {
                valid_native_error(error, "PDF_PROTECTED", "FPDF_ERR_PASSWORD")
            }
        }
    }
}

fn valid_native_error(error: &WorkerDiagnostic, code: &str, cause: &str) -> bool {
    error.error_code == code
        && error.message == cause
        && !error.retryable
        && error.stage == "parse"
        && error.backend.as_deref() == Some(ParserBackend::RustPdfiumV3.as_str())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsePayload {
    result: ParseResult,
    window: Option<PdfWindow>,
    observation: Option<PdfObservation>,
}

impl ParsePayload {
    fn valid_for(&self, key: &CacheKey) -> bool {
        let result = &self.result;
        if result.observed_source_version != key.source_guard
            || result.file_path.is_empty()
            || result.file_path.contains('\0')
            || !result.parser_backend.supports(&result.file_type)
            || result.worker_lane != result.parser_backend.lane()
            || result.content.chars().take(MAX_FILE_CHARS + 1).count() > MAX_FILE_CHARS
        {
            return false;
        }
        let Some(audit) = &result.evidence_audit else {
            return false;
        };
        let units = audit.content_units.as_deref().unwrap_or_default();
        if audit.validate(&result.content).is_err()
            || units.iter().any(|unit| {
                unit.ranges.is_empty()
                    && !matches!(
                        unit.kind,
                        ContentUnitKind::Heading
                            | ContentUnitKind::TableHeader
                            | ContentUnitKind::Placeholder
                    )
            })
            || (!result.content.is_empty()
                && units.is_empty()
                && audit.read_ranges.as_ref().is_none_or(Vec::is_empty))
        {
            return false;
        }
        match (&self.window, &self.observation) {
            (None, None) => result.parser_backend != ParserBackend::RustPdfiumV3,
            (Some(window), Some(observation)) => {
                if result.parser_backend != ParserBackend::RustPdfiumV3
                    || !observation.valid_for(window, false)
                {
                    return false;
                }
                let first_text = observation
                    .pages
                    .observations
                    .as_ref()
                    .and_then(|pages| pages.last())
                    .filter(|page| page.has_text)
                    .map(|page| page.page);
                let pages: HashSet<_> = window
                    .extraction_pages(observation.page_count, first_text)
                    .into_iter()
                    .collect();
                let expected_label = first_text.is_none().then_some(OCR_RANGE_LABEL);
                audit
                    .read_ranges
                    .iter()
                    .flatten()
                    .chain(
                        audit
                            .content_units
                            .iter()
                            .flatten()
                            .flat_map(|unit| &unit.ranges),
                    )
                    .all(|range| {
                        range.unit == SourceRangeUnit::PdfPage
                            && range.label.as_deref() == expected_label
                            && range.start <= observation.page_count
                            && range.start.checked_add(1) == Some(range.end)
                            && pages.contains(&range.start)
                    })
            }
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NegativePayload {
    window: PdfWindow,
    evidence: PdfNegative,
}

pub(super) struct ParseColumns {
    pub(super) backend: &'static str,
    pub(super) content_text: String,
    pub(super) content_sha256: String,
    pub(super) source_sha256: String,
}

pub(super) struct EncodedEntry {
    pub(super) reference: CacheReference,
    pub(super) payload: Vec<u8>,
    pub(super) record_sha256: String,
    pub(super) parse: Option<ParseColumns>,
}

impl EncodedEntry {
    pub(super) fn parsed(item: CachedParse) -> Result<Self, ReportError> {
        if !item.query.valid() || !valid_hash(&item.source_sha256) {
            return Err(invalid("解析来源或原文件哈希无效"));
        }
        let mut payload = ParsePayload {
            result: item.result,
            window: item.query.pdf_window,
            observation: item.pdf_observation,
        };
        if !payload.valid_for(&item.query.key) {
            return Err(invalid("解析正文或来源范围无效"));
        }
        let parse = ParseColumns {
            backend: payload.result.parser_backend.as_str(),
            content_sha256: crate::snapshots::digest(payload.result.content.as_bytes()),
            content_text: std::mem::take(&mut payload.result.content),
            source_sha256: item.source_sha256,
        };
        let bytes = serde_json::to_vec(&payload)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(invalid("解析记录超出大小上限"));
        }
        let reference = CacheReference::Parse(item.query.key);
        let record_sha256 = parse_hash(
            reference.key(),
            &bytes,
            parse.backend,
            &parse.content_sha256,
            &parse.source_sha256,
        );
        Ok(Self {
            reference,
            payload: bytes,
            record_sha256,
            parse: Some(parse),
        })
    }

    pub(super) fn negative(item: CachedNegative) -> Result<Self, ReportError> {
        if !item.query.valid() {
            return Err(invalid("PDF 负项查询键无效"));
        }
        let window = item
            .query
            .pdf_window
            .ok_or_else(|| invalid("PDF 负项缺少页窗口"))?;
        if !item.evidence.valid_for(&window) {
            return Err(invalid("PDF 负项缺少完整页级观察或已确认的错误证明"));
        }
        let payload = serde_json::to_vec(&NegativePayload {
            window,
            evidence: item.evidence,
        })?;
        if payload.len() > MAX_RECORD_BYTES {
            return Err(invalid("PDF 负项超出大小上限"));
        }
        let reference = CacheReference::Negative(item.query.key);
        let record_sha256 = entry_hash(b"negative", reference.key(), &[&payload]);
        Ok(Self {
            reference,
            payload,
            record_sha256,
            parse: None,
        })
    }
}

pub(super) fn read_parse(
    row: &Row<'_>,
    key: &CacheKey,
    expected_window: Option<&PdfWindow>,
) -> rusqlite::Result<Option<ParseValue>> {
    let bytes = row.get_ref(3)?.as_blob()?;
    let record_sha256 = row.get_ref(4)?.as_str()?;
    let backend = row.get_ref(6)?.as_str()?;
    let content_sha256 = row.get_ref(7)?.as_str()?;
    let source_sha256 = row.get_ref(8)?.as_str()?;
    let content = row.get_ref(9)?.as_str()?;
    if bytes.len() > MAX_RECORD_BYTES
        || content.len() > MAX_CONTENT_BYTES
        || !valid_hash(source_sha256)
        || crate::snapshots::digest(content.as_bytes()) != content_sha256
        || parse_hash(key, bytes, backend, content_sha256, source_sha256) != record_sha256
    {
        return Ok(None);
    }
    let Ok(mut payload) = serde_json::from_slice::<ParsePayload>(bytes) else {
        return Ok(None);
    };
    if !payload.result.content.is_empty() {
        return Ok(None);
    }
    payload.result.content = content.to_owned();
    if payload.window.as_ref() != expected_window
        || payload.result.parser_backend.as_str() != backend
        || !payload.valid_for(key)
    {
        return Ok(None);
    }
    Ok(Some(ParseValue {
        result: Box::new(payload.result),
        observation: payload.observation,
        source_sha256: source_sha256.to_owned(),
    }))
}

pub(super) fn read_negative(
    row: &Row<'_>,
    key: &CacheKey,
    expected_window: &PdfWindow,
) -> rusqlite::Result<Option<PdfNegative>> {
    let bytes = row.get_ref(3)?.as_blob()?;
    let record_sha256 = row.get_ref(4)?.as_str()?;
    if bytes.len() > MAX_RECORD_BYTES || entry_hash(b"negative", key, &[bytes]) != record_sha256 {
        return Ok(None);
    }
    let Ok(payload) = serde_json::from_slice::<NegativePayload>(bytes) else {
        return Ok(None);
    };
    if &payload.window != expected_window || !payload.evidence.valid_for(expected_window) {
        return Ok(None);
    }
    Ok(Some(payload.evidence))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BoundReference {
    pub(super) reference: CacheReference,
    pub(super) record_sha256: String,
}

#[derive(Default)]
pub(super) struct PreparedBatch {
    pub(super) hits: CacheHits,
    pub(super) entries: Vec<EncodedEntry>,
    pub(super) snapshot: Option<EncodedSnapshot>,
}

impl PreparedBatch {
    pub(super) fn new(batch: CacheBatch, deadline: Instant) -> Result<Self, ReportError> {
        let mut entries = Vec::with_capacity(batch.parsed.len() + batch.negatives.len());
        let mut written_keys = HashSet::with_capacity(entries.capacity());
        for entry in batch
            .parsed
            .into_iter()
            .map(EncodedEntry::parsed)
            .chain(batch.negatives.into_iter().map(EncodedEntry::negative))
        {
            check_time(deadline)?;
            let entry = entry?;
            if !written_keys.insert(entry.reference.key().clone()) {
                return Err(invalid("同一缓存键在批次内有多个结果"));
            }
            entries.push(entry);
        }
        let snapshot = batch
            .snapshot
            .filter(|_| !entries.is_empty())
            .map(|snapshot| EncodedSnapshot::new(snapshot, &batch.hits, &entries, deadline))
            .transpose()?
            .flatten();
        Ok(Self {
            hits: batch.hits,
            entries,
            snapshot,
        })
    }
}

pub(super) struct EncodedSnapshot {
    pub(super) key: String,
    pub(super) manifest: Vec<u8>,
    pub(super) refs: String,
    pub(super) fingerprint: String,
}

impl EncodedSnapshot {
    fn new(
        snapshot: CacheSnapshot,
        hits: &CacheHits,
        written: &[EncodedEntry],
        deadline: Instant,
    ) -> Result<Option<Self>, ReportError> {
        if !valid_hash(&snapshot.key) || snapshot.manifest.is_empty() {
            return Err(invalid("快照键或清单无效"));
        }
        let written: HashMap<_, _> = written
            .iter()
            .map(|entry| {
                (
                    entry.reference.key(),
                    (&entry.reference, &entry.record_sha256),
                )
            })
            .collect();
        let mut seen = HashSet::with_capacity(snapshot.references.len());
        let mut bound = Vec::with_capacity(snapshot.references.len());
        for reference in snapshot.references {
            check_time(deadline)?;
            let key = reference.key();
            if !seen.insert(key.clone()) {
                return Err(invalid("快照引用重复"));
            }
            let record_sha256 = match written.get(key) {
                Some((kind, hash)) if *kind == &reference => Some(*hash),
                Some(_) => None,
                None => hits.proof(&reference).map(|hit| &hit.record_sha256),
            }
            .ok_or_else(|| invalid("快照引用缺少已验证的缓存结果"))?
            .clone();
            bound.push(BoundReference {
                reference,
                record_sha256,
            });
        }
        let refs = serde_json::to_string(&bound)?;
        if refs.len().saturating_add(snapshot.manifest.len()) > storage::MAX_SNAPSHOT_BYTES {
            return Ok(None);
        }
        let fingerprint = snapshot_hash(&snapshot.key, &snapshot.manifest, &refs);
        Ok(Some(Self {
            key: snapshot.key,
            manifest: snapshot.manifest,
            refs,
            fingerprint,
        }))
    }
}

pub(super) fn snapshot_hash(key: &str, manifest: &[u8], refs: &str) -> String {
    hash(b"snapshot", [key.as_bytes(), manifest, refs.as_bytes()])
}

fn parse_hash(
    key: &CacheKey,
    bytes: &[u8],
    backend: &str,
    content_sha256: &str,
    source_sha256: &str,
) -> String {
    entry_hash(
        b"parse",
        key,
        &[
            bytes,
            backend.as_bytes(),
            content_sha256.as_bytes(),
            source_sha256.as_bytes(),
        ],
    )
}

fn entry_hash(kind: &[u8], key: &CacheKey, data: &[&[u8]]) -> String {
    hash(
        kind,
        [
            key.file_id.as_bytes(),
            key.source_guard.as_bytes(),
            key.profile_hash.as_bytes(),
        ]
        .into_iter()
        .chain(data.iter().copied()),
    )
}

fn hash<'a>(kind: &[u8], pieces: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"scan-cache-v4\0");
    hasher.update(kind);
    for piece in pieces {
        hasher.update((piece.len() as u64).to_le_bytes());
        hasher.update(piece);
    }
    hex(&hasher.finalize())
}
