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
    legacy_cache_status TEXT,
    legacy_cache_miss_reason TEXT,
    parse_cache_status TEXT CHECK (parse_cache_status IN ('fresh', 'miss', 'snapshot', 'not_applicable')),
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
    snapshot_hit INTEGER NOT NULL DEFAULT 0 CHECK (snapshot_hit IN (0, 1))
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

CREATE INDEX idx_classification_cache_cached ON classification_cache(cached_at_ms);
CREATE INDEX idx_context_runs_artifact ON context_runs(artifact_id);
CREATE INDEX idx_context_runs_reused ON context_runs(reused_from_context_run_id);
CREATE INDEX idx_artifact_files_file ON context_artifact_files(file_identity);
CREATE INDEX idx_artifact_decisions_file ON context_artifact_decisions(file_identity);
CREATE UNIQUE INDEX idx_artifacts_snapshot_key ON context_artifacts(snapshot_key_sha256) WHERE snapshot_eligible = 1;
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
    migrate_terminal_envelopes(transaction)?;

    // Rows that predate the upgrade (running/abandoned) carry no envelope; they
    // are still audited as migrated_v1 rather than being mislabeled full_v2.
    transaction.execute(
        "UPDATE scan_runs SET audit_provenance_version='migrated_v1'
         WHERE status IN ('running', 'abandoned')",
        [],
    )?;

    // v1 parse cache rows carry no SourceGuardV2 and cannot be projected safely;
    // the cache can be rebuilt from source (spec Part 8.2).
    let invalidated = transaction.execute("DELETE FROM parse_cache", [])? as u64;

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
            let semantic_summary_json = serde_json::to_string(&envelope.summary).map_err(|error| {
                SchemaError::MigrationFailed(format!(
                    "scan_run {scan_run_id}: context summary could not be serialized: {error}"
                ))
            })?;
            // Parent + owned semantic rows only. The summary is serialized once
            // into semantic_summary_json; metadata_json is a separate copy and is
            // not part of the artifact payload.
            let artifact_size_bytes = (final_context.len() + semantic_summary_json.len()) as i64;
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
                "scan_extension_metrics",
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
}
