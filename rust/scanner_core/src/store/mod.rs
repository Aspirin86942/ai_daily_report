//! Rust-owned scanner inventory, successful parse cache, and run audit store.

mod artifacts;
mod lease;
mod retention;
mod runs;
#[cfg(test)]
mod tests;
mod types;

pub use artifacts::canonical_envelope_json;
use artifacts::*;
use lease::*;
use retention::*;
pub(crate) use runs::rebuild_envelope_from_metadata;
use types::*;
pub use types::{
    ActiveRun, AttemptRuntime, BeginRunOutcome, CanonicalRequest, ContextDecisionRecord,
    ContextRunRecord, DiagnosticSeverity, FinalizationBatch, RunDiagnosticRecord, SnapshotHit,
    SnapshotHitRef, StoreError, StoredEnvelope, TerminalAuditTimings, WorkerFingerprint,
};

pub mod cache;
pub mod inventory;
pub mod schema;

use ai_daily_discovery::normalize_contract_path_text;
use ai_daily_scanner_contract::{
    BuildContextRequest, CacheMissReason, ContextAction, ContextDecision, ContextEnvelope,
    ContextSummary, Diagnostic, DiagnosticStage, EngineStatus, ErrorCode, ExecutionMetricsV2,
    ExtensionMetric, NormalizedScannerSettings, Nullable, ParseStatus, RunStatus, StageMetric,
    StageName, Validate,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::artifact::{ArtifactDecisionRow, ArtifactDraft, ArtifactFileRow, SnapshotKeyParts};
use crate::deadline::{Clock, RunDeadlines};
use crate::identity::{engine_identity, EngineIdentity};

pub use cache::{
    classifier_profile_hash, parse_profile_hash, sha256_hex, CacheAwarePlanEntry, CacheEntry,
    CacheLookup, CacheWriteRecord, ClassificationCacheEntry, ClassificationCacheLookup,
    ClassificationCacheMissReason, ClassificationCacheWriteRecord, RouteStackFingerprint,
    RouteStackFingerprints, CLASSIFIER_PROFILE_HASH_ALGORITHM, PARSE_PROFILE_HASH_ALGORITHM,
};
pub use inventory::{FileResultRecord, InventoryRecord};
pub use schema::{BUSY_TIMEOUT_MS, LATEST_USER_VERSION};

pub const SCAN_DB_FILENAME: &str = "scan_index_rust_v3.sqlite3";
pub const REQUEST_HASH_ALGORITHM: &str = "sha256-request-v1";
pub const HEARTBEAT_INTERVAL_MS: u64 = 5_000;
pub const LEASE_GRACE_MS: u64 = 20_000;
const TERMINAL_WRITE_BATCH_SIZE: usize = 256;
const _: () = assert!(LEASE_GRACE_MS >= HEARTBEAT_INTERVAL_MS * 3);
const _: () = assert!(LEASE_GRACE_MS > BUSY_TIMEOUT_MS);
const CACHE_TRANSACTION_BATCH_SIZE: usize = 256;

#[derive(Debug)]
pub struct ScannerStore {
    connection: Connection,
    path: PathBuf,
}
impl ScannerStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        validate_database_path(path)?;
        let existed = path.is_file();
        if existed {
            require_existing_v3(path)?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut connection = Connection::open_with_flags(path, flags).map_err(cache_open)?;
        schema::configure_connection(&connection).map_err(schema_open)?;
        if existed {
            schema::require_v3(&connection).map_err(schema_open)?;
        } else {
            schema::create_v3(&mut connection).map_err(schema_open)?;
        }
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    pub fn open_existing(path: &Path) -> Result<Self, StoreError> {
        validate_database_path(path)?;
        require_existing_v3(path)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(path, flags).map_err(cache_open)?;
        schema::configure_connection(&connection).map_err(schema_open)?;
        schema::require_v3(&connection).map_err(schema_open)?;
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    /// Batch `last_accessed_bucket` update for parse/classification cache hits
    /// (spec Part 4): same row at most once/day, all hits in one transaction.
    pub fn touch_cache_access(
        &mut self,
        now_ms: u64,
        parse_hits: &[String],
        classification_hits: &[String],
    ) -> Result<(), StoreError> {
        self.touch_cache_access_impl(now_ms, parse_hits, classification_hits, None)
    }

    pub fn touch_cache_access_with_deadline(
        &mut self,
        now_ms: u64,
        parse_hits: &[String],
        classification_hits: &[String],
        deadlines: RunDeadlines,
        clock: &dyn Clock,
    ) -> Result<(), StoreError> {
        self.touch_cache_access_impl(
            now_ms,
            parse_hits,
            classification_hits,
            Some((deadlines, clock)),
        )
    }

    fn touch_cache_access_impl(
        &mut self,
        now_ms: u64,
        parse_hits: &[String],
        classification_hits: &[String],
        deadline: Option<(RunDeadlines, &dyn Clock)>,
    ) -> Result<(), StoreError> {
        let now_ms = checked_i64(now_ms, "cache touch timestamp")?;
        if parse_hits.is_empty() && classification_hits.is_empty() {
            return Ok(());
        }
        let ensure_work_deadline = || {
            if deadline
                .is_some_and(|(deadlines, clock)| deadlines.remaining_to_work_deadline(clock) == 0)
            {
                Err(StoreError::WorkDeadlineExhausted)
            } else {
                Ok(())
            }
        };
        ensure_work_deadline()?;
        if let Some((deadlines, clock)) = deadline {
            let remaining_ms = deadlines.remaining_to_work_deadline(clock);
            if remaining_ms == 0 {
                return Err(StoreError::WorkDeadlineExhausted);
            }
            self.connection
                .busy_timeout(Duration::from_millis(remaining_ms.min(BUSY_TIMEOUT_MS)))
                .map_err(cache_write)?;
        }
        let bucket = cache::date_bucket_for_ms(now_ms);
        let result = (|| -> Result<(), StoreError> {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(cache_write)?;
            cache::touch_parse_cache_access_with_check(&transaction, parse_hits, &bucket, &|| {
                ensure_work_deadline().is_ok()
            })
            .map_err(cache_mutation)?;
            cache::touch_classification_cache_access_with_check(
                &transaction,
                classification_hits,
                &bucket,
                &|| ensure_work_deadline().is_ok(),
            )
            .map_err(cache_mutation)?;
            ensure_work_deadline()?;
            transaction.commit().map_err(cache_write)
        })();
        let restore = if deadline.is_some() {
            restore_default_busy_timeout(&self.connection).map_err(cache_write)
        } else {
            Ok(())
        };
        match (result, restore) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(_)) => {
                eprintln!(
                    "scanner warning: SQLite busy timeout restore failed after cache touch commit"
                );
                Ok(())
            }
            (Err(error), Ok(())) => Err(error),
            (Err(_), Err(error)) => Err(StoreError::BusyTimeoutRestore {
                detail: error.to_string(),
            }),
        }
    }

    /// Opportunistic age/orphan row GC (spec Part 4): runs after the terminal
    /// COMMIT only when `remaining_to_absolute_deadline >= 10ms`. Independent
    /// transaction with `busy_timeout=0`, bounded indexed delete batches, and a
    /// 10ms admission budget checked before each statement. It only forms a
    /// freelist (no vacuum) and never rewrites the committed terminal result.
    pub fn run_opportunistic_gc(
        &mut self,
        now_ms: u64,
        budget_ms: u64,
        clock: &dyn Clock,
    ) -> Result<(), StoreError> {
        #[cfg(test)]
        TEST_OPPORTUNISTIC_GC_CALLS.with(|count| count.set(count.get().saturating_add(1)));
        let now_ms = checked_i64(now_ms, "opportunistic gc timestamp")?;
        if budget_ms == 0 {
            return Ok(());
        }
        let deadline_ms = clock.now_ms().saturating_add(budget_ms);
        let budget_open = || clock.now_ms() < deadline_ms;
        if !budget_open() {
            return Ok(());
        }
        self.connection
            .busy_timeout(Duration::from_millis(0))
            .map_err(cache_open)?;
        let outcome = (|| -> Result<(), StoreError> {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Deferred)
                .map_err(cache_write)?;
            // One bounded orphan-artifact delete batch.
            if budget_open() {
                transaction
                    .execute(
                        "DELETE FROM context_artifacts WHERE artifact_id IN (
                            SELECT artifact_id FROM context_artifacts
                            WHERE NOT EXISTS(
                                SELECT 1 FROM context_runs
                                WHERE context_runs.artifact_id = context_artifacts.artifact_id
                            )
                            ORDER BY created_at_ms ASC, artifact_id ASC
                            LIMIT 64
                         )",
                        [],
                    )
                    .map_err(cache_write)?;
                #[cfg(test)]
                TEST_OPPORTUNISTIC_GC_STATEMENTS_EXECUTED
                    .with(|count| count.set(count.get().saturating_add(1)));
            }
            // One bounded aged-terminal-run delete batch.
            let cutoff = now_ms.saturating_sub(cache::TERMINAL_RUN_MAX_AGE_DAYS * 86_400_000);
            if budget_open() {
                transaction
                    .execute(
                        "DELETE FROM scan_runs WHERE scan_run_id IN (
                            SELECT scan_run_id FROM scan_runs
                            WHERE status IN ('success', 'partial', 'error', 'abandoned')
                              AND finished_at_ms IS NOT NULL AND finished_at_ms < ?1
                            ORDER BY finished_at_ms ASC, scan_run_id ASC
                            LIMIT 64
                         )",
                        params![cutoff],
                    )
                    .map_err(cache_write)?;
                #[cfg(test)]
                TEST_OPPORTUNISTIC_GC_STATEMENTS_EXECUTED
                    .with(|count| count.set(count.get().saturating_add(1)));
            }
            // COMMIT is another non-preemptible statement. If admission has
            // expired, dropping this transaction rolls back optional GC only.
            if !budget_open() {
                return Ok(());
            }
            transaction.commit().map_err(cache_write)
        })();
        let restore = restore_default_busy_timeout(&self.connection);
        match (outcome, restore) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (_, Err(error)) => Err(StoreError::BusyTimeoutRestore {
                detail: error.to_string(),
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lookup_cache(
        &self,
        file_identity: &str,
        source_version: &str,
        source_guard_kind: &str,
        source_guard_sha256: &str,
        parse_profile_hash: &str,
        inventory_existed_before: bool,
    ) -> Result<CacheLookup, StoreError> {
        let key = cache::CacheLookupKey {
            file_identity,
            source_version,
            source_guard_kind,
            source_guard_sha256,
            parse_profile_hash,
            inventory_existed_before,
        };
        validate_cache_lookup_key(&key)?;
        let lookup = cache::lookup_cache(
            &self.connection,
            file_identity,
            source_version,
            source_guard_kind,
            source_guard_sha256,
            parse_profile_hash,
            inventory_existed_before,
        )
        .map_err(cache_open)?;
        reject_corrupt_parse_cache_lookup(lookup)
    }

    fn lookup_cache_batch(
        &self,
        keys: &[cache::CacheLookupKey<'_>],
    ) -> Result<Vec<CacheLookup>, StoreError> {
        for key in keys {
            validate_cache_lookup_key(key)?;
        }
        cache::lookup_cache_batch(&self.connection, keys)
            .map_err(cache_open)?
            .into_iter()
            .map(reject_corrupt_parse_cache_lookup)
            .collect()
    }

    /// Upserts the global `file_inventory` in one bounded short transaction and
    /// returns the set of file_identities that already existed before this round
    /// (spec Part 4 miss-reason tree step 4). Only a completed receipt opens
    /// cache lookup; a later terminal `finalize` re-upserts the same rows with
    /// the active scan_run_id (idempotent ON CONFLICT DO UPDATE).
    pub fn prepare_inventory(
        &mut self,
        records: &[InventoryRecord],
        scan_run_id: i64,
        now_ms: u64,
    ) -> Result<HashSet<String>, StoreError> {
        self.prepare_inventory_impl(records, scan_run_id, now_ms, None)
    }

    pub fn prepare_inventory_with_deadline(
        &mut self,
        records: &[InventoryRecord],
        scan_run_id: i64,
        now_ms: u64,
        deadlines: RunDeadlines,
        clock: &dyn Clock,
    ) -> Result<HashSet<String>, StoreError> {
        self.prepare_inventory_impl(records, scan_run_id, now_ms, Some((deadlines, clock)))
    }

    fn prepare_inventory_impl(
        &mut self,
        records: &[InventoryRecord],
        scan_run_id: i64,
        now_ms: u64,
        deadline: Option<(RunDeadlines, &dyn Clock)>,
    ) -> Result<HashSet<String>, StoreError> {
        let ensure_work_deadline = || {
            if deadline
                .is_some_and(|(deadlines, clock)| deadlines.remaining_to_work_deadline(clock) == 0)
            {
                Err(StoreError::WorkDeadlineExhausted)
            } else {
                Ok(())
            }
        };
        let now_ms = checked_i64(now_ms, "inventory timestamp")?;
        let mut unique = HashSet::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            if index % CACHE_TRANSACTION_BATCH_SIZE == 0 {
                ensure_work_deadline()?;
            }
            record.validate().map_err(StoreError::InvalidRequest)?;
            if !unique.insert(record.file_identity.as_str()) {
                return Err(StoreError::InvalidRequest(
                    "prepared inventory identities must be unique".to_string(),
                ));
            }
        }
        let mut existed = std::collections::HashSet::new();
        for (batch_index, chunk) in records.chunks(CACHE_TRANSACTION_BATCH_SIZE).enumerate() {
            let is_last_batch = (batch_index + 1) * CACHE_TRANSACTION_BATCH_SIZE >= records.len();
            ensure_work_deadline()?;
            if let Some((deadlines, clock)) = deadline {
                let remaining_ms = deadlines.remaining_to_work_deadline(clock);
                if remaining_ms == 0 {
                    return Err(StoreError::WorkDeadlineExhausted);
                }
                self.connection
                    .busy_timeout(Duration::from_millis(remaining_ms.min(BUSY_TIMEOUT_MS)))
                    .map_err(cache_write)?;
            }
            let batch_result = (|| {
                let transaction = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(cache_write)?;
                for record in chunk {
                    ensure_work_deadline()?;
                    let already: bool = transaction
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM file_inventory WHERE file_identity=?1)",
                            [&record.file_identity],
                            |row| row.get(0),
                        )
                        .map_err(cache_write)?;
                    if already {
                        existed.insert(record.file_identity.clone());
                    }
                }
                inventory::upsert_inventory(
                    &transaction,
                    scan_run_id,
                    now_ms,
                    chunk,
                    &ensure_work_deadline,
                )?;
                ensure_work_deadline()?;
                transaction.commit().map_err(cache_write)
            })();
            let restore_result = if deadline.is_some() {
                restore_default_busy_timeout(&self.connection).map_err(cache_write)
            } else {
                Ok(())
            };
            match (batch_result, restore_result) {
                (Ok(()), Ok(())) => {}
                (Ok(()), Err(_)) if is_last_batch => {
                    eprintln!(
                        "scanner warning: SQLite busy timeout restore failed after inventory commit"
                    );
                }
                (Err(error), Ok(())) => return Err(error),
                (_, Err(error)) => {
                    return Err(StoreError::BusyTimeoutRestore {
                        detail: error.to_string(),
                    })
                }
            }
        }
        Ok(existed)
    }

    /// Successful parse-cache write in an independent short transaction
    /// (spec Solution persistence boundary 2). Receipt-typed: the COMMIT is the
    /// linearization point.
    pub fn write_success_parse_cache(
        &mut self,
        records: &[CacheWriteRecord],
        cached_at_ms: u64,
    ) -> Result<(), StoreError> {
        self.write_success_parse_cache_impl(records, cached_at_ms, None)
    }

    pub fn write_success_parse_cache_with_deadline(
        &mut self,
        records: &[CacheWriteRecord],
        cached_at_ms: u64,
        deadlines: RunDeadlines,
        clock: &dyn Clock,
    ) -> Result<(), StoreError> {
        self.write_success_parse_cache_impl(records, cached_at_ms, Some((deadlines, clock)))
    }

    fn write_success_parse_cache_impl(
        &mut self,
        records: &[CacheWriteRecord],
        cached_at_ms: u64,
        deadline: Option<(RunDeadlines, &dyn Clock)>,
    ) -> Result<(), StoreError> {
        if deadline.is_some() && records.len() > CACHE_TRANSACTION_BATCH_SIZE {
            return Err(StoreError::InvalidRequest(format!(
                "parse cache write batch exceeds {CACHE_TRANSACTION_BATCH_SIZE} records"
            )));
        }
        let ensure_work_deadline = || {
            if deadline
                .is_some_and(|(deadlines, clock)| deadlines.remaining_to_work_deadline(clock) == 0)
            {
                Err(StoreError::WorkDeadlineExhausted)
            } else {
                Ok(())
            }
        };
        let cached_at_ms = checked_i64(cached_at_ms, "cache write timestamp")?;
        for (index, record) in records.iter().enumerate() {
            if index % CACHE_TRANSACTION_BATCH_SIZE == 0 {
                ensure_work_deadline()?;
            }
            record.validate().map_err(StoreError::InvalidRequest)?;
        }
        for chunk in records.chunks(CACHE_TRANSACTION_BATCH_SIZE) {
            ensure_work_deadline()?;
            if let Some((deadlines, clock)) = deadline {
                let remaining_ms = deadlines.remaining_to_work_deadline(clock);
                if remaining_ms == 0 {
                    return Err(StoreError::WorkDeadlineExhausted);
                }
                self.connection
                    .busy_timeout(Duration::from_millis(remaining_ms.min(BUSY_TIMEOUT_MS)))
                    .map_err(cache_write)?;
            }
            let batch_result = (|| {
                let transaction = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(cache_write)?;
                ensure_work_deadline()?;
                cache::write_success_cache_with_check(&transaction, cached_at_ms, chunk, &|| {
                    ensure_work_deadline().is_ok()
                })
                .map_err(cache_mutation)?;
                ensure_work_deadline()?;
                transaction.commit().map_err(cache_write)
            })();
            let restore_result = if deadline.is_some() {
                restore_default_busy_timeout(&self.connection).map_err(cache_write)
            } else {
                Ok(())
            };
            match (batch_result, restore_result) {
                (Ok(()), Ok(())) => {}
                (Ok(()), Err(_)) => {
                    // This deadline-aware API admits exactly one transaction
                    // batch, so the successful COMMIT is the complete receipt.
                    eprintln!(
                        "scanner warning: SQLite busy timeout restore failed after parse cache commit"
                    );
                }
                (Err(error), Ok(())) => return Err(error),
                (Err(_), Err(error)) => {
                    return Err(StoreError::BusyTimeoutRestore {
                        detail: error.to_string(),
                    })
                }
            }
        }
        Ok(())
    }

    /// Typed classification-cache lookup (spec Part 3.2). The miss-reason tree
    /// distinguishes `entry_absent_or_evicted` from `new_file` via
    /// `inventory_existed_before` (returned by `prepare_inventory`).
    // The cache identity is frozen and each key component remains explicit.
    #[allow(clippy::too_many_arguments)]
    pub fn lookup_classification_cache(
        &self,
        file_identity: &str,
        source_version: &str,
        source_guard_kind: &str,
        source_guard_sha256: &str,
        classifier_profile_hash: &str,
        classifier_build: &str,
        inventory_existed_before: bool,
    ) -> Result<ClassificationCacheLookup, StoreError> {
        if file_identity.is_empty()
            || source_version.is_empty()
            || inventory::parse_source_version(source_version).is_err()
            || !valid_guard_wire(source_guard_kind, source_guard_sha256)
            || !inventory::is_sha256(classifier_profile_hash)
            || !inventory::is_sha256(classifier_build)
        {
            return Err(StoreError::InvalidRequest(
                "classification cache lookup key is invalid".to_string(),
            ));
        }
        cache::lookup_classification_cache(
            &self.connection,
            file_identity,
            source_version,
            source_guard_kind,
            source_guard_sha256,
            classifier_profile_hash,
            classifier_build,
            inventory_existed_before,
        )
        .map_err(cache_lookup_read_error)
    }

    /// Success-only classification-cache write in an independent short
    /// transaction (spec Part 3.2: no negative cache).
    pub fn write_success_classification_cache(
        &mut self,
        records: &[ClassificationCacheWriteRecord],
        cached_at_ms: u64,
    ) -> Result<(), StoreError> {
        self.write_success_classification_cache_impl(records, cached_at_ms, None)
    }

    pub fn write_success_classification_cache_with_deadline(
        &mut self,
        records: &[ClassificationCacheWriteRecord],
        cached_at_ms: u64,
        deadlines: RunDeadlines,
        clock: &dyn Clock,
    ) -> Result<(), StoreError> {
        self.write_success_classification_cache_impl(
            records,
            cached_at_ms,
            Some((deadlines, clock)),
        )
    }

    fn write_success_classification_cache_impl(
        &mut self,
        records: &[ClassificationCacheWriteRecord],
        cached_at_ms: u64,
        deadline: Option<(RunDeadlines, &dyn Clock)>,
    ) -> Result<(), StoreError> {
        if deadline.is_some() && records.len() > CACHE_TRANSACTION_BATCH_SIZE {
            return Err(StoreError::InvalidRequest(format!(
                "classification cache write batch exceeds {CACHE_TRANSACTION_BATCH_SIZE} records"
            )));
        }
        let ensure_work_deadline = || {
            if deadline
                .is_some_and(|(deadlines, clock)| deadlines.remaining_to_work_deadline(clock) == 0)
            {
                Err(StoreError::WorkDeadlineExhausted)
            } else {
                Ok(())
            }
        };
        let cached_at_ms = checked_i64(cached_at_ms, "classification cache write timestamp")?;
        for (index, record) in records.iter().enumerate() {
            if index % CACHE_TRANSACTION_BATCH_SIZE == 0 {
                ensure_work_deadline()?;
            }
            record.validate().map_err(StoreError::InvalidRequest)?;
        }
        for chunk in records.chunks(CACHE_TRANSACTION_BATCH_SIZE) {
            ensure_work_deadline()?;
            if let Some((deadlines, clock)) = deadline {
                let remaining_ms = deadlines.remaining_to_work_deadline(clock);
                if remaining_ms == 0 {
                    return Err(StoreError::WorkDeadlineExhausted);
                }
                self.connection
                    .busy_timeout(Duration::from_millis(remaining_ms.min(BUSY_TIMEOUT_MS)))
                    .map_err(cache_write)?;
            }
            let batch_result = (|| {
                let transaction = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(cache_write)?;
                ensure_work_deadline()?;
                cache::write_success_classification_cache_with_check(
                    &transaction,
                    cached_at_ms,
                    chunk,
                    &|| ensure_work_deadline().is_ok(),
                )
                .map_err(cache_mutation)?;
                ensure_work_deadline()?;
                transaction.commit().map_err(cache_write)
            })();
            let restore_result = if deadline.is_some() {
                restore_default_busy_timeout(&self.connection).map_err(cache_write)
            } else {
                Ok(())
            };
            match (batch_result, restore_result) {
                (Ok(()), Ok(())) => {}
                (Ok(()), Err(_)) => {
                    eprintln!(
                        "scanner warning: SQLite busy timeout restore failed after classification cache commit"
                    );
                }
                (Err(error), Ok(())) => return Err(error),
                (Err(_), Err(error)) => {
                    return Err(StoreError::BusyTimeoutRestore {
                        detail: error.to_string(),
                    })
                }
            }
        }
        Ok(())
    }

    pub fn attach_cache_evidence(
        &self,
        planned_files: Vec<crate::planner::PlannedFile>,
        profile: &NormalizedScannerSettings,
        route_stacks: &RouteStackFingerprints,
    ) -> Result<Vec<CacheAwarePlanEntry>, StoreError> {
        let mut entries = planned_files
            .into_iter()
            .map(|planned| match planned.action {
                crate::planner::PlanAction::Reject(_) => Ok(CacheAwarePlanEntry {
                    planned,
                    parse_profile_hash: None,
                    cache_lookup: None,
                }),
                crate::planner::PlanAction::Parse(route) => {
                    let profile_hash =
                        parse_profile_hash(1, route_stacks.for_route(route), profile)
                            .map_err(StoreError::InvalidRequest)?;
                    Ok(CacheAwarePlanEntry {
                        planned,
                        parse_profile_hash: Some(profile_hash),
                        cache_lookup: None,
                    })
                }
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let zero_guard = "0".repeat(64);
        let keys = entries
            .iter()
            .filter_map(|entry| {
                entry
                    .parse_profile_hash
                    .as_deref()
                    .map(|parse_profile_hash| cache::CacheLookupKey {
                        file_identity: &entry.planned.file.file_identity,
                        source_version: &entry.planned.file.source_version,
                        source_guard_kind: "content_sha256_v1",
                        source_guard_sha256: &zero_guard,
                        parse_profile_hash,
                        inventory_existed_before: false,
                    })
            })
            .collect::<Vec<_>>();
        let mut lookups = self.lookup_cache_batch(&keys)?.into_iter();
        for entry in &mut entries {
            if entry.parse_profile_hash.is_some() {
                entry.cache_lookup = Some(
                    lookups
                        .next()
                        .ok_or_else(|| cache_open("batch cache lookup count mismatch"))?,
                );
            }
        }
        if lookups.next().is_some() {
            return Err(cache_open("batch cache lookup count mismatch"));
        }
        Ok(entries)
    }
}

fn validate_database_path(path: &Path) -> Result<(), StoreError> {
    if !path.is_absolute()
        || path.file_name().and_then(|value| value.to_str()) != Some(SCAN_DB_FILENAME)
        || !path.parent().is_some_and(Path::is_dir)
    {
        return Err(StoreError::InvalidRequest(format!(
            "scan_db_path must be an absolute path ending in {SCAN_DB_FILENAME} with an existing parent"
        )));
    }
    Ok(())
}

fn require_existing_v3(path: &Path) -> Result<(), StoreError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(path, flags).map_err(cache_open)?;
    schema::require_v3(&connection).map_err(schema_open)
}

