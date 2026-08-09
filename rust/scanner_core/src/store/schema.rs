//! Frozen v1 schema, full v2 schema, and explicit transactional migrations for
//! the Rust-owned DB.
//!
//! v2 is a one-time schema foundation (spec Part 8.2): a fresh database is built
//! as the full v2 schema in one transaction (auto_vacuum=INCREMENTAL is set before
//! the first table), and a committed v1 database is upgraded inside a single
//! transaction that also migrates the legacy terminal envelopes.

use crate::store::cache::sha256_hex;
use ai_daily_scanner_contract::{ContextEnvelope, EngineStatus, Validate};
use rusqlite::{params, Connection, TransactionBehavior};
use std::time::Duration;
use thiserror::Error;

pub const LATEST_USER_VERSION: i32 = 2;
pub const COMMITTED_USER_VERSIONS: &[i32] = &[0, 1];
pub const BUSY_TIMEOUT_MS: u64 = 2_000;

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("scanner database schema is newer than this engine")]
    TooNew,
    #[error("scanner database v1 must be upgraded by the upgrade-db command")]
    UpgradeRequired,
    #[error("scanner database v1 to v2 migration failed: {0}")]
    MigrationFailed(String),
    #[error("scanner database schema migration failed")]
    Sql(#[from] rusqlite::Error),
}

/// v1 is intentionally frozen. Future changes must be appended as a migration.
pub const V1_DDL: &str = r#"
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
    last_seen_run_id INTEGER NOT NULL REFERENCES scan_runs(scan_run_id),
    last_seen_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE parse_cache (
    file_identity TEXT NOT NULL REFERENCES file_inventory(file_identity) ON DELETE CASCADE,
    source_version TEXT NOT NULL,
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
    PRIMARY KEY (file_identity, source_version, parse_profile_hash)
) STRICT;

CREATE TABLE scan_file_results (
    scan_run_id INTEGER NOT NULL REFERENCES scan_runs(scan_run_id) ON DELETE CASCADE,
    file_identity TEXT NOT NULL REFERENCES file_inventory(file_identity),
    relative_path TEXT NOT NULL,
    source_version TEXT NOT NULL,
    parse_profile_hash TEXT NOT NULL CHECK (length(parse_profile_hash) = 64),
    cache_status TEXT NOT NULL CHECK (cache_status IN ('fresh', 'miss')),
    cache_miss_reason TEXT NOT NULL CHECK (cache_miss_reason IN ('', 'new_file', 'error_cache', 'source_version_changed', 'parser_profile_changed')),
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
    PRIMARY KEY (scan_run_id, file_identity)
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
    created_at_ms INTEGER NOT NULL
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

CREATE INDEX idx_scan_runs_status_updated ON scan_runs(status, updated_at_ms);
CREATE INDEX idx_scan_runs_started ON scan_runs(started_at_ms);
CREATE INDEX idx_scan_runs_finished ON scan_runs(finished_at_ms);
CREATE INDEX idx_attempts_owner_status ON scan_run_attempts(owner_id, status);
CREATE INDEX idx_diagnostics_code ON run_diagnostics(error_code);
CREATE INDEX idx_inventory_last_seen ON file_inventory(last_seen_run_id);
CREATE INDEX idx_cache_created ON parse_cache(cached_at_ms);
CREATE INDEX idx_file_results_identity ON scan_file_results(file_identity);
CREATE INDEX idx_file_results_status ON scan_file_results(parse_status);
CREATE INDEX idx_context_decisions_file ON context_decisions(file_identity);
"#;

/// Full v2 schema applied to a fresh (user_version 0) database in one transaction.
/// A fresh database must set `PRAGMA auto_vacuum=INCREMENTAL` before the first
/// table is created (spec Part 4/8.2); that pragma is applied by `migrate`.
/// `scan_execution_metrics` per-run authoritative metrics (spec Part 5.3).
/// Used by the v2 amendment; fresh and upgraded databases get the table from
/// `V2_DDL` / `V2_UPGRADE_DDL`.
const EXECUTION_METRICS_DDL: &str = r#"
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
"#;

pub const V2_DDL: &str = r#"
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
    audit_provenance_version TEXT NOT NULL DEFAULT 'full_v2' CHECK (audit_provenance_version IN ('migrated_v1', 'full_v2')),
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
    legacy_cache_status TEXT,
    legacy_cache_miss_reason TEXT,
    parse_cache_status TEXT CHECK (parse_cache_status IN ('fresh', 'miss', 'snapshot', 'not_applicable')),
    PRIMARY KEY (scan_run_id, file_identity)
) STRICT;

