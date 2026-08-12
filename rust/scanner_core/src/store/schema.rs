//! Fresh-only scanner database schema.
//!
//! A missing database is created atomically at user_version=3. Existing files
//! are accepted only when they already report version 3; no migration or
//! compatibility amendment exists in the runtime.

use rusqlite::{Connection, TransactionBehavior};
use std::time::Duration;
use thiserror::Error;

pub const LATEST_USER_VERSION: i32 = 3;
pub const BUSY_TIMEOUT_MS: u64 = 2_000;

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("scanner database schema mismatch: expected user_version=3, found {0}")]
    Mismatch(i32),
    #[error("scanner database schema initialization failed")]
    Sql(#[from] rusqlite::Error),
}

pub const V3_DDL: &str = r#"
CREATE TABLE scan_runs (
    scan_run_id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT NOT NULL UNIQUE,
    canonical_request_json TEXT NOT NULL,
    request_hash_algorithm TEXT NOT NULL CHECK (request_hash_algorithm = 'sha256-request-v1'),
    request_hash TEXT NOT NULL CHECK (length(request_hash) = 64),
    owner_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'success', 'partial', 'error', 'abandoned')),
    created_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER,
    final_envelope_json TEXT,
    final_envelope_metadata_json TEXT,
    audit_size_bytes INTEGER,
    CHECK (
        (status IN ('running', 'abandoned') AND final_envelope_json IS NULL)
        OR
        (status IN ('success', 'partial', 'error') AND final_envelope_json IS NOT NULL)
    ),
    CHECK (
        (status = 'running' AND finished_at_ms IS NULL)
        OR
        (status <> 'running' AND finished_at_ms IS NOT NULL)
    )
) STRICT;

CREATE TABLE engine_lease (
    lease_key INTEGER PRIMARY KEY CHECK (lease_key = 1),
    owner_id TEXT NOT NULL UNIQUE,
    owner_pid INTEGER NOT NULL,
    acquired_at_ms INTEGER NOT NULL,
    heartbeat_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    CHECK (heartbeat_at_ms >= acquired_at_ms),
    CHECK (expires_at_ms > heartbeat_at_ms)
) STRICT;

CREATE TABLE scan_run_attempts (
    scan_run_id INTEGER NOT NULL REFERENCES scan_runs(scan_run_id) ON DELETE CASCADE,
    attempt_number INTEGER NOT NULL CHECK (attempt_number >= 1),
    owner_id TEXT NOT NULL,
    normalized_scan_db_path TEXT NOT NULL,
    normalized_office_worker_path TEXT NOT NULL,
    normalized_python_executable TEXT NOT NULL,
    normalized_python_module_root TEXT NOT NULL,
    python_document_worker_module TEXT NOT NULL,
    engine_fingerprint TEXT NOT NULL,
    office_worker_contract TEXT,
    office_worker_version TEXT,
    office_worker_build TEXT,
    python_worker_contract TEXT,
    python_worker_version TEXT,
    python_worker_build TEXT,
    started_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER,
    status TEXT NOT NULL CHECK (status IN ('running', 'success', 'partial', 'error', 'abandoned')),
    CHECK (
        status NOT IN ('success', 'partial')
        OR (
            office_worker_contract IS NOT NULL
            AND office_worker_version IS NOT NULL
            AND office_worker_build IS NOT NULL
            AND python_worker_contract IS NOT NULL
            AND python_worker_version IS NOT NULL
            AND python_worker_build IS NOT NULL
        )
    ),
    PRIMARY KEY (scan_run_id, attempt_number)
) STRICT;