fn schema_open(error: schema::SchemaError) -> StoreError {
    match error {
        schema::SchemaError::Mismatch(found) => StoreError::SchemaMismatch { found },
        schema::SchemaError::LayoutMismatch => StoreError::SchemaLayoutMismatch,
        schema::SchemaError::Sql(error) => cache_open(error),
    }
}

fn checked_i64(value: u64, field: &str) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::InvalidRequest(format!("{field} exceeds SQLite integer range")))
}

/// SourceGuardV2 wire for the parse-cache key: only an available kind with a
/// 64-char lowercase-hex SHA-256 (unavailable files never reach the cache).
fn valid_guard_wire(kind: &str, hash: &str) -> bool {
    matches!(
        kind,
        "windows_file_id_change_time_v1" | "unix_inode_ctime_v1" | "content_sha256_v1"
    ) && inventory::is_sha256(hash)
}

fn validate_cache_lookup_key(key: &cache::CacheLookupKey<'_>) -> Result<(), StoreError> {
    if key.file_identity.is_empty()
        || key.source_version.is_empty()
        || inventory::parse_source_version(key.source_version).is_err()
        || !valid_guard_wire(key.source_guard_kind, key.source_guard_sha256)
        || !inventory::is_sha256(key.parse_profile_hash)
    {
        return Err(StoreError::InvalidRequest(
            "cache lookup key is invalid".to_string(),
        ));
    }
    Ok(())
}