CREATE TABLE scan_file_execution_v2 (
    scan_run_id INTEGER NOT NULL,
    file_identity TEXT NOT NULL,
    parse_transport TEXT NOT NULL CHECK (parse_transport IN ('session', 'one_shot', 'rust_in_process', 'snapshot', 'not_applicable')),
    parse_attempt_count INTEGER NOT NULL CHECK (parse_attempt_count BETWEEN 0 AND 3),
    classification_status TEXT CHECK (classification_status IN ('text_in_parse_window', 'no_text_in_parse_window', 'not_classified_by_budget', 'unknown', 'error')),
    classification_page_count INTEGER CHECK (classification_page_count IS NULL OR classification_page_count >= 0),
    classification_cache_status TEXT CHECK (classification_cache_status IN ('fresh', 'miss', 'snapshot', 'not_eligible')),
    classification_cache_miss_reason TEXT,
    classification_result_examined_pages INTEGER CHECK (classification_result_examined_pages IS NULL OR classification_result_examined_pages >= 0),
    classification_run_inspected_pages INTEGER CHECK (classification_run_inspected_pages IS NULL OR classification_run_inspected_pages >= 0),
    classification_nominal_charged_pages INTEGER CHECK (classification_nominal_charged_pages IS NULL OR classification_nominal_charged_pages >= 0),
    classification_duration_ms INTEGER CHECK (classification_duration_ms IS NULL OR classification_duration_ms >= 0),
    classification_transport TEXT CHECK (classification_transport IN ('session', 'one_shot', 'snapshot', 'not_applicable')),
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

CREATE TABLE schema_migration_history (
    user_version INTEGER PRIMARY KEY CHECK (user_version >= 1),
    origin TEXT NOT NULL CHECK (origin IN ('created_empty', 'upgraded_v1')),
    upgrade_request_id TEXT,
    engine_build TEXT NOT NULL,
    committed_at_ms INTEGER NOT NULL,
    CHECK (
        (origin = 'upgraded_v1' AND upgrade_request_id IS NOT NULL)
        OR
        (origin = 'created_empty' AND upgrade_request_id IS NULL)
    )
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

/// New v2 tables plus the column additions applied when upgrading a committed v1
/// database. `file_inventory` is rebuilt separately because its `last_seen_run_id`
/// FK cannot be altered in place.
const V2_UPGRADE_DDL: &str = r#"
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

CREATE TABLE schema_migration_history (
    user_version INTEGER PRIMARY KEY CHECK (user_version >= 1),
    origin TEXT NOT NULL CHECK (origin IN ('created_empty', 'upgraded_v1')),
    upgrade_request_id TEXT,
    engine_build TEXT NOT NULL,
    committed_at_ms INTEGER NOT NULL,
    CHECK (
        (origin = 'upgraded_v1' AND upgrade_request_id IS NOT NULL)
        OR
        (origin = 'created_empty' AND upgrade_request_id IS NULL)
    )
) STRICT;

ALTER TABLE scan_runs ADD COLUMN final_envelope_metadata_json TEXT;
ALTER TABLE scan_runs ADD COLUMN audit_provenance_version TEXT NOT NULL DEFAULT 'full_v2' CHECK (audit_provenance_version IN ('migrated_v1', 'full_v2'));
ALTER TABLE scan_runs ADD COLUMN audit_size_bytes INTEGER;

ALTER TABLE context_runs ADD COLUMN artifact_id INTEGER REFERENCES context_artifacts(artifact_id) ON DELETE RESTRICT;
ALTER TABLE context_runs ADD COLUMN reused_from_context_run_id INTEGER REFERENCES context_runs(context_run_id) ON DELETE SET NULL;
ALTER TABLE context_runs ADD COLUMN snapshot_hit INTEGER NOT NULL DEFAULT 0 CHECK (snapshot_hit IN (0, 1));

ALTER TABLE scan_file_results ADD COLUMN legacy_cache_status TEXT;
ALTER TABLE scan_file_results ADD COLUMN legacy_cache_miss_reason TEXT;
ALTER TABLE scan_file_results ADD COLUMN parse_cache_status TEXT CHECK (parse_cache_status IN ('fresh', 'miss', 'snapshot', 'not_applicable'));

CREATE TABLE scan_file_execution_v2 (
    scan_run_id INTEGER NOT NULL,
    file_identity TEXT NOT NULL,
    parse_transport TEXT NOT NULL CHECK (parse_transport IN ('session', 'one_shot', 'rust_in_process', 'snapshot', 'not_applicable')),
    parse_attempt_count INTEGER NOT NULL CHECK (parse_attempt_count BETWEEN 0 AND 3),
    classification_status TEXT CHECK (classification_status IN ('text_in_parse_window', 'no_text_in_parse_window', 'not_classified_by_budget', 'unknown', 'error')),
    classification_page_count INTEGER CHECK (classification_page_count IS NULL OR classification_page_count >= 0),
    classification_cache_status TEXT CHECK (classification_cache_status IN ('fresh', 'miss', 'snapshot', 'not_eligible')),
    classification_cache_miss_reason TEXT,
    classification_result_examined_pages INTEGER CHECK (classification_result_examined_pages IS NULL OR classification_result_examined_pages >= 0),
    classification_run_inspected_pages INTEGER CHECK (classification_run_inspected_pages IS NULL OR classification_run_inspected_pages >= 0),
    classification_nominal_charged_pages INTEGER CHECK (classification_nominal_charged_pages IS NULL OR classification_nominal_charged_pages >= 0),
    classification_duration_ms INTEGER CHECK (classification_duration_ms IS NULL OR classification_duration_ms >= 0),
    classification_transport TEXT CHECK (classification_transport IN ('session', 'one_shot', 'snapshot', 'not_applicable')),
    classification_attempt_count INTEGER CHECK (classification_attempt_count IS NULL OR classification_attempt_count BETWEEN 0 AND 3),
    classifier_build TEXT CHECK (classifier_build IS NULL OR length(classifier_build) = 64),
    classifier_profile_hash TEXT CHECK (classifier_profile_hash IS NULL OR length(classifier_profile_hash) = 64),
    PRIMARY KEY (scan_run_id, file_identity),
    FOREIGN KEY (scan_run_id, file_identity) REFERENCES scan_file_results(scan_run_id, file_identity) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_classification_cache_cached ON classification_cache(cached_at_ms);
CREATE INDEX idx_context_runs_artifact ON context_runs(artifact_id);
CREATE INDEX idx_context_runs_reused ON context_runs(reused_from_context_run_id);
CREATE INDEX idx_artifact_files_file ON context_artifact_files(file_identity);
CREATE INDEX idx_artifact_decisions_file ON context_artifact_decisions(file_identity);
CREATE UNIQUE INDEX idx_artifacts_snapshot_key ON context_artifacts(snapshot_key_sha256) WHERE snapshot_eligible = 1;
"#;

const SCAN_FILE_EXECUTION_V2_DDL: &str = r#"
CREATE TABLE scan_file_execution_v2 (
    scan_run_id INTEGER NOT NULL,
    file_identity TEXT NOT NULL,
    parse_transport TEXT NOT NULL CHECK (parse_transport IN ('session', 'one_shot', 'rust_in_process', 'snapshot', 'not_applicable')),
    parse_attempt_count INTEGER NOT NULL CHECK (parse_attempt_count BETWEEN 0 AND 3),
    classification_status TEXT CHECK (classification_status IN ('text_in_parse_window', 'no_text_in_parse_window', 'not_classified_by_budget', 'unknown', 'error')),
    classification_page_count INTEGER CHECK (classification_page_count IS NULL OR classification_page_count >= 0),
    classification_cache_status TEXT CHECK (classification_cache_status IN ('fresh', 'miss', 'snapshot', 'not_eligible')),
    classification_cache_miss_reason TEXT,
    classification_result_examined_pages INTEGER CHECK (classification_result_examined_pages IS NULL OR classification_result_examined_pages >= 0),
    classification_run_inspected_pages INTEGER CHECK (classification_run_inspected_pages IS NULL OR classification_run_inspected_pages >= 0),
    classification_nominal_charged_pages INTEGER CHECK (classification_nominal_charged_pages IS NULL OR classification_nominal_charged_pages >= 0),
    classification_duration_ms INTEGER CHECK (classification_duration_ms IS NULL OR classification_duration_ms >= 0),
    classification_transport TEXT CHECK (classification_transport IN ('session', 'one_shot', 'snapshot', 'not_applicable')),
    classification_attempt_count INTEGER CHECK (classification_attempt_count IS NULL OR classification_attempt_count BETWEEN 0 AND 3),
    classifier_build TEXT CHECK (classifier_build IS NULL OR length(classifier_build) = 64),
    classifier_profile_hash TEXT CHECK (classifier_profile_hash IS NULL OR length(classifier_profile_hash) = 64),
    PRIMARY KEY (scan_run_id, file_identity),
    FOREIGN KEY (scan_run_id, file_identity) REFERENCES scan_file_results(scan_run_id, file_identity) ON DELETE CASCADE
) STRICT;
"#;

/// Replacement table used during the v1→v2 file_inventory rebuild. SQLite cannot
/// alter a FK constraint in place, so the v1 table is rebuilt with a nullable
/// `last_seen_run_id ON DELETE SET NULL` and the two nullable source-guard columns.
const FILE_INVENTORY_V2_DDL: &str = r#"
CREATE TABLE file_inventory_v2 (
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
"#;

/// Rebuilds `parse_cache` with the SourceGuardV2-bound primary key
/// (spec R3-29 / acceptance "SourceGuardV2 在 legacy mtime+size 未变的内容替换上
/// 仍强制 cache miss"). Legacy rows carry no guard and cannot be guard-bound, so
/// the rebuild clears them (the cache can be rebuilt from source). Fresh v2
/// databases get the guard-bound shape directly from `V2_DDL`; committed v1→v2
/// databases and pre-amendment v2 databases are converged here.
const PARSE_CACHE_GUARD_DDL: &str = r#"
CREATE TABLE parse_cache_v2 (
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
DROP TABLE parse_cache;
ALTER TABLE parse_cache_v2 RENAME TO parse_cache;
CREATE INDEX idx_cache_created ON parse_cache(cached_at_ms);
"#;

/// `context_runs` with the snapshot relationship CHECK (spec Part 5.1):
/// `reused_from_context_run_id` is required exactly when `snapshot_hit=1`.
/// SQLite cannot add a CHECK to an existing table in place, so the table is
/// rebuilt by create-new/copy/drop/rename (like `file_inventory_v2`). The
/// caller must disable FK enforcement for the rebuild transaction. The
/// self-reference is written against `context_runs_v2`; SQLite rewrites it to
/// `context_runs` on the rename.
const CONTEXT_RUNS_V2_DDL: &str = r#"
CREATE TABLE context_runs_v2 (
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
    reused_from_context_run_id INTEGER REFERENCES context_runs_v2(context_run_id) ON DELETE SET NULL,
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
"#;

/// Rebuilds `context_runs` with the snapshot relationship CHECK. Used by the
/// v1→v2 upgrade (after the migrated artifacts are linked) and by the v2
/// amendment for databases committed before the CHECK existed. The caller has
/// disabled FK enforcement for the duration of the enclosing transaction.
fn rebuild_context_runs(transaction: &rusqlite::Transaction<'_>) -> Result<(), SchemaError> {
    transaction.execute_batch(CONTEXT_RUNS_V2_DDL)?;
    transaction.execute(
        "INSERT INTO context_runs_v2(
            context_run_id, scan_run_id, context_profile_hash, status,
            final_context, context_sha256, source_file_count, success_count,
            timeout_count, included_file_count, omitted_file_count,
            error_file_count, input_chars, output_chars, total_duration_ms,
            discovery_duration_ms, parse_duration_ms, compression_duration_ms,
            created_at_ms, artifact_id, reused_from_context_run_id, snapshot_hit
         ) SELECT context_run_id, scan_run_id, context_profile_hash, status,
             final_context, context_sha256, source_file_count, success_count,
             timeout_count, included_file_count, omitted_file_count,
             error_file_count, input_chars, output_chars, total_duration_ms,
             discovery_duration_ms, parse_duration_ms, compression_duration_ms,
             created_at_ms, artifact_id, reused_from_context_run_id, snapshot_hit
           FROM context_runs",
        [],
    )?;
    transaction.execute_batch(
        "DROP TABLE context_runs;
         ALTER TABLE context_runs_v2 RENAME TO context_runs;
         CREATE INDEX idx_context_runs_artifact ON context_runs(artifact_id);
         CREATE INDEX idx_context_runs_reused ON context_runs(reused_from_context_run_id);",
    )?;
    Ok(())
}

/// True when the committed `context_runs` table already carries the snapshot
/// relationship + status⇔artifact CHECKs (spec Part 5.1). The strongest marker
/// is the status⇔artifact_id CHECK; databases that predate it are rebuilt by the
/// v2 amendment so migrated/legacy rows satisfy the new invariant (fail closed).
fn context_runs_has_snapshot_check(connection: &Connection) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT instr(sql, 'status IN (''success'', ''partial'') AND artifact_id IS NOT NULL') > 0
         FROM sqlite_schema WHERE type='table' AND name='context_runs'",
        [],
        |row| row.get(0),
    )
}

/// `scan_file_results` with the full v2 miss-reason CHECK (spec Part 4/5.2:
/// `parser_identity_changed` / `entry_absent_or_evicted` in addition to the v1
/// set). SQLite cannot add a CHECK to an existing table in place, so the table
/// is rebuilt by create-new/copy/drop/rename for the v1→v2 upgrade and the v2
/// amendment. All v1 + v2 columns are carried over verbatim.
const SCAN_FILE_RESULTS_V2_DDL: &str = r#"
CREATE TABLE scan_file_results_v2 (
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
    legacy_cache_status TEXT,
    legacy_cache_miss_reason TEXT,
    parse_cache_status TEXT CHECK (parse_cache_status IN ('fresh', 'miss', 'snapshot', 'not_applicable')),
    PRIMARY KEY (scan_run_id, file_identity)
) STRICT;
"#;

fn rebuild_scan_file_results(transaction: &rusqlite::Transaction<'_>) -> Result<(), SchemaError> {
    transaction.execute_batch(SCAN_FILE_RESULTS_V2_DDL)?;
    transaction.execute(
        "INSERT INTO scan_file_results_v2(
            scan_run_id, file_identity, relative_path, source_version,
            parse_profile_hash, cache_status, cache_miss_reason, parse_status,
            parser_backend, worker_lane, truncated, content_sha256,
            primary_duration_ms, fallback_duration_ms, parse_duration_ms,
            failure_class, fallback_backend, fallback_reason_code,
            error_code, error_message, error_retryable, error_stage,
            error_file_path, error_backend, legacy_cache_status,
            legacy_cache_miss_reason, parse_cache_status
         ) SELECT scan_run_id, file_identity, relative_path, source_version,
             parse_profile_hash, cache_status, cache_miss_reason, parse_status,
             parser_backend, worker_lane, truncated, content_sha256,
             primary_duration_ms, fallback_duration_ms, parse_duration_ms,
             failure_class, fallback_backend, fallback_reason_code,
             error_code, error_message, error_retryable, error_stage,
             error_file_path, error_backend, legacy_cache_status,
             legacy_cache_miss_reason, parse_cache_status
           FROM scan_file_results",
        [],
    )?;
    transaction.execute_batch(
        "DROP TABLE scan_file_results;
         ALTER TABLE scan_file_results_v2 RENAME TO scan_file_results;
         CREATE INDEX idx_file_results_identity ON scan_file_results(file_identity);
         CREATE INDEX idx_file_results_status ON scan_file_results(parse_status);",
    )?;
    Ok(())
}

/// True when the committed `scan_file_results` already carries the full v2
/// miss-reason CHECK (the `parser_identity_changed` marker text).
fn scan_file_results_has_v2_miss_reason_check(connection: &Connection) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT instr(sql, 'parser_identity_changed') > 0
         FROM sqlite_schema WHERE type='table' AND name='scan_file_results'",
        [],
        |row| row.get(0),
    )
}

