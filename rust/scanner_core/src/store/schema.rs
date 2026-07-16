//! Frozen v1 schema and explicit transactional migrations for the Rust-owned DB.

use rusqlite::{Connection, TransactionBehavior};
use std::time::Duration;
use thiserror::Error;

pub const LATEST_USER_VERSION: i32 = 1;
pub const COMMITTED_USER_VERSIONS: &[i32] = &[0];
pub const BUSY_TIMEOUT_MS: u64 = 2_000;

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("scanner database schema is newer than this engine")]
    TooNew,
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

pub fn configure_connection(connection: &Connection) -> Result<(), SchemaError> {
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, "foreign_keys", true)?;
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
    if version == 0 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(V1_DDL)?;
        transaction.pragma_update(None, "user_version", LATEST_USER_VERSION)?;
        transaction.commit()?;
    }
    Ok(())
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
        require_durable_finalization(&connection).expect("durable finalization");
        let final_synchronous: i32 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .expect("final synchronous");
        assert_eq!(final_synchronous, 2);
        assert_eq!(
            tables,
            vec![
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
            ]
        );
    }

    #[test]
    fn every_committed_user_version_migrates_transactionally() {
        for version in COMMITTED_USER_VERSIONS {
            let (_directory, mut connection) =
                open_database(&format!("migration-{version}.sqlite3"));
            connection
                .pragma_update(None, "user_version", version)
                .expect("seed version");
            configure_connection(&connection).expect("pragmas");
            migrate(&mut connection).expect("migration");
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
    fn every_foreign_key_child_column_has_a_leading_index() {
        let (_directory, mut connection) = open_database("indexed-foreign-keys.sqlite3");
        configure_connection(&connection).expect("pragmas");
        migrate(&mut connection).expect("migration");

        for table in [
            "scan_run_attempts",
            "run_diagnostics",
            "file_inventory",
            "parse_cache",
            "scan_file_results",
            "scan_stage_metrics",
            "scan_extension_metrics",
            "context_runs",
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