CREATE TABLE run_diagnostics (
    scan_run_id INTEGER NOT NULL REFERENCES scan_runs(scan_run_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    severity TEXT NOT NULL CHECK (severity IN ('warning', 'error')),
    error_code TEXT NOT NULL,
    message TEXT NOT NULL,
    retryable INTEGER NOT NULL CHECK (retryable IN (0, 1)),
    stage TEXT NOT NULL,
    file_path TEXT,
    backend TEXT,
    PRIMARY KEY (scan_run_id, sequence)
) STRICT;

CREATE TABLE file_inventory (
    file_identity TEXT PRIMARY KEY,
    absolute_path TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    file_type TEXT NOT NULL,
    source_version TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    mtime_ns INTEGER NOT NULL CHECK (mtime_ns >= 0),
    last_seen_run_id INTEGER REFERENCES scan_runs(scan_run_id) ON DELETE SET NULL,
    last_seen_at_ms INTEGER NOT NULL,
    source_guard_kind TEXT,
    source_guard_sha256 TEXT,
    CHECK (
        (source_guard_kind IS NULL AND source_guard_sha256 IS NULL)
        OR
        (source_guard_kind = 'unavailable' AND source_guard_sha256 IS NULL)
        OR
        (source_guard_kind IN ('windows_file_id_change_time_v1', 'unix_inode_ctime_v1', 'content_sha256_v1')
         AND source_guard_sha256 IS NOT NULL)
    ),
    CHECK (source_guard_sha256 IS NULL OR length(source_guard_sha256) = 64)
) STRICT;

CREATE TABLE parse_cache (
    file_identity TEXT NOT NULL REFERENCES file_inventory(file_identity) ON DELETE CASCADE,
    source_version TEXT NOT NULL,
    source_guard_kind TEXT NOT NULL CHECK (source_guard_kind IN ('windows_file_id_change_time_v1', 'unix_inode_ctime_v1', 'content_sha256_v1')),
    source_guard_sha256 TEXT NOT NULL CHECK (length(source_guard_sha256) = 64),
    parse_profile_hash TEXT NOT NULL CHECK (length(parse_profile_hash) = 64),
    content TEXT NOT NULL,
    content_sha256 TEXT NOT NULL CHECK (length(content_sha256) = 64),
    parser_backend TEXT NOT NULL,
    worker_lane TEXT NOT NULL,
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    worker_contract_version TEXT NOT NULL,
    worker_version TEXT NOT NULL,
    worker_build TEXT NOT NULL,
    cached_at_ms INTEGER NOT NULL,
    entry_size_bytes INTEGER NOT NULL CHECK (entry_size_bytes >= 0),
    generation_rank INTEGER NOT NULL DEFAULT 1 CHECK (generation_rank IN (0, 1)),
    last_accessed_bucket TEXT NOT NULL,
    PRIMARY KEY (file_identity, source_version, source_guard_kind, source_guard_sha256, parse_profile_hash)
) STRICT;

CREATE TABLE classification_cache (
    file_identity TEXT NOT NULL REFERENCES file_inventory(file_identity) ON DELETE CASCADE,
    source_version TEXT NOT NULL,
    source_guard_kind TEXT NOT NULL CHECK (source_guard_kind IN ('windows_file_id_change_time_v1', 'unix_inode_ctime_v1', 'content_sha256_v1')),
    source_guard_sha256 TEXT NOT NULL CHECK (length(source_guard_sha256) = 64),
    classifier_profile_hash TEXT NOT NULL CHECK (length(classifier_profile_hash) = 64),
    classifier_build TEXT NOT NULL CHECK (length(classifier_build) = 64),
    status TEXT NOT NULL CHECK (status IN ('text_in_parse_window', 'no_text_in_parse_window')),
    page_count INTEGER NOT NULL CHECK (page_count >= 0),
    result_examined_pages INTEGER NOT NULL CHECK (result_examined_pages >= 0),
    cached_at_ms INTEGER NOT NULL,
    entry_size_bytes INTEGER NOT NULL CHECK (entry_size_bytes >= 0),
    generation_rank INTEGER NOT NULL DEFAULT 1 CHECK (generation_rank IN (0, 1)),
    last_accessed_bucket TEXT NOT NULL,
    PRIMARY KEY (file_identity, source_version, source_guard_kind, source_guard_sha256, classifier_profile_hash, classifier_build)
) STRICT;

CREATE TABLE scan_file_results (
    scan_run_id INTEGER NOT NULL REFERENCES scan_runs(scan_run_id) ON DELETE CASCADE,
    file_identity TEXT NOT NULL REFERENCES file_inventory(file_identity),
    relative_path TEXT NOT NULL,
    source_version TEXT NOT NULL,
    parse_profile_hash TEXT NOT NULL CHECK (length(parse_profile_hash) = 64),
    cache_status TEXT NOT NULL CHECK (cache_status IN ('fresh', 'miss')),
    cache_miss_reason TEXT NOT NULL CHECK (cache_miss_reason IN ('', 'new_file', 'error_cache', 'source_version_changed', 'parser_profile_changed', 'parser_identity_changed', 'entry_absent_or_evicted')),
    parse_status TEXT NOT NULL CHECK (parse_status IN ('success', 'error', 'timeout', 'not_parsed')),
    parser_backend TEXT NOT NULL,
    worker_lane TEXT NOT NULL,
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    content_sha256 TEXT NOT NULL CHECK (length(content_sha256) = 64),
    primary_duration_ms INTEGER NOT NULL CHECK (primary_duration_ms >= 0),
    fallback_duration_ms INTEGER NOT NULL CHECK (fallback_duration_ms >= 0),
    parse_duration_ms INTEGER NOT NULL CHECK (parse_duration_ms >= 0),
    failure_class TEXT NOT NULL,
    fallback_backend TEXT NOT NULL,
    fallback_reason_code TEXT NOT NULL,
    error_code TEXT,
    error_message TEXT,
    error_retryable INTEGER CHECK (error_retryable IN (0, 1)),
    error_stage TEXT,
    error_file_path TEXT,
    error_backend TEXT,
    parse_cache_status TEXT CHECK (parse_cache_status IN ('fresh', 'miss', 'snapshot', 'not_applicable')),
    PRIMARY KEY (scan_run_id, file_identity)
) STRICT;

CREATE TABLE scan_file_execution (
    scan_run_id INTEGER NOT NULL,
    file_identity TEXT NOT NULL,
    parse_transport TEXT NOT NULL CHECK (parse_transport IN ('session', 'rust_in_process', 'snapshot', 'not_applicable')),
    parse_attempt_count INTEGER NOT NULL CHECK (parse_attempt_count BETWEEN 0 AND 3),
    classification_status TEXT CHECK (classification_status IN ('text_in_parse_window', 'no_text_in_parse_window', 'not_classified_by_budget', 'unknown', 'error')),
    classification_page_count INTEGER CHECK (classification_page_count IS NULL OR classification_page_count >= 0),
    classification_cache_status TEXT CHECK (classification_cache_status IN ('fresh', 'miss', 'snapshot', 'not_eligible')),
    classification_cache_miss_reason TEXT,
    classification_result_examined_pages INTEGER CHECK (classification_result_examined_pages IS NULL OR classification_result_examined_pages >= 0),
    classification_run_inspected_pages INTEGER CHECK (classification_run_inspected_pages IS NULL OR classification_run_inspected_pages >= 0),
    classification_nominal_charged_pages INTEGER CHECK (classification_nominal_charged_pages IS NULL OR classification_nominal_charged_pages >= 0),
    classification_duration_ms INTEGER CHECK (classification_duration_ms IS NULL OR classification_duration_ms >= 0),
    classification_transport TEXT CHECK (classification_transport IN ('session', 'snapshot', 'not_applicable')),
    classification_attempt_count INTEGER CHECK (classification_attempt_count IS NULL OR classification_attempt_count BETWEEN 0 AND 3),
    classifier_build TEXT CHECK (classifier_build IS NULL OR length(classifier_build) = 64),
    classifier_profile_hash TEXT CHECK (classifier_profile_hash IS NULL OR length(classifier_profile_hash) = 64),
    PRIMARY KEY (scan_run_id, file_identity),
    FOREIGN KEY (scan_run_id, file_identity) REFERENCES scan_file_results(scan_run_id, file_identity) ON DELETE CASCADE
) STRICT;

CREATE TABLE scan_stage_metrics (
    scan_run_id INTEGER NOT NULL REFERENCES scan_runs(scan_run_id) ON DELETE CASCADE,
    stage TEXT NOT NULL CHECK (stage IN ('discovery', 'cache', 'parse', 'context')),
    item_count INTEGER NOT NULL CHECK (item_count >= 0),
    duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
    PRIMARY KEY (scan_run_id, stage)
) STRICT;

CREATE TABLE scan_extension_metrics (
    scan_run_id INTEGER NOT NULL REFERENCES scan_runs(scan_run_id) ON DELETE CASCADE,
    extension TEXT NOT NULL,
    file_count INTEGER NOT NULL CHECK (file_count >= 0),
    parse_duration_ms INTEGER NOT NULL CHECK (parse_duration_ms >= 0),
    success_count INTEGER NOT NULL CHECK (success_count >= 0),
    error_count INTEGER NOT NULL CHECK (error_count >= 0),
    timeout_count INTEGER NOT NULL CHECK (timeout_count >= 0),
    PRIMARY KEY (scan_run_id, extension)
) STRICT;

CREATE TABLE context_artifacts (
    artifact_id INTEGER PRIMARY KEY AUTOINCREMENT,
    snapshot_eligible INTEGER NOT NULL CHECK (snapshot_eligible IN (0, 1)),
    snapshot_key_sha256 TEXT,
    snapshot_key_json TEXT,
    final_context TEXT NOT NULL,
    context_sha256 TEXT NOT NULL CHECK (length(context_sha256) = 64),
    semantic_summary_json TEXT NOT NULL,
    artifact_size_bytes INTEGER NOT NULL CHECK (artifact_size_bytes >= 0),
    created_at_ms INTEGER NOT NULL,
    last_accessed_bucket TEXT NOT NULL,
    CHECK (
        (snapshot_eligible = 1 AND snapshot_key_sha256 IS NOT NULL AND snapshot_key_json IS NOT NULL)
        OR
        (snapshot_eligible = 0 AND snapshot_key_sha256 IS NULL AND snapshot_key_json IS NULL)
    )
) STRICT;

CREATE TABLE context_artifact_files (
    artifact_id INTEGER NOT NULL REFERENCES context_artifacts(artifact_id) ON DELETE CASCADE,
    file_identity TEXT NOT NULL REFERENCES file_inventory(file_identity) ON DELETE RESTRICT,
    relative_path TEXT NOT NULL,
    source_version TEXT NOT NULL,
    source_guard_kind TEXT,
    source_guard_sha256 TEXT,
    parse_profile_hash TEXT NOT NULL CHECK (length(parse_profile_hash) = 64),
    parse_status TEXT NOT NULL CHECK (parse_status IN ('success', 'error', 'timeout', 'not_parsed')),
    parser_backend TEXT NOT NULL,
    worker_lane TEXT NOT NULL,
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    content_sha256 TEXT NOT NULL CHECK (length(content_sha256) = 64),
    classifier_status TEXT,
    classifier_page_count INTEGER CHECK (classifier_page_count IS NULL OR classifier_page_count >= 0),
    classifier_result_examined_pages INTEGER CHECK (classifier_result_examined_pages IS NULL OR classifier_result_examined_pages >= 0),
    classifier_nominal_charged_pages INTEGER CHECK (classifier_nominal_charged_pages IS NULL OR classifier_nominal_charged_pages >= 0),
    classifier_build TEXT CHECK (classifier_build IS NULL OR length(classifier_build) = 64),
    classifier_profile_hash TEXT CHECK (classifier_profile_hash IS NULL OR length(classifier_profile_hash) = 64),
    PRIMARY KEY (artifact_id, file_identity)
) STRICT;

CREATE TABLE context_artifact_decisions (
    artifact_id INTEGER NOT NULL,
    file_identity TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('keep', 'compress', 'metadata_only', 'omit', 'error')),
    reason TEXT NOT NULL,
    priority INTEGER NOT NULL CHECK (priority >= 0),
    input_chars INTEGER NOT NULL CHECK (input_chars >= 0),
    output_chars INTEGER NOT NULL CHECK (output_chars >= 0),
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    error_code TEXT NOT NULL,
    PRIMARY KEY (artifact_id, file_identity),
    FOREIGN KEY (artifact_id, file_identity) REFERENCES context_artifact_files(artifact_id, file_identity) ON DELETE CASCADE
) STRICT;