pub fn configure_connection(connection: &Connection) -> Result<(), SchemaError> {
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    // A fresh database must set auto_vacuum=INCREMENTAL before its first table.
    // It must also precede journal_mode=WAL: switching a fresh DB to WAL writes the
    // header with the default auto_vacuum, after which the pragma is a no-op until
    // VACUUM. On an existing DB this is harmless (no effect until VACUUM).
    connection.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    let _: String = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

pub fn require_durable_finalization(connection: &Connection) -> Result<(), SchemaError> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

pub fn migrate(connection: &mut Connection) -> Result<(), SchemaError> {
    let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > LATEST_USER_VERSION {
        return Err(SchemaError::TooNew);
    }
    if version == 1 {
        // A committed v1 database must never be auto-migrated by a generic
        // migrate() call: only the separately-authorized upgrade-db command may
        // upgrade it (spec Part 8.3). Callers that verified the gate can route
        // through the explicit upgrade_v1_to_v2 entry.
        return Err(SchemaError::UpgradeRequired);
    }
    if version == 0 {
        // A fresh database must set auto_vacuum=INCREMENTAL before the first table
        // is created (spec Part 4/8.2). configure_connection already does this
        // before WAL for the normal flow; the statement here is a safety net for
        // callers that invoke migrate without configuring the connection first.
        connection.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(V2_DDL)?;
        insert_created_empty_history(&transaction)?;
        transaction.pragma_update(None, "user_version", LATEST_USER_VERSION)?;
        transaction.commit()?;
        return Ok(());
    }
    // Version 2 schema amendment: the parse_cache table MUST be SourceGuardV2-bound
    // and carry the retention columns (`entry_size_bytes`, `generation_rank`,
    // `last_accessed_bucket`, spec Part 4). A committed v2 database created
    // before the amendment has the legacy key/columns; detect the missing column
    // and rebuild the table (clearing legacy rows).
    if version == LATEST_USER_VERSION {
        let has_guard: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('parse_cache')
                WHERE name='source_guard_kind'
             )",
            [],
            |row| row.get(0),
        )?;
        let has_size: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('parse_cache')
                WHERE name='entry_size_bytes'
             )",
            [],
            |row| row.get(0),
        )?;
        if !has_guard || !has_size {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(PARSE_CACHE_GUARD_DDL)?;
            transaction.commit()?;
        }
        // context_runs snapshot relationship CHECK (spec Part 5.1): databases
        // committed before the CHECK existed are rebuilt with it. FK
        // enforcement is disabled for the rebuild so dropping the parent does
        // not cascade context_decisions; data violating the invariant fails
        // the migration (fail closed) rather than being silently accepted.
        if !context_runs_has_snapshot_check(connection)? {
            connection.pragma_update(None, "foreign_keys", false)?;
            let migration = (|| -> Result<(), SchemaError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                rebuild_context_runs(&transaction)?;
                transaction.commit()?;
                Ok(())
            })();
            connection.pragma_update(None, "foreign_keys", true)?;
            migration?;
        }
        // spec Part 5.3: databases committed before the `scan_execution_metrics`
        // table existed are amended in place. Migrated v1 runs carry no row
        // (they fail closed on v2 inspect); full_v2 finalize inserts one row.
        let has_execution_metrics: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema
                WHERE type='table' AND name='scan_execution_metrics'
             )",
            [],
            |row| row.get(0),
        )?;
        if !has_execution_metrics {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(EXECUTION_METRICS_DDL)?;
            transaction.commit()?;
        }
        // spec Part 4/5.2: databases whose `scan_file_results.cache_miss_reason`
        // CHECK predates the v2 reason set are rebuilt with the full set.
        if !scan_file_results_has_v2_miss_reason_check(connection)? {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            rebuild_scan_file_results(&transaction)?;
            transaction.commit()?;
        }
        let has_file_execution_v2: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema
                WHERE type='table' AND name='scan_file_execution_v2'
             )",
            [],
            |row| row.get(0),
        )?;
        if !has_file_execution_v2 {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(SCAN_FILE_EXECUTION_V2_DDL)?;
            transaction.commit()?;
        }
    }
    Ok(())
}

