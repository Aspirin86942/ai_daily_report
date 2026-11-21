use std::mem::size_of_val;

use ai_daily_worker_contract::SourceRange;

use super::*;

const MAX_BATCH_RECORDS: usize = 32;
const MAX_BATCH_BYTES: usize = 1024 * 1024;
const MAX_BATCH_WAIT_MS: u64 = 25;
const BATCH_DEADLINE_MARGIN_MS: u64 = 50;

pub(super) fn cache_timestamp_ms() -> Result<u64, CachePortError> {
    crate::store::current_time_millis().map_err(|error| CachePortError::Store {
        detail: error.to_string(),
    })
}

pub(super) trait CacheRecord: Sized {
    const LABEL: &'static str;

    fn estimated_bytes(&self) -> usize;

    fn write(cache: &dyn CachePort, now: u64, records: &[Self]) -> Result<(), CachePortError>;
}

impl CacheRecord for CacheWriteRecord {
    const LABEL: &'static str = "parse cache";

    fn estimated_bytes(&self) -> usize {
        let text = [
            &self.file_identity,
            &self.source_version,
            &self.source_guard_kind,
            &self.source_guard_sha256,
            &self.parse_profile_hash,
            &self.content,
            &self.content_sha256,
            &self.parser_backend,
            &self.worker_lane,
            &self.worker_contract_version,
            &self.worker_version,
            &self.worker_build,
        ]
        .iter()
        .map(|value| value.len())
        .sum::<usize>();
        let audit = self.evidence_audit.as_ref().map_or(0, |audit| {
            let ranges = audit.read_ranges.as_deref().unwrap_or_default();
            let units = audit.content_units.as_deref().unwrap_or_default();
            size_of_val(units)
                + range_bytes(ranges)
                + units
                    .iter()
                    .map(|unit| unit.unit_id.len() + range_bytes(&unit.ranges))
                    .sum::<usize>()
                + audit.encoding.as_ref().map_or(0, String::len)
                + audit.detected_format.as_ref().map_or(0, String::len)
        });
        size_of_val(self) + text + audit
    }

    fn write(cache: &dyn CachePort, now: u64, records: &[Self]) -> Result<(), CachePortError> {
        cache.write_parse(now, records)
    }
}

fn range_bytes(ranges: &[SourceRange]) -> usize {
    size_of_val(ranges)
        + ranges
            .iter()
            .filter_map(|range| range.label.as_ref())
            .map(String::len)
            .sum::<usize>()
}

impl CacheRecord for ClassificationCacheWriteRecord {
    const LABEL: &'static str = "classification cache";

    fn estimated_bytes(&self) -> usize {
        size_of_val(self)
            + [
                &self.file_identity,
                &self.source_version,
                &self.source_guard_kind,
                &self.source_guard_sha256,
                &self.classifier_profile_hash,
                &self.classifier_build,
                &self.status,
            ]
            .iter()
            .map(|value| value.len())
            .sum::<usize>()
    }

    fn write(cache: &dyn CachePort, now: u64, records: &[Self]) -> Result<(), CachePortError> {
        cache.write_classification(now, records)
    }
}

struct WriteSource<'a> {
    index: usize,
    path: &'a str,
    backend: &'static str,
}

pub(super) struct CacheBatch<'a, R> {
    scheduler: &'a BudgetedContextScheduler,
    records: Vec<R>,
    sources: Vec<WriteSource<'a>>,
    bytes: usize,
    flush_at: Option<u64>,
    receipts: Vec<(usize, R)>,
    warnings: Vec<(usize, Diagnostic)>,
    deadline_exhausted: bool,
}

impl<'a, R: CacheRecord> CacheBatch<'a, R> {
    pub(super) fn new(scheduler: &'a BudgetedContextScheduler) -> Self {
        Self {
            scheduler,
            records: Vec::new(),
            sources: Vec::new(),
            bytes: 0,
            flush_at: None,
            receipts: Vec::new(),
            warnings: Vec::new(),
            deadline_exhausted: false,
        }
    }