CREATE TABLE context_runs (
    context_run_id INTEGER PRIMARY KEY,
    scan_run_id INTEGER NOT NULL UNIQUE REFERENCES scan_runs(scan_run_id) ON DELETE CASCADE,
    context_profile_hash TEXT NOT NULL CHECK (length(context_profile_hash) = 64),
    status TEXT NOT NULL CHECK (status IN ('success', 'partial', 'error')),
    final_context TEXT NOT NULL,
    context_sha256 TEXT NOT NULL CHECK (length(context_sha256) = 64),
    source_file_count INTEGER NOT NULL CHECK (source_file_count >= 0),
    success_count INTEGER NOT NULL CHECK (success_count >= 0),
    timeout_count INTEGER NOT NULL CHECK (timeout_count >= 0),
    included_file_count INTEGER NOT NULL CHECK (included_file_count >= 0),
    omitted_file_count INTEGER NOT NULL CHECK (omitted_file_count >= 0),
    error_file_count INTEGER NOT NULL CHECK (error_file_count >= 0),
    input_chars INTEGER NOT NULL CHECK (input_chars >= 0),
    output_chars INTEGER NOT NULL CHECK (output_chars >= 0),
    total_duration_ms INTEGER NOT NULL CHECK (total_duration_ms >= 0),
    discovery_duration_ms INTEGER NOT NULL CHECK (discovery_duration_ms >= 0),
    parse_duration_ms INTEGER NOT NULL CHECK (parse_duration_ms >= 0),
    compression_duration_ms INTEGER NOT NULL CHECK (compression_duration_ms >= 0),
    created_at_ms INTEGER NOT NULL,
    artifact_id INTEGER REFERENCES context_artifacts(artifact_id) ON DELETE RESTRICT,
    reused_from_context_run_id INTEGER REFERENCES context_runs(context_run_id) ON DELETE SET NULL,
    snapshot_hit INTEGER NOT NULL DEFAULT 0 CHECK (snapshot_hit IN (0, 1)),
    CHECK (
        snapshot_hit = 1 OR reused_from_context_run_id IS NULL
    ),
    CHECK (
        (status IN ('success', 'partial') AND artifact_id IS NOT NULL)
        OR
        (status = 'error' AND artifact_id IS NULL)
    )
) STRICT;