/// Upgrades a committed v1 database to v2 outside the automatic `migrate` path,
/// recording the caller's request id in the migration history. The upgrade-db
/// command is the only production caller; normal `ScannerStore::open` fails
/// closed on v1 with `SCHEMA_UPGRADE_REQUIRED` instead of routing here.
///
/// The returned count is the number of legacy parse-cache rows invalidated by
/// the committed migration (`invalidated == detected` after COMMIT).
pub fn upgrade_v1_to_v2(connection: &mut Connection, request_id: &str) -> Result<u64, SchemaError> {
    connection.pragma_update(None, "foreign_keys", false)?;
    let migration = (|| -> Result<u64, SchemaError> {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let invalidated = migrate_v1_to_v2(&transaction, request_id)?;
        transaction.pragma_update(None, "user_version", LATEST_USER_VERSION)?;
        transaction.commit()?;
        Ok(invalidated)
    })();
    connection.pragma_update(None, "foreign_keys", true)?;
    migration
}

fn insert_created_empty_history(transaction: &rusqlite::Transaction<'_>) -> Result<(), SchemaError> {
    transaction.execute(
        "INSERT INTO schema_migration_history(
            user_version, origin, upgrade_request_id, engine_build, committed_at_ms
         ) VALUES (2, 'created_empty', NULL, ?1, ?2)",
        params![env!("AI_DAILY_ENGINE_BUILD"), now_ms()],
    )?;
    Ok(())
}