    pub(super) fn push(&mut self, index: usize, path: &'a str, backend: &'static str, record: R) {
        let bytes = record.estimated_bytes();
        if self.bytes.saturating_add(bytes) > MAX_BATCH_BYTES {
            self.flush();
        }
        let now = self.scheduler.clock.now_ms();
        let work_deadline = self.scheduler.stored_work_deadline;
        if now >= work_deadline || self.deadline_exhausted {
            self.deadline_exhausted = true;
            return;
        }
        self.records.push(record);
        self.sources.push(WriteSource {
            index,
            path,
            backend,
        });
        self.bytes += bytes;
        let flush_by = work_deadline.saturating_sub(BATCH_DEADLINE_MARGIN_MS);
        let flush_at = *self
            .flush_at
            .get_or_insert(now.saturating_add(MAX_BATCH_WAIT_MS).min(flush_by));
        // An oversized record retains the existing single-record write path.
        if self.records.len() >= MAX_BATCH_RECORDS
            || self.bytes >= MAX_BATCH_BYTES
            || now >= flush_at
        {
            self.flush();
        }
    }

    pub(super) fn flush_at(&self) -> Option<u64> {
        self.flush_at
    }

    fn write(&self, records: &[R]) -> Result<(), CachePortError> {
        if self.deadline_exhausted
            || self.scheduler.clock.now_ms() >= self.scheduler.stored_work_deadline
        {
            return Err(CachePortError::DeadlineExhausted {
                detail: "work deadline exhausted before cache write".to_string(),
            });
        }
        R::write(
            self.scheduler.cache.as_ref(),
            cache_timestamp_ms()?,
            records,
        )
    }

    pub(super) fn flush(&mut self) {
        if self.records.is_empty() {
            return;
        }
        let mut records = std::mem::take(&mut self.records);
        let mut sources = std::mem::take(&mut self.sources);
        match self.write(&records) {
            Ok(()) => self.receipts.extend(
                sources
                    .drain(..)
                    .map(|source| source.index)
                    .zip(records.drain(..)),
            ),
            Err(CachePortError::DeadlineExhausted { .. }) => self.deadline_exhausted = true,
            Err(CachePortError::Record { .. }) if records.len() > 1 => {
                // The failed transaction rolled back. Only record-specific
                // failures justify trying the other rows individually.
                let mut rows = sources.drain(..).zip(records.drain(..));
                while let Some((source, record)) = rows.next() {
                    match self.write(std::slice::from_ref(&record)) {
                        Ok(()) => self.receipts.push((source.index, record)),
                        Err(CachePortError::DeadlineExhausted { .. }) => {
                            self.deadline_exhausted = true;
                            break;
                        }
                        Err(error @ CachePortError::Record { .. }) => self.skip(&source, &error),
                        Err(error) => {
                            self.skip(&source, &error);
                            for (remaining, _) in rows {
                                self.skip(&remaining, &error);
                            }
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                for source in &sources {
                    self.skip(source, &error);
                }
            }
        }
        records.clear();
        sources.clear();
        self.records = records;
        self.sources = sources;
        self.bytes = 0;
        self.flush_at = None;
    }

    fn skip(&mut self, source: &WriteSource<'_>, error: &CachePortError) {
        self.warnings.push((
            source.index,
            Diagnostic {
                error_code: ErrorCode::CacheWriteFailed,
                message: format!(
                    "{} write skipped: {}",
                    R::LABEL,
                    error.diagnostic(DiagnosticStage::Cache).message
                ),
                retryable: true,
                stage: DiagnosticStage::Cache,
                file_path: Nullable(Some(source.path.to_string())),
                backend: Nullable(Some(source.backend.to_string())),
            },
        ));
    }

    pub(super) fn finish(mut self, metrics: &mut ExecutionMetrics) -> (Vec<R>, Vec<Diagnostic>) {
        self.flush();
        if self.deadline_exhausted {
            metrics.stage_deadline_exhausted_count = 1;
        }
        (nominal_values(self.receipts), nominal_values(self.warnings))
    }
}

pub(super) fn nominal_values<T>(mut values: Vec<(usize, T)>) -> Vec<T> {
    values.sort_by_key(|(index, _)| *index);
    values.into_iter().map(|(_, value)| value).collect()
}