CREATE TABLE context_decisions (
    context_run_id INTEGER NOT NULL REFERENCES context_runs(context_run_id) ON DELETE CASCADE,
    file_identity TEXT NOT NULL REFERENCES file_inventory(file_identity),
    relative_path TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('keep', 'compress', 'metadata_only', 'omit', 'error')),
    reason TEXT NOT NULL,
    priority INTEGER NOT NULL CHECK (priority >= 0),
    input_chars INTEGER NOT NULL CHECK (input_chars >= 0),
    output_chars INTEGER NOT NULL CHECK (output_chars >= 0),
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    error_code TEXT NOT NULL,
    PRIMARY KEY (context_run_id, file_identity)
) STRICT;

CREATE TABLE scan_execution_metrics (
    scan_run_id INTEGER NOT NULL REFERENCES scan_runs(scan_run_id) ON DELETE CASCADE,
    discovery_observed_file_count INTEGER NOT NULL CHECK (discovery_observed_file_count >= 0),
    source_guard_content_hash_file_count INTEGER NOT NULL CHECK (source_guard_content_hash_file_count >= 0),
    source_guard_unavailable_count INTEGER NOT NULL CHECK (source_guard_unavailable_count >= 0),
    source_guard_bytes_read INTEGER NOT NULL CHECK (source_guard_bytes_read >= 0),
    candidate_file_count INTEGER NOT NULL CHECK (candidate_file_count >= 0),
    admitted_file_count INTEGER NOT NULL CHECK (admitted_file_count >= 0),
    classification_slot_count INTEGER NOT NULL CHECK (classification_slot_count >= 0),
    confirmed_run_inspected_pages_total INTEGER NOT NULL CHECK (confirmed_run_inspected_pages_total >= 0),
    unobserved_classification_attempt_count INTEGER NOT NULL CHECK (unobserved_classification_attempt_count >= 0),
    nominal_charged_pages_total INTEGER NOT NULL CHECK (nominal_charged_pages_total >= 0),
    extraction_slot_count INTEGER NOT NULL CHECK (extraction_slot_count >= 0),
    pdfplumber_invocations INTEGER NOT NULL CHECK (pdfplumber_invocations >= 0),
    snapshot_hit INTEGER NOT NULL CHECK (snapshot_hit IN (0, 1)),
    parse_cache_lookup_count INTEGER NOT NULL CHECK (parse_cache_lookup_count >= 0),
    classification_cache_lookup_count INTEGER NOT NULL CHECK (classification_cache_lookup_count >= 0),
    parse_cache_all_hit INTEGER CHECK (parse_cache_all_hit IN (0, 1) OR parse_cache_all_hit IS NULL),
    classification_cache_all_hit INTEGER CHECK (classification_cache_all_hit IN (0, 1) OR classification_cache_all_hit IS NULL),
    stage_deadline_exhausted_count INTEGER NOT NULL CHECK (stage_deadline_exhausted_count IN (0, 1)),
    session_restart_count INTEGER NOT NULL CHECK (session_restart_count >= 0),
    session_fallback_count INTEGER NOT NULL CHECK (session_fallback_count >= 0),
    classify_attempt_count INTEGER NOT NULL CHECK (classify_attempt_count >= 0),
    parse_attempt_count INTEGER NOT NULL CHECK (parse_attempt_count >= 0),
    reserved_chars INTEGER NOT NULL CHECK (reserved_chars >= 0),
    rendered_chars INTEGER NOT NULL CHECK (rendered_chars >= 0),
    worker_handshake_ms INTEGER NOT NULL CHECK (worker_handshake_ms >= 0),
    discovery_ms INTEGER NOT NULL CHECK (discovery_ms >= 0),
    snapshot_lookup_ms INTEGER NOT NULL CHECK (snapshot_lookup_ms >= 0),
    current_run_audit_write_ms INTEGER NOT NULL CHECK (current_run_audit_write_ms >= 0),
    terminal_precommit_ms INTEGER NOT NULL CHECK (terminal_precommit_ms >= 0),
    deadline_precommit_elapsed_ms INTEGER NOT NULL CHECK (deadline_precommit_elapsed_ms >= 0),
    envelope_rebuild_ms INTEGER NOT NULL CHECK (envelope_rebuild_ms >= 0),
    terminal_rows_written INTEGER NOT NULL CHECK (terminal_rows_written >= 0),
    peak_worker_rss_bytes INTEGER CHECK (peak_worker_rss_bytes IS NULL OR peak_worker_rss_bytes >= 0),
    PRIMARY KEY (scan_run_id)
) STRICT;