/// Upgrades a committed v1 database to v2 inside the caller's transaction.
///
/// The file_inventory rebuild (create-new, copy, drop-old, rename) is one atomic
/// step; the caller has disabled FK enforcement for the duration of this
/// transaction and re-enables it after COMMIT. Every legacy terminal envelope is
/// parsed and validated; any single failure aborts the whole migration and keeps
/// user_version 1. Returns the number of legacy parse-cache rows deleted.
fn migrate_v1_to_v2(
    transaction: &rusqlite::Transaction<'_>,
    upgrade_request_id: &str,
) -> Result<u64, SchemaError> {
    rebuild_file_inventory(transaction)?;
    transaction.execute_batch(V2_UPGRADE_DDL)?;
    migrate_file_result_legacy_cache(transaction)?;
    // Rebuild scan_file_results with the full v2 miss-reason CHECK (spec Part
    // 4/5.2) AFTER the legacy cache columns are populated, so the copy carries
    // both the v1 and v2 audit columns.
    rebuild_scan_file_results(transaction)?;
    migrate_terminal_envelopes(transaction)?;
    // Rebuild context_runs with the snapshot relationship CHECK after the
    // migrated artifacts are linked, so the copied rows satisfy the invariant.
    rebuild_context_runs(transaction)?;

    // Rows that predate the upgrade (running/abandoned) carry no envelope; they
    // are still audited as migrated_v1 rather than being mislabeled full_v2.
    transaction.execute(
        "UPDATE scan_runs SET audit_provenance_version='migrated_v1'
         WHERE status IN ('running', 'abandoned')",
        [],
    )?;

    // v1 parse cache rows carry no SourceGuardV2 and cannot be projected safely;
    // the cache can be rebuilt from source (spec Part 8.2). The table is rebuilt
    // with the SourceGuardV2-bound primary key so post-upgrade writes are
    // guard-bound (spec R3-29).
    let invalidated: u64 = transaction
        .query_row("SELECT count(*) FROM parse_cache", [], |row| {
            row.get::<_, i64>(0)
        })?
        as u64;
    transaction.execute_batch(PARSE_CACHE_GUARD_DDL)?;

    transaction.execute(
        "INSERT INTO schema_migration_history(
            user_version, origin, upgrade_request_id, engine_build, committed_at_ms
         ) VALUES (2, 'upgraded_v1', ?1, ?2, ?3)",
        params![upgrade_request_id, env!("AI_DAILY_ENGINE_BUILD"), now_ms()],
    )?;
    Ok(invalidated)
}

fn rebuild_file_inventory(transaction: &rusqlite::Transaction<'_>) -> Result<(), SchemaError> {
    transaction.execute_batch(FILE_INVENTORY_V2_DDL)?;
    transaction.execute(
        "INSERT INTO file_inventory_v2(
            file_identity, absolute_path, relative_path, file_type, source_version,
            size_bytes, mtime_ns, last_seen_run_id, last_seen_at_ms
         ) SELECT file_identity, absolute_path, relative_path, file_type, source_version,
             size_bytes, mtime_ns, last_seen_run_id, last_seen_at_ms FROM file_inventory",
        [],
    )?;
    transaction.execute_batch(
        "DROP TABLE file_inventory;
         ALTER TABLE file_inventory_v2 RENAME TO file_inventory;
         CREATE INDEX idx_inventory_last_seen ON file_inventory(last_seen_run_id);",
    )?;
    Ok(())
}

fn migrate_file_result_legacy_cache(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), SchemaError> {
    transaction.execute(
        "UPDATE scan_file_results
         SET legacy_cache_status = cache_status,
             legacy_cache_miss_reason = cache_miss_reason",
        [],
    )?;
    Ok(())
}