fn now_millis() -> Result<u64, StoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|_| StoreError::InvalidRequest("system clock is invalid".to_string()))
}

pub fn current_time_millis() -> Result<u64, StoreError> {
    now_millis()
}

fn cache_open(error: impl ToString) -> StoreError {
    StoreError::CacheOpen {
        detail: error.to_string(),
    }
}

fn cache_lookup_read_error(error: cache::CacheLookupReadError) -> StoreError {
    match error {
        cache::CacheLookupReadError::Sqlite(error) => cache_open(error),
        cache::CacheLookupReadError::Corrupt(message) => {
            StoreError::RunCorrupt(message.to_string())
        }
    }
}

fn reject_corrupt_parse_cache_lookup(lookup: CacheLookup) -> Result<CacheLookup, StoreError> {
    if matches!(lookup, CacheLookup::Miss(CacheMissReason::ErrorCache)) {
        return Err(StoreError::RunCorrupt(
            "exact parse cache row failed integrity validation".to_string(),
        ));
    }
    Ok(lookup)
}

fn cache_write(error: impl ToString) -> StoreError {
    StoreError::CacheWrite {
        detail: error.to_string(),
    }
}

fn cache_mutation(error: cache::CacheMutationError) -> StoreError {
    match error {
        cache::CacheMutationError::Sqlite(error) => cache_write(error),
        cache::CacheMutationError::DeadlineExhausted => StoreError::WorkDeadlineExhausted,
    }
}

fn restore_default_busy_timeout(connection: &Connection) -> rusqlite::Result<()> {
    #[cfg(test)]
    if TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.get()) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
}

#[cfg(test)]
thread_local! {
    static TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static TEST_FORCE_INCREMENTAL_VACUUM_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static TEST_TERMINAL_DIAGNOSTIC_BATCHES_WRITTEN: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static TEST_OPPORTUNISTIC_GC_CALLS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static TEST_OPPORTUNISTIC_GC_STATEMENTS_EXECUTED: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_test_opportunistic_gc_calls() -> usize {
    TEST_OPPORTUNISTIC_GC_CALLS.with(|count| count.replace(0))
}

// ---------------------------------------------------------------------------
// context artifact write path + snapshot finalization（spec Part 4/5.1/5.2）
// ---------------------------------------------------------------------------