CREATE INDEX idx_scan_runs_status_updated ON scan_runs(status, updated_at_ms);
CREATE INDEX idx_scan_runs_started ON scan_runs(started_at_ms);
CREATE INDEX idx_scan_runs_finished ON scan_runs(finished_at_ms);
CREATE INDEX idx_attempts_owner_status ON scan_run_attempts(owner_id, status);
CREATE INDEX idx_diagnostics_code ON run_diagnostics(error_code);
CREATE INDEX idx_inventory_last_seen ON file_inventory(last_seen_run_id);
CREATE INDEX idx_cache_created ON parse_cache(cached_at_ms);
CREATE INDEX idx_classification_cache_cached ON classification_cache(cached_at_ms);
CREATE INDEX idx_file_results_identity ON scan_file_results(file_identity);
CREATE INDEX idx_file_results_status ON scan_file_results(parse_status);
CREATE INDEX idx_context_runs_artifact ON context_runs(artifact_id);
CREATE INDEX idx_context_runs_reused ON context_runs(reused_from_context_run_id);
CREATE INDEX idx_artifact_files_file ON context_artifact_files(file_identity);
CREATE INDEX idx_artifact_decisions_file ON context_artifact_decisions(file_identity);
CREATE INDEX idx_context_decisions_file ON context_decisions(file_identity);
CREATE UNIQUE INDEX idx_artifacts_snapshot_key ON context_artifacts(snapshot_key_sha256) WHERE snapshot_eligible = 1;
"#;