/// Parses and validates every terminal `final_envelope_json`. Success/Partial
/// contexts are extracted to a payload artifact (`snapshot_eligible=false`) and
/// linked from `context_runs`; the file body is removed from the metadata JSON;
/// every run is audited as `migrated_v1`. Any unparseable row fails the migration.
fn migrate_terminal_envelopes(transaction: &rusqlite::Transaction<'_>) -> Result<(), SchemaError> {
    let mut statement = transaction.prepare(
        "SELECT scan_run_id, request_id, status, created_at_ms, final_envelope_json
         FROM scan_runs
         WHERE status IN ('success', 'partial', 'error')",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut rows: Vec<_> = rows.collect::<Result<_, _>>()?;
    drop(statement);

    for (scan_run_id, request_id, status, created_at_ms, envelope_json) in rows.drain(..) {
        let envelope: ContextEnvelope = serde_json::from_str(&envelope_json).map_err(|error| {
            SchemaError::MigrationFailed(format!(
                "scan_run {scan_run_id}: final envelope JSON is invalid: {error}"
            ))
        })?;
        envelope.validate().map_err(|error| {
            SchemaError::MigrationFailed(format!(
                "scan_run {scan_run_id}: final envelope violates the contract: {error}"
            ))
        })?;
        let status_matches = matches!(
            (status.as_str(), envelope.status),
            ("success", EngineStatus::Ok)
                | ("partial", EngineStatus::Partial)
                | ("error", EngineStatus::Error)
        );
        if !status_matches || envelope.request_id != request_id {
            return Err(SchemaError::MigrationFailed(format!(
                "scan_run {scan_run_id}: final envelope does not match its run"
            )));
        }

        // Metadata JSON keeps the small envelope fields verbatim (including the
        // original warnings) but drops the file body, which lives in the artifact.
        let mut metadata = serde_json::to_value(&envelope)
            .map_err(|error| {
                SchemaError::MigrationFailed(format!(
                    "scan_run {scan_run_id}: final envelope could not be canonicalized: {error}"
                ))
            })?
            .as_object()
            .cloned()
            .ok_or_else(|| {
                SchemaError::MigrationFailed(format!(
                    "scan_run {scan_run_id}: final envelope is not a JSON object"
                ))
            })?;
        metadata.remove("file_context");
        let metadata_json = serde_json::to_string(&serde_json::Value::Object(metadata))
            .map_err(|error| {
                SchemaError::MigrationFailed(format!(
                    "scan_run {scan_run_id}: envelope metadata could not be serialized: {error}"
                ))
            })?;

        if matches!(envelope.status, EngineStatus::Ok | EngineStatus::Partial) {
            let final_context = envelope.file_context.clone();
            let context_sha256 = sha256_hex(final_context.as_bytes());
            // `semantic_summary_json` stores the immutable SemanticSummary shape
            // (spec Part 5.1); migrated rows have no reserved/rendered tracking,
            // so both fall back to the rendered output char count. This keeps the
            // migrated artifact loadable by the store's replay/artifact readers.
            let semantic_summary = crate::artifact::SemanticSummary {
                source_file_count: envelope.summary.source_file_count,
                success_count: envelope.summary.success_count,
                timeout_count: envelope.summary.timeout_count,
                included_file_count: envelope.summary.included_file_count,
                omitted_file_count: envelope.summary.omitted_file_count,
                error_file_count: envelope.summary.error_file_count,
                input_chars: envelope.summary.input_chars,
                output_chars: envelope.summary.output_chars,
                reserved_chars: envelope.summary.output_chars,
                rendered_chars: envelope.summary.output_chars,
            };
            let semantic_summary_json = serde_json::to_string(&semantic_summary).map_err(|error| {
                SchemaError::MigrationFailed(format!(
                    "scan_run {scan_run_id}: context summary could not be serialized: {error}"
                ))
            })?;
            // Parent + owned semantic rows only (spec Part 4 exact logical
            // bytes). The size definition is SHARED with the write path
            // (store/mod.rs artifact_size_bytes), so migrated payload artifacts
            // and new artifacts use the same accounting. Migrated artifacts are
            // ineligible: no snapshot key, no owned file/decision rows.
            let artifact_size_bytes = super::artifact_size_bytes(
                &final_context,
                &context_sha256,
                &semantic_summary_json,
                None,
                &[],
                &[],
            );
            transaction.execute(
                "INSERT INTO context_artifacts(
                    snapshot_eligible, snapshot_key_sha256, snapshot_key_json,
                    final_context, context_sha256, semantic_summary_json,
                    artifact_size_bytes, created_at_ms, last_accessed_bucket
                 ) VALUES (0, NULL, NULL, ?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    final_context,
                    context_sha256,
                    semantic_summary_json,
                    artifact_size_bytes,
                    created_at_ms,
                    date_bucket(created_at_ms),
                ],
            )?;
            let artifact_id = transaction.last_insert_rowid();
            let linked = transaction.execute(
                "UPDATE context_runs SET artifact_id=?1 WHERE scan_run_id=?2",
                params![artifact_id, scan_run_id],
            )?;
            if linked != 1 {
                return Err(SchemaError::MigrationFailed(format!(
                    "scan_run {scan_run_id}: success/partial run has no context_runs row"
                )));
            }
        }

        let audit_size_bytes = (envelope_json.len() + metadata_json.len()) as i64;
        transaction.execute(
            "UPDATE scan_runs
             SET final_envelope_metadata_json=?1, audit_provenance_version='migrated_v1', audit_size_bytes=?2
             WHERE scan_run_id=?3",
            params![metadata_json, audit_size_bytes, scan_run_id],
        )?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn date_bucket(created_at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(created_at_ms)
        .map(|value| value.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;
    use tempfile::tempdir;

    fn open_database(name: &str) -> (tempfile::TempDir, Connection) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join(name);
        let connection = Connection::open(path).expect("database opens");
        (directory, connection)
    }

    #[test]
    fn fresh_schema_has_all_frozen_v1_tables_and_pragmas() {
        let (_directory, mut connection) = open_database("scan_index_v2.sqlite3");
        configure_connection(&connection).expect("pragmas");
        migrate(&mut connection).expect("migration");

        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version");
        let foreign_keys: i32 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("foreign_keys");
        let journal_mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal_mode");
        let synchronous: i32 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .expect("synchronous");
        let auto_vacuum: i64 = connection
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
            .expect("auto_vacuum");
        let tables: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .expect("schema query")
            .query_map([], |row| row.get(0))
            .expect("table rows")
            .collect::<Result<_, _>>()
            .expect("table names");

        assert_eq!(version, LATEST_USER_VERSION);
        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(synchronous, 1);
        assert_eq!(auto_vacuum, 2, "fresh v2 database must be auto_vacuum=INCREMENTAL");
        require_durable_finalization(&connection).expect("durable finalization");
        let final_synchronous: i32 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .expect("final synchronous");
        assert_eq!(final_synchronous, 2);
        assert_eq!(
            tables,
            vec![
                "classification_cache",
                "context_artifact_decisions",
                "context_artifact_files",
                "context_artifacts",
                "context_decisions",
                "context_runs",
                "engine_lease",
                "file_inventory",
                "parse_cache",
                "run_diagnostics",
                "scan_execution_metrics",
                "scan_extension_metrics",
                "scan_file_execution_v2",
                "scan_file_results",
                "scan_run_attempts",
                "scan_runs",
                "scan_stage_metrics",
                "schema_migration_history",
            ]
        );
    }

    #[test]
    fn every_committed_user_version_migrates_transactionally() {
        for &version in COMMITTED_USER_VERSIONS {
            let (_directory, mut connection) =
                open_database(&format!("migration-{version}.sqlite3"));
            if version == 1 {
                connection
                    .execute_batch(V1_DDL)
                    .expect("v1 schema for the upgrade fixture");
            }
            connection
                .pragma_update(None, "user_version", version)
                .expect("seed version");
            configure_connection(&connection).expect("pragmas");
            if version == 1 {
                // A committed v1 database is upgraded only through the explicit
                // upgrade_v1_to_v2 entry (the upgrade-db command); migrate()
                // refuses it (spec Part 8.3).
                upgrade_v1_to_v2(&mut connection, "123e4567-e89b-42d3-a456-426614174000")
                    .expect("explicit v1 upgrade");
            } else {
                migrate(&mut connection).expect("migration");
            }
            let migrated: i32 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("migrated version");
            assert_eq!(migrated, LATEST_USER_VERSION);
            connection
                .execute(
                    "INSERT INTO scan_runs(request_id, canonical_request_json, request_hash_algorithm, request_hash, owner_id, status, created_at_ms, started_at_ms, updated_at_ms) VALUES (?1, '{}', 'sha256-request-v1', ?2, 'owner', 'running', 1, 1, 1)",
                    (format!("request-{version}"), "0".repeat(64)),
                )
                .expect("migrated schema is writable");
        }
    }

    #[test]
    fn migrate_refuses_to_auto_upgrade_a_committed_v1_database() {
        let (_directory, mut connection) = open_database("v1-refused.sqlite3");
        connection
            .execute_batch(V1_DDL)
            .expect("v1 schema for the refused upgrade fixture");
        connection.pragma_update(None, "user_version", 1).expect("seed version");
        configure_connection(&connection).expect("pragmas");

        assert!(matches!(
            migrate(&mut connection),
            Err(SchemaError::UpgradeRequired)
        ));
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version");
        assert_eq!(version, 1, "migrate must not touch a v1 database");
        let parse_cache_count: i64 = connection
            .query_row("SELECT count(*) FROM sqlite_schema WHERE name='parse_cache'", [], |row| {
                row.get(0)
            })
            .expect("schema count");
        assert_eq!(parse_cache_count, 1, "v1 schema must be left untouched");
    }

    #[test]
    fn every_foreign_key_child_column_has_a_leading_index() {
        let (_directory, mut connection) = open_database("indexed-foreign-keys.sqlite3");
        configure_connection(&connection).expect("pragmas");
        migrate(&mut connection).expect("migration");

        for table in [
            "scan_run_attempts",
            "run_diagnostics",
            "file_inventory",
            "parse_cache",
            "classification_cache",
            "scan_file_results",
            "scan_stage_metrics",
            "scan_extension_metrics",
            "context_runs",
            "context_artifacts",
            "context_artifact_files",
            "context_artifact_decisions",
            "context_decisions",
        ] {
            let foreign_key_columns: Vec<String> = connection
                .prepare(&format!("PRAGMA foreign_key_list('{table}')"))
                .expect("foreign key query")
                .query_map([], |row| row.get(3))
                .expect("foreign key rows")
                .collect::<Result<_, _>>()
                .expect("foreign key columns");
            let index_names: Vec<String> = connection
                .prepare(&format!("PRAGMA index_list('{table}')"))
                .expect("index query")
                .query_map([], |row| row.get(1))
                .expect("index rows")
                .collect::<Result<_, _>>()
                .expect("index names");
            let mut leading_columns = std::collections::HashSet::new();
            for index_name in index_names {
                let first_column: Option<String> = connection
                    .query_row(&format!("PRAGMA index_info('{index_name}')"), [], |row| {
                        row.get(2)
                    })
                    .optional()
                    .expect("index info");
                if let Some(column) = first_column {
                    leading_columns.insert(column);
                }
            }
            for column in foreign_key_columns {
                assert!(
                    leading_columns.contains(&column),
                    "{table}.{column} must have a leading index"
                );
            }
        }
    }

    #[test]
    fn failed_v1_migration_rolls_back_every_table() {
        let (_directory, mut connection) = open_database("failed-migration.sqlite3");
        connection
            .execute("CREATE TABLE context_runs(conflict INTEGER)", [])
            .expect("seed conflict");
        configure_connection(&connection).expect("pragmas");

        assert!(migrate(&mut connection).is_err());
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version");
        let engine_lease_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='engine_lease'",
                [],
                |row| row.get(0),
            )
            .expect("schema count");
        let scan_runs_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='scan_runs'",
                [],
                |row| row.get(0),
            )
            .expect("scan_runs schema count");
        assert_eq!(version, 0);
        assert_eq!(engine_lease_count, 0);
        assert_eq!(scan_runs_count, 0);
    }

    /// `context_runs` as committed before the snapshot relationship CHECK
    /// (spec Part 5.1) existed; used to seed the v2 amendment fixture.
    const OLD_CONTEXT_RUNS_DDL: &str = r#"
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
        snapshot_hit INTEGER NOT NULL DEFAULT 0 CHECK (snapshot_hit IN (0, 1))
    ) STRICT;
    "#;

    #[test]
    fn v2_amendment_rebuilds_context_runs_with_snapshot_check() {
        let (_directory, mut connection) = open_database("context-runs-amendment.sqlite3");
        configure_connection(&connection).expect("pragmas");
        migrate(&mut connection).expect("fresh migration");

        // Simulate a v2 database committed before the snapshot CHECK existed:
        // replace context_runs with the pre-CHECK shape.
        connection
            .pragma_update(None, "foreign_keys", false)
            .expect("disable foreign keys for the fixture rebuild");
        connection
            .execute_batch("DROP TABLE context_runs;")
            .expect("drop context_runs");
        connection
            .execute_batch(OLD_CONTEXT_RUNS_DDL)
            .expect("pre-CHECK context_runs");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("re-enable foreign keys");
        assert!(
            !context_runs_has_snapshot_check(&connection).expect("check probe"),
            "fixture must start without the snapshot CHECK"
        );

        migrate(&mut connection).expect("v2 amendment migration");
        assert!(
            context_runs_has_snapshot_check(&connection).expect("check probe"),
            "v2 amendment must rebuild context_runs with the snapshot CHECK"
        );
    }

    #[test]
    fn fresh_v2_context_runs_rejects_snapshot_hit_without_reused_from() {
        let (_directory, mut connection) = open_database("context-runs-snapshot-check.sqlite3");
        configure_connection(&connection).expect("pragmas");
        migrate(&mut connection).expect("fresh migration");
        for index in 1..=2_i64 {
            connection
                .execute(
                    "INSERT INTO scan_runs(
                        request_id, canonical_request_json, request_hash_algorithm,
                        request_hash, owner_id, status, created_at_ms, started_at_ms,
                        updated_at_ms, finished_at_ms, final_envelope_json
                     ) VALUES (?1, '{}', 'sha256-request-v1', ?2, 'owner', 'success',
                       1, 1, 1, 2, '{}')",
                    params![
                        format!("00000000-0000-4000-8000-{index:012}"),
                        "0".repeat(64),
                    ],
                )
                .expect("scan_runs row");
        }
        // Every success context_runs row must reference an artifact (spec Part
        // 5.1 status⇔artifact_id CHECK); seed one ineligible payload artifact.
        connection
            .execute(
                "INSERT INTO context_artifacts(
                    snapshot_eligible, snapshot_key_sha256, snapshot_key_json,
                    final_context, context_sha256, semantic_summary_json,
                    artifact_size_bytes, created_at_ms, last_accessed_bucket
                 ) VALUES (0, NULL, NULL, 'ctx', ?1, '{}', 3, 1, '2026-08-08')",
                params!["1".repeat(64)],
            )
            .expect("payload artifact row");
        connection
            .execute(
                "INSERT INTO context_runs(
                    context_run_id, scan_run_id, context_profile_hash, status,
                    final_context, context_sha256, source_file_count, success_count,
                    timeout_count, included_file_count, omitted_file_count,
                    error_file_count, input_chars, output_chars, total_duration_ms,
                    discovery_duration_ms, parse_duration_ms, compression_duration_ms,
                    created_at_ms, artifact_id, snapshot_hit
                 ) VALUES (1, 1, ?1, 'success', 'ctx', ?2, 1, 1, 0, 1, 0, 0,
                           1, 1, 1, 0, 0, 0, 1, 1, 0)",
                params!["0".repeat(64), "1".repeat(64)],
            )
            .expect("snapshot_hit=0 without reused_from must insert");
        // A snapshot-hit row whose source run was GC'd keeps snapshot_hit=1 with a
        // NULL reused_from (ON DELETE SET NULL) — the relaxed CHECK allows it.
        connection
            .execute(
                "INSERT INTO context_runs(
                    context_run_id, scan_run_id, context_profile_hash, status,
                    final_context, context_sha256, source_file_count, success_count,
                    timeout_count, included_file_count, omitted_file_count,
                    error_file_count, input_chars, output_chars, total_duration_ms,
                    discovery_duration_ms, parse_duration_ms, compression_duration_ms,
                    created_at_ms, artifact_id, snapshot_hit
                 ) VALUES (2, 2, ?1, 'success', 'ctx', ?2, 1, 1, 0, 1, 0, 0,
                           1, 1, 1, 0, 0, 0, 1, 1, 1)",
                params!["0".repeat(64), "1".repeat(64)],
            )
            .expect("snapshot_hit=1 with a null reused_from (post-GC) must insert");
        // snapshot_hit=0 with a non-null reused_from is invalid.
        let orphan_non_hit = connection.execute(
            "INSERT INTO context_runs(
                context_run_id, scan_run_id, context_profile_hash, status,
                final_context, context_sha256, source_file_count, success_count,
                timeout_count, included_file_count, omitted_file_count,
                error_file_count, input_chars, output_chars, total_duration_ms,
                discovery_duration_ms, parse_duration_ms, compression_duration_ms,
                created_at_ms, artifact_id, snapshot_hit, reused_from_context_run_id
             ) VALUES (3, 3, ?1, 'success', 'ctx', ?2, 1, 1, 0, 1, 0, 0,
                       1, 1, 1, 0, 0, 0, 1, 1, 0, 1)",
            params!["0".repeat(64), "1".repeat(64)],
        );
        assert!(
            orphan_non_hit.is_err(),
            "snapshot_hit=0 with reused_from_context_run_id must be rejected"
        );
    }
}
