use crate::{
    infra::{json::valid_hash, locking::ReportLock},
    types::SCAN_CACHE_FILENAME,
    ReportError,
};
use daily_parsers::contract::{ParseResult, PdfPageAudit, WorkerDiagnostic};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

mod records;
mod storage;
#[cfg(test)]
mod tests;

use records::{read_negative, read_parse, snapshot_hash, BoundReference, PreparedBatch};
use storage::{refresh_timeout, sql};

pub const CACHE_FILENAME: &str = SCAN_CACHE_FILENAME;
const MAX_CACHE_BYTES: u64 = 1 << 30;
const MAX_BUSY_WAIT: Duration = Duration::from_secs(2);
const LOCK_SCOPE: &str = "scan-cache-v4";

pub(crate) fn verify_schema(connection: &Connection) -> Result<(), ReportError> {
    storage::verify_schema(connection)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheKey {
    pub file_id: String,
    pub source_guard: String,
    pub profile_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfWindow {
    pub max_pages: u64,
    pub policy_version: String,
    pub extract_max_pages: u64,
    pub selected_pages: Option<Vec<u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheQuery {
    pub key: CacheKey,
    pub pdf_window: Option<PdfWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfObservation {
    pub page_count: u64,
    pub pages: PdfPageAudit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfNegative {
    NoText(PdfObservation),
    Damaged(WorkerDiagnostic),
    PasswordProtected(WorkerDiagnostic),
}

pub struct CachedParse {
    pub query: CacheQuery,
    pub result: ParseResult,
    pub source_sha256: String,
    pub pdf_observation: Option<PdfObservation>,
}

pub struct CachedNegative {
    pub query: CacheQuery,
    pub evidence: PdfNegative,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "key", rename_all = "snake_case")]
pub enum CacheReference {
    Parse(CacheKey),
    Negative(CacheKey),
}

impl CacheReference {
    fn key(&self) -> &CacheKey {
        match self {
            Self::Parse(key) | Self::Negative(key) => key,
        }
    }
}

#[derive(Clone)]
pub struct CacheSnapshot {
    pub key: String,
    pub manifest: Vec<u8>,
    pub references: Vec<CacheReference>,
}

pub(super) struct StoredSnapshot {
    pub origin: u64,
    pub manifest: Vec<u8>,
    references: Vec<BoundReference>,
    access_day: i64,
}

struct Hit {
    record_sha256: String,
    last_access_day: i64,
    parsed: bool,
    value: Option<CacheValue>,
}

pub(super) enum CacheValue {
    Parsed(ParseValue),
    Negative(PdfNegative),
}

pub(super) struct ParseValue {
    pub result: Box<ParseResult>,
    pub source_sha256: String,
    pub observation: Option<PdfObservation>,
}

#[derive(Default)]
pub struct CacheHits {
    entries: HashMap<CacheKey, Hit>,
    session: Option<Arc<()>>,
}

impl CacheHits {
    pub(super) fn get(&self, key: &CacheKey) -> Option<&CacheValue> {
        self.entries.get(key)?.value.as_ref()
    }

    pub(super) fn take(&mut self, key: &CacheKey) -> Option<CacheValue> {
        self.entries.get_mut(key)?.value.take()
    }

    fn proof(&self, reference: &CacheReference) -> Option<&Hit> {
        self.entries
            .get(reference.key())
            .filter(|hit| hit.parsed == matches!(reference, CacheReference::Parse(_)))
    }

    pub fn extend(&mut self, other: Self) {
        match (&self.session, other.session) {
            (None, session) => self.session = session,
            (Some(left), Some(right)) if !Arc::ptr_eq(left, &right) => {
                self.session = Some(Arc::new(()));
            }
            _ => {}
        }
        self.entries.extend(other.entries);
    }
}

#[derive(Default)]
pub struct CacheBatch {
    pub hits: CacheHits,
    pub parsed: Vec<CachedParse>,
    pub negatives: Vec<CachedNegative>,
    pub snapshot: Option<CacheSnapshot>,
}

pub struct ScanCache {
    // Close SQLite before releasing the filesystem lock.
    connection: Option<storage::Database>,
    deadline: Instant,
    _lock: ReportLock,
    path: PathBuf,
    session: Arc<()>,
    snapshot_access: HashMap<String, i64>,
    max_bytes: u64,
}

impl ScanCache {
    pub fn open(cache_dir: &Path, deadline: Instant) -> Result<Self, ReportError> {
        let path = std::path::absolute(cache_dir)?.join(CACHE_FILENAME);
        check_time(deadline)?;
        storage::check_paths(&path)?;
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(MAX_BUSY_WAIT);
        let lock = ReportLock::acquire(&path, LOCK_SCOPE, wait)?;
        check_time(deadline)?;
        let mut cache = Self {
            connection: None,
            deadline,
            _lock: lock,
            path,
            session: Arc::new(()),
            snapshot_access: HashMap::new(),
            max_bytes: MAX_CACHE_BYTES,
        };
        if cache.path.try_exists()? {
            let connection = storage::open(
                &cache.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY,
                cache.deadline,
            )?;
            verify_schema(&connection)?;
            cache.connection = Some(connection);
        }
        check_time(cache.deadline)?;
        Ok(cache)
    }

    /// Explicitly initialize an empty cache for doctor. Normal scans initialize with their first batch.
    pub fn initialize(self) -> Result<(), ReportError> {
        check_time(self.deadline)?;
        if self.connection.is_some() {
            return Ok(());
        }
        self.initialize_and_commit(
            PreparedBatch::default(),
            chrono::Utc::now().timestamp_millis(),
        )
    }

    pub fn lookup<'a>(
        &self,
        queries: impl IntoIterator<Item = &'a CacheQuery>,
    ) -> Result<CacheHits, ReportError> {
        let deadline = self.deadline;
        check_time(deadline)?;
        let mut hits = CacheHits {
            session: Some(Arc::clone(&self.session)),
            ..CacheHits::default()
        };
        let queries = queries.into_iter();
        let mut wanted = HashMap::with_capacity(queries.size_hint().0);
        for query in queries {
            check_time(deadline)?;
            if !query.valid()
                || wanted
                    .insert(&query.key, &query.pdf_window)
                    .is_some_and(|seen| seen != &query.pdf_window)
            {
                return Err(invalid("查询键或 PDF 页窗口无效"));
            }
        }
        let Some(connection) = self.connection.as_ref() else {
            return Ok(hits);
        };
        let keys: Vec<_> = wanted.keys().copied().collect();
        for chunk in keys.chunks(256) {
            storage::read_batch(connection, deadline, chunk, |row| {
                use std::collections::hash_map::Entry;
                let key = storage::row_key(row)?;
                let parsed = row.get::<_, bool>(10)?;
                let window = wanted[&key].as_ref();
                let value = if parsed {
                    read_parse(row, &key, window)?.map(CacheValue::Parsed)
                } else if let Some(window) = window {
                    read_negative(row, &key, window)?.map(CacheValue::Negative)
                } else {
                    None
                };
                if let Some(value) = value {
                    match hits.entries.entry(key) {
                        Entry::Vacant(slot) => {
                            slot.insert(Hit {
                                record_sha256: row.get(4)?,
                                last_access_day: row.get(5)?,
                                parsed,
                                value: Some(value),
                            });
                        }
                        // Ambiguous outcomes require a reparse.
                        Entry::Occupied(slot) => {
                            slot.remove();
                        }
                    }
                }
                Ok(())
            })?;
        }
        check_time(deadline)?;
        Ok(hits)
    }

    pub(super) fn read_snapshot(
        &self,
        snapshot_key: &str,
    ) -> Result<Option<StoredSnapshot>, ReportError> {
        let deadline = self.deadline;
        check_time(deadline)?;
        if !valid_hash(snapshot_key) {
            return Err(invalid("快照键无效"));
        }
        let Some(connection) = self.connection.as_ref() else {
            return Ok(None);
        };
        refresh_timeout(connection, deadline)?;
        let row = sql(
            deadline,
            connection
                .query_row(
                    "SELECT manifest, refs, record_sha256, last_access_day FROM scan_snapshots
                     WHERE snapshot_key=?1 AND length(manifest) + octet_length(refs) <= ?2",
                    params![snapshot_key, storage::MAX_SNAPSHOT_BYTES as i64],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional(),
        )?;
        let Some((manifest, refs, record_sha256, access_day)) = row else {
            return Ok(None);
        };
        let Some((origin, _)) = manifest.split_first_chunk::<8>() else {
            return Ok(None);
        };
        let origin = u64::from_le_bytes(*origin);
        if origin == 0 || snapshot_hash(snapshot_key, &manifest, &refs) != record_sha256 {
            return Ok(None);
        }
        let Ok(references) = serde_json::from_str::<Vec<BoundReference>>(&refs) else {
            return Ok(None);
        };
        Ok(Some(StoredSnapshot {
            origin,
            manifest,
            references,
            access_day,
        }))
    }

    pub(super) fn accept_snapshot<'a>(
        &mut self,
        key: &str,
        snapshot: &StoredSnapshot,
        hits: &CacheHits,
        mut expected: impl Iterator<Item = (&'a CacheKey, bool)>,
    ) -> Result<bool, ReportError> {
        self.verify_hits(hits)?;
        for bound in &snapshot.references {
            check_time(self.deadline)?;
            if expected.next()
                != Some((
                    bound.reference.key(),
                    matches!(bound.reference, CacheReference::Parse(_)),
                ))
                || hits
                    .proof(&bound.reference)
                    .is_none_or(|hit| hit.record_sha256 != bound.record_sha256)
            {
                return Ok(false);
            }
        }
        if expected.next().is_some() {
            return Ok(false);
        }
        self.snapshot_access
            .insert(key.to_owned(), snapshot.access_day);
        Ok(true)
    }

    /// Consume the lock holder, so a finished or timed-out scan cannot publish later.
    pub fn commit(self, batch: CacheBatch) -> Result<(), ReportError> {
        self.commit_at(batch, chrono::Utc::now().timestamp_millis())
    }

    fn commit_at(mut self, batch: CacheBatch, now_ms: i64) -> Result<(), ReportError> {
        let deadline = self.deadline;
        check_time(deadline)?;
        self.verify_hits(&batch.hits)?;
        let day = now_ms.div_euclid(86_400_000);
        let has_entries = !batch.parsed.is_empty() || !batch.negatives.is_empty();
        let touch = batch
            .hits
            .entries
            .values()
            .any(|hit| hit.last_access_day < day)
            || self.snapshot_access.values().any(|access| *access < day);
        if !has_entries && !touch {
            return Ok(());
        }
        let batch = PreparedBatch::new(batch, deadline)?;
        let Some(reader) = self.connection.take() else {
            return self.initialize_and_commit(batch, now_ms);
        };
        drop(reader);
        let connection =
            storage::open(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE, self.deadline)?;
        self.write(connection, false, batch, now_ms)
    }

    fn initialize_and_commit(self, batch: PreparedBatch, now_ms: i64) -> Result<(), ReportError> {
        let deadline = self.deadline;
        check_time(deadline)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        let connection =
            storage::open(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE, self.deadline)?;
        storage::enable_wal(&connection, deadline)?;
        self.write(connection, true, batch, now_ms)
    }

    fn write(
        &self,
        mut connection: storage::Database,
        initialize: bool,
        batch: PreparedBatch,
        now_ms: i64,
    ) -> Result<(), ReportError> {
        let deadline = self.deadline;
        let day = now_ms.div_euclid(86_400_000);
        refresh_timeout(&connection, deadline)?;
        storage::checkpoint_if_needed(&connection, &self.path, self.max_bytes / 2, deadline)?;
        let tx = sql(
            deadline,
            connection.transaction_with_behavior(TransactionBehavior::Immediate),
        )?;
        if initialize {
            sql(deadline, tx.execute_batch(storage::SCHEMA))?;
        }
        storage::write_entries(&tx, deadline, &batch.hits, batch.entries, day, now_ms)?;
        for (key, access) in &self.snapshot_access {
            if *access < day {
                refresh_timeout(&tx, deadline)?;
                sql(deadline, tx.execute(
                    "UPDATE scan_snapshots SET last_access_day=?1 WHERE snapshot_key=?2 AND last_access_day<?1",
                    params![day, key],
                ))?;
            }
        }
        if let Some(snapshot) = batch.snapshot {
            storage::write_snapshot(&tx, deadline, snapshot, day, now_ms)?;
        }
        storage::evict(&tx, &self.path, self.max_bytes, deadline)?;
        refresh_timeout(&tx, deadline)?;
        sql(deadline, tx.commit())?;
        storage::checkpoint_if_needed(&connection, &self.path, self.max_bytes, deadline)
    }

    /// Cache integrity probe. A missing database is an empty, rebuildable cache.
    pub fn check(cache_dir: &Path, deadline: Instant) -> Result<(), ReportError> {
        let path = std::path::absolute(cache_dir)?.join(CACHE_FILENAME);
        storage::check_paths(&path)?;
        if !path.try_exists()? {
            return Ok(());
        }
        let connection = storage::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY, deadline)?;
        verify_schema(&connection)?;
        refresh_timeout(&connection, deadline)?;
        let status = sql(
            deadline,
            connection.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0)),
        )?;
        if status != "ok" || storage::disk_len(&path)? > MAX_CACHE_BYTES {
            return Err(invalid("扫描缓存完整性或大小检查失败"));
        }
        Ok(())
    }

    pub fn cleanup(&mut self) -> Result<(), ReportError> {
        let deadline = self.deadline;
        check_time(deadline)?;
        if self.connection.is_none() {
            return Ok(());
        }
        self.session = Arc::new(());
        self.snapshot_access.clear();
        let mut connection =
            storage::open(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE, self.deadline)?;
        storage::checkpoint_if_needed(&connection, &self.path, self.max_bytes, deadline)?;
        refresh_timeout(&connection, deadline)?;
        let tx = sql(
            deadline,
            connection.transaction_with_behavior(TransactionBehavior::Immediate),
        )?;
        storage::prune_snapshots(&tx, deadline)?;
        storage::evict(&tx, &self.path, self.max_bytes, deadline)?;
        refresh_timeout(&tx, deadline)?;
        sql(deadline, tx.commit())?;
        storage::checkpoint_if_needed(&connection, &self.path, self.max_bytes, deadline)?;
        Ok(())
    }

    fn verify_hits(&self, hits: &CacheHits) -> Result<(), ReportError> {
        if hits
            .session
            .as_ref()
            .is_some_and(|session| !Arc::ptr_eq(session, &self.session))
        {
            return Err(invalid("查询结果属于另一个扫描会话"));
        }
        Ok(())
    }
}

fn check_time(deadline: Instant) -> Result<(), ReportError> {
    if Instant::now() >= deadline {
        Err(ReportError::new(
            "SCAN_CACHE_DEADLINE",
            "扫描缓存收尾已到截止时间",
        ))
    } else {
        Ok(())
    }
}

fn invalid(message: &'static str) -> ReportError {
    ReportError::new("SCAN_CACHE_INVALID", message)
}