pub fn configure_connection(connection: &Connection) -> Result<(), SchemaError> {
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    let _: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

pub fn require_durable_finalization(connection: &Connection) -> Result<(), SchemaError> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

pub fn user_version(connection: &Connection) -> Result<i32, SchemaError> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

pub fn require_v3(connection: &Connection) -> Result<(), SchemaError> {
    let version = user_version(connection)?;
    if version == LATEST_USER_VERSION {
        Ok(())
    } else {
        Err(SchemaError::Mismatch(version))
    }
}

pub fn create_v3(connection: &mut Connection) -> Result<(), SchemaError> {
    let version = user_version(connection)?;
    if version != 0 {
        return Err(SchemaError::Mismatch(version));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(V3_DDL)?;
    transaction.pragma_update(None, "user_version", LATEST_USER_VERSION)?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fresh_database_is_created_at_v3_and_reopens() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("scan_index_v3.sqlite3");
        let mut connection = Connection::open(&path).expect("database opens");
        configure_connection(&connection).expect("pragmas");
        create_v3(&mut connection).expect("v3 schema");
        assert_eq!(user_version(&connection).unwrap(), 3);
        require_v3(&connection).expect("v3 reopens");

        let one_shot_tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE sql LIKE '%one_shot%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(one_shot_tables, 0);
    }

    #[test]
    fn every_non_v3_version_is_rejected() {
        let directory = tempdir().expect("temporary directory");
        for version in [0, 1, 2, 4] {
            let path = directory.path().join(format!("version-{version}.sqlite3"));
            let connection = Connection::open(path).expect("database opens");
            connection
                .pragma_update(None, "user_version", version)
                .expect("seed version");
            assert!(
                matches!(require_v3(&connection), Err(SchemaError::Mismatch(found)) if found == version)
            );
        }
    }
}
