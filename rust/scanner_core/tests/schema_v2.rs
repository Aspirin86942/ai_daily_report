//! v2 schema foundation migration tests (spec Part 8.2).
//!
//! Task 3: one-time v2 schema foundation — new tables, column additions, and the
//! transactional v1→v2 migration of existing data. A fresh empty database becomes
//! the full v2 schema in one go (auto_vacuum=INCREMENTAL before the first table),
//! and a committed v1 database upgrades inside a single transaction.

use ai_daily_scanner_core::store::schema::{
    configure_connection, migrate, upgrade_v1_to_v2, LATEST_USER_VERSION, V1_DDL,
};

/// Request id recorded in schema_migration_history by the explicit v1→v2 upgrade.
const UPGRADE_REQUEST_ID: &str = "123e4567-e89b-42d3-a456-426614174000";

#[derive(Clone, Copy)]
enum FixtureRunKind {
    Success,
    Partial,
    Error,
    Running,
}

impl FixtureRunKind {
    fn request_id(&self) -> &'static str {
        match self {
            FixtureRunKind::Success => "123e4567-e89b-42d3-a456-426614174000",
            FixtureRunKind::Partial => "223e4567-e89b-42d3-a456-426614174001",
            FixtureRunKind::Error => "323e4567-e89b-42d3-a456-426614174002",
            FixtureRunKind::Running => "423e4567-e89b-42d3-a456-426614174003",
        }
    }
}

/// Builds a committed v1 database whose single run matches `kind`. Terminal
/// fixtures carry one inventory row, one legacy parse-cache row, and one file
/// result row; Success/Partial fixtures also carry a context_runs row.
fn build_v1_fixture(kind: FixtureRunKind) -> (tempfile::TempDir, rusqlite::Connection) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("scan_index_v1.sqlite3");
    let connection = rusqlite::Connection::open(path).expect("database opens");
    configure_connection(&connection).expect("pragmas");
    connection.execute_batch(V1_DDL).expect("v1 schema builds");
    connection
        .pragma_update(None, "user_version", 1)
        .expect("seed user_version");

    let request_id = kind.request_id();
    match kind {
        FixtureRunKind::Success => insert_success_run(&connection, request_id),
        FixtureRunKind::Partial => insert_partial_run(&connection, request_id),
        FixtureRunKind::Error => insert_error_run(&connection, request_id),
        FixtureRunKind::Running => insert_running_run(&connection, request_id),
    }
    (directory, connection)
}

fn open_v1_fixture() -> (tempfile::TempDir, rusqlite::Connection) {
    build_v1_fixture(FixtureRunKind::Success)
}

fn insert_scan_run(
    connection: &rusqlite::Connection,
    request_id: &str,
    status: &str,
    finished_at_ms: Option<i64>,
    envelope: Option<&str>,
) {
    connection
        .execute(
            "INSERT INTO scan_runs(
                request_id, canonical_request_json, request_hash_algorithm, request_hash,
                owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                finished_at_ms, final_envelope_json
             ) VALUES (?1, '{}', 'sha256-request-v1', ?2, 'owner', ?3, 1, 1, 1, ?4, ?5)",
            rusqlite::params![request_id, "0".repeat(64), status, finished_at_ms, envelope],
        )
        .expect("scan_runs row");
}

fn insert_common_rows(connection: &rusqlite::Connection) {
    connection
        .execute(
            "INSERT INTO file_inventory(
                file_identity, absolute_path, relative_path, file_type, source_version,
                size_bytes, mtime_ns, last_seen_run_id, last_seen_at_ms
             ) VALUES ('C:\\work\\a.txt', 'C:\\work\\a.txt', 'a.txt', '.txt',
                       'mtime_ns=1:size=5', 5, 1, 1, 1)",
            [],
        )
        .expect("inventory row");
    connection
        .execute(
            "INSERT INTO parse_cache(
                file_identity, source_version, parse_profile_hash, content, content_sha256,
                parser_backend, worker_lane, truncated, worker_contract_version,
                worker_version, worker_build, cached_at_ms
             ) VALUES ('C:\\work\\a.txt', 'mtime_ns=1:size=5', ?1, 'hello', ?2,
                       'rust_xlsx_bounded_v2', 'rust_core', 0,
                       'ai_daily_worker_v1', '1.0', 'legacy-build', 1)",
            rusqlite::params!["0".repeat(64), "1".repeat(64)],
        )
        .expect("legacy parse cache row");
    connection
        .execute(
            "INSERT INTO scan_file_results(
                scan_run_id, file_identity, relative_path, source_version, parse_profile_hash,
                cache_status, cache_miss_reason, parse_status, parser_backend, worker_lane,
                truncated, content_sha256, primary_duration_ms, fallback_duration_ms,
                parse_duration_ms, failure_class, fallback_backend, fallback_reason_code,
                error_code, error_message, error_retryable, error_stage, error_file_path,
                error_backend
             ) VALUES (1, 'C:\\work\\a.txt', 'a.txt', 'mtime_ns=1:size=5', ?1,
                       'fresh', '', 'success', 'rust_xlsx_bounded_v2', 'rust_core',
                       0, ?2, 0, 0, 1, '', '', '', NULL, NULL, NULL, NULL, NULL, NULL)",
            rusqlite::params!["5".repeat(64), "6".repeat(64)],
        )
        .expect("file result row");
}

fn insert_context_run(connection: &rusqlite::Connection, status: &str, final_context: &str) {
    connection
        .execute(
            "INSERT INTO context_runs(
                context_run_id, scan_run_id, context_profile_hash, status,
                final_context, context_sha256, source_file_count, success_count,
                timeout_count, included_file_count, omitted_file_count,
                error_file_count, input_chars, output_chars, total_duration_ms,
                discovery_duration_ms, parse_duration_ms, compression_duration_ms,
                created_at_ms
             ) VALUES (1, 1, ?1, ?2, ?3, ?4, 1, 1, 0, 1, 0, 0, 1, 1, 1, 0, 0, 0, 1)",
            rusqlite::params!["3".repeat(64), status, final_context, "4".repeat(64)],
        )
        .expect("context run row");
}

const SUCCESS_ENVELOPE: &str = r##"{
    "contract": "ai_daily_context",
    "protocol_version": 1,
    "request_id": "123e4567-e89b-42d3-a456-426614174000",
    "engine_version": "test",
    "engine_build": "test-build",
    "status": "ok",
    "file_context": "# daily report\n- shipped scanner foundation",
    "summary": {
        "source_file_count": 1, "success_count": 1, "timeout_count": 0,
        "included_file_count": 1, "omitted_file_count": 0, "error_file_count": 0,
        "input_chars": 1, "output_chars": 1, "total_duration_ms": 1,
        "discovery_duration_ms": 0, "parse_duration_ms": 0, "compression_duration_ms": 0
    },
    "scan_run_id": 1,
    "context_run_id": 1,
    "warnings": [],
    "error": null
}"##;

const PARTIAL_ENVELOPE: &str = r##"{
    "contract": "ai_daily_context",
    "protocol_version": 1,
    "request_id": "223e4567-e89b-42d3-a456-426614174001",
    "engine_version": "test",
    "engine_build": "test-build",
    "status": "partial",
    "file_context": "# partial report\n- recovered context",
    "summary": {
        "source_file_count": 2, "success_count": 1, "timeout_count": 0,
        "included_file_count": 1, "omitted_file_count": 0, "error_file_count": 1,
        "input_chars": 2, "output_chars": 2, "total_duration_ms": 1,
        "discovery_duration_ms": 0, "parse_duration_ms": 0, "compression_duration_ms": 0
    },
    "scan_run_id": 1,
    "context_run_id": 1,
    "warnings": [
        {
            "error_code": "PARSER_FAILED",
            "message": "one file failed",
            "retryable": false,
            "stage": "parse",
            "file_path": null,
            "backend": null
        }
    ],
    "error": null
}"##;

const ERROR_ENVELOPE: &str = r##"{
    "contract": "ai_daily_context",
    "protocol_version": 1,
    "request_id": "323e4567-e89b-42d3-a456-426614174002",
    "engine_version": "test",
    "engine_build": "test-build",
    "status": "error",
    "file_context": "",
    "summary": {
        "source_file_count": 0, "success_count": 0, "timeout_count": 0,
        "included_file_count": 0, "omitted_file_count": 0, "error_file_count": 0,
        "input_chars": 0, "output_chars": 0, "total_duration_ms": 1,
        "discovery_duration_ms": 0, "parse_duration_ms": 0, "compression_duration_ms": 0
    },
    "scan_run_id": 1,
    "context_run_id": null,
    "warnings": [],
    "error": {
        "error_code": "PARSER_FAILED",
        "message": "scanner could not start",
        "retryable": false,
        "stage": "parse",
        "file_path": null,
        "backend": null
    }
}"##;

fn insert_success_run(connection: &rusqlite::Connection, request_id: &str) {
    insert_scan_run(
        connection,
        request_id,
        "success",
        Some(2),
        Some(SUCCESS_ENVELOPE),
    );
    insert_common_rows(connection);
    insert_context_run(
        connection,
        "success",
        "# daily report\n- shipped scanner foundation",
    );
}

fn insert_partial_run(connection: &rusqlite::Connection, request_id: &str) {
    insert_scan_run(
        connection,
        request_id,
        "partial",
        Some(2),
        Some(PARTIAL_ENVELOPE),
    );
    insert_common_rows(connection);
    insert_context_run(
        connection,
        "partial",
        "# partial report\n- recovered context",
    );
}

fn insert_error_run(connection: &rusqlite::Connection, request_id: &str) {
    insert_scan_run(
        connection,
        request_id,
        "error",
        Some(2),
        Some(ERROR_ENVELOPE),
    );
    insert_common_rows(connection);
}

fn insert_running_run(connection: &rusqlite::Connection, request_id: &str) {
    insert_scan_run(connection, request_id, "running", None, None);
}

#[test]
fn fresh_v2_db_has_incremental_vacuum_and_new_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scan_index_v2.sqlite3");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    configure_connection(&conn).unwrap();
    migrate(&mut conn).unwrap();
    let ver: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, LATEST_USER_VERSION);
    let vacuum: i64 = conn
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .unwrap();
    assert_eq!(vacuum, 2, "auto_vacuum must be INCREMENTAL (2)");
    for table in [
        "classification_cache",
        "context_artifacts",
        "context_artifact_files",
        "context_artifact_decisions",
        "schema_migration_history",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "missing table {table}");
    }
    let origin: String = conn
        .query_row(
            "SELECT origin FROM schema_migration_history WHERE user_version=?1",
            [LATEST_USER_VERSION],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(origin, "created_empty");
}

#[test]
fn migrated_v1_rows_are_audited_as_migrated_and_caches_invalidated() {
    let (_directory, mut conn) = open_v1_fixture();

    upgrade_v1_to_v2(&mut conn, UPGRADE_REQUEST_ID).expect("v1 database upgrades to v2");

    let ver: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 2);

    let migrated_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM scan_runs WHERE audit_provenance_version='migrated_v1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        migrated_count, 1,
        "terminal run must be audited as migrated_v1"
    );

    let parse_cache_count: i64 = conn
        .query_row("SELECT count(*) FROM parse_cache", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        parse_cache_count, 0,
        "legacy parse cache rows must be deleted"
    );

    let history_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_migration_history WHERE origin='upgraded_v1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(history_count, 1, "upgraded_v1 history row must be recorded");

    let upgrade_request_id: Option<String> = conn
        .query_row(
            "SELECT upgrade_request_id FROM schema_migration_history WHERE user_version=2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        upgrade_request_id.is_some(),
        "upgraded_v1 history row must carry an upgrade_request_id"
    );

    // The migrated envelope body is extracted to a payload artifact and the
    // file_context is removed from the scan_runs metadata JSON.
    let metadata_json: Option<String> = conn
        .query_row(
            "SELECT final_envelope_metadata_json FROM scan_runs WHERE scan_run_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_json.as_deref().expect("metadata JSON present")).unwrap();
    assert!(
        metadata.get("file_context").is_none(),
        "file_context must be removed from the metadata JSON"
    );
    let artifact_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM context_artifacts WHERE snapshot_eligible=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        artifact_count, 1,
        "success run must produce one payload artifact"
    );
    let artifact_id: Option<i64> = conn
        .query_row(
            "SELECT artifact_id FROM context_runs WHERE scan_run_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        artifact_id.is_some(),
        "context_runs must link the migrated payload artifact"
    );
    // The migrated artifact's semantic_summary_json must be SemanticSummary-shaped
    // (spec Part 5.1), so the store's replay/artifact reader can load it.
    let semantic_summary_json: String = conn
        .query_row(
            "SELECT semantic_summary_json FROM context_artifacts WHERE artifact_id=?1",
            [artifact_id.unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    let migrated_summary: ai_daily_scanner_core::artifact::SemanticSummary =
        serde_json::from_str(&semantic_summary_json)
            .expect("migrated semantic summary must deserialize as SemanticSummary");
    assert_eq!(migrated_summary.source_file_count, 1);
    assert_eq!(migrated_summary.success_count, 1);

    // Payload artifacts (snapshot_eligible=0) never carry semantic rows; the
    // body lives only in the artifact's final_context.
    let artifact_files_count: i64 = conn
        .query_row("SELECT count(*) FROM context_artifact_files", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        artifact_files_count, 0,
        "migrated payload artifacts carry no artifact file rows"
    );
    let artifact_decisions_count: i64 = conn
        .query_row("SELECT count(*) FROM context_artifact_decisions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        artifact_decisions_count, 0,
        "migrated payload artifacts carry no artifact decision rows"
    );

    // The file_inventory rebuild preserves the inventory row.
    let inventory_count: i64 = conn
        .query_row("SELECT count(*) FROM file_inventory", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        inventory_count, 1,
        "inventory rows must survive the rebuild"
    );

    // The legacy cache evidence is moved to the nullable legacy columns.
    let legacy_status: Option<String> = conn
        .query_row(
            "SELECT legacy_cache_status FROM scan_file_results WHERE scan_run_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let legacy_reason: Option<String> = conn
        .query_row(
            "SELECT legacy_cache_miss_reason FROM scan_file_results WHERE scan_run_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy_status.as_deref(), Some("fresh"));
    assert_eq!(legacy_reason.as_deref(), Some(""));
}

#[test]
fn partial_run_migrates_to_payload_artifact_with_verbatim_warnings() {
    let (_directory, mut conn) = build_v1_fixture(FixtureRunKind::Partial);

    upgrade_v1_to_v2(&mut conn, UPGRADE_REQUEST_ID).expect("partial v1 run upgrades to v2");

    let artifact_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM context_artifacts WHERE snapshot_eligible=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        artifact_count, 1,
        "partial run must produce one payload artifact"
    );
    let files_count: i64 = conn
        .query_row("SELECT count(*) FROM context_artifact_files", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        files_count, 0,
        "payload artifacts carry no artifact file rows"
    );
    let decisions_count: i64 = conn
        .query_row("SELECT count(*) FROM context_artifact_decisions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        decisions_count, 0,
        "payload artifacts carry no artifact decision rows"
    );

    // The old Envelope warnings are replayed verbatim in the metadata JSON.
    let metadata_json: Option<String> = conn
        .query_row(
            "SELECT final_envelope_metadata_json FROM scan_runs WHERE scan_run_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_json.as_deref().expect("metadata JSON present")).unwrap();
    assert!(
        metadata.get("file_context").is_none(),
        "file_context must be removed from the metadata JSON"
    );
    let warnings = metadata.get("warnings").expect("warnings field present");
    let warning = warnings.get(0).expect("first warning present");
    assert_eq!(warning.get("error_code").unwrap(), "PARSER_FAILED");
}

#[test]
fn error_run_migrates_without_artifact() {
    let (_directory, mut conn) = build_v1_fixture(FixtureRunKind::Error);

    upgrade_v1_to_v2(&mut conn, UPGRADE_REQUEST_ID).expect("error v1 run upgrades to v2");

    let artifact_count: i64 = conn
        .query_row("SELECT count(*) FROM context_artifacts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(artifact_count, 0, "error run must not produce an artifact");
    let context_count: i64 = conn
        .query_row("SELECT count(*) FROM context_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(context_count, 0, "v1 error runs carry no context_runs row");
    let provenance: String = conn
        .query_row(
            "SELECT audit_provenance_version FROM scan_runs WHERE scan_run_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        provenance, "migrated_v1",
        "error run must be audited as migrated_v1"
    );
}

#[test]
fn nonterminal_v1_rows_are_audited_as_migrated_v1() {
    let (_directory, mut conn) = build_v1_fixture(FixtureRunKind::Running);

    upgrade_v1_to_v2(&mut conn, UPGRADE_REQUEST_ID).expect("v1 database upgrades to v2");

    let provenance: String = conn
        .query_row(
            "SELECT audit_provenance_version FROM scan_runs WHERE scan_run_id=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        provenance, "migrated_v1",
        "pre-upgrade running/abandoned rows must not be mislabeled full_v2"
    );
}

#[test]
fn migrate_on_v2_database_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scan_index_v2.sqlite3");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    configure_connection(&conn).unwrap();
    migrate(&mut conn).unwrap();

    let history_before: i64 = conn
        .query_row("SELECT count(*) FROM schema_migration_history", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        history_before, 1,
        "fresh v2 database has exactly one history row"
    );

    migrate(&mut conn).expect("migrate on an already-v2 database must be a no-op");

    let ver: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, LATEST_USER_VERSION);
    let history_after: i64 = conn
        .query_row("SELECT count(*) FROM schema_migration_history", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        history_after, 1,
        "idempotent migrate must not add a history row"
    );
}

#[test]
fn corrupt_v1_envelope_aborts_migration_and_keeps_user_version() {
    let (_directory, mut conn) = open_v1_fixture();
    conn.execute(
        "UPDATE scan_runs SET final_envelope_json='{not valid json' WHERE scan_run_id=1",
        [],
    )
    .expect("corrupt envelope seeded");

    let result = upgrade_v1_to_v2(&mut conn, UPGRADE_REQUEST_ID);
    assert!(result.is_err(), "migration must fail on a corrupt envelope");

    let ver: i32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 1, "failed migration must keep the old user_version");

    let parse_cache_count: i64 = conn
        .query_row("SELECT count(*) FROM parse_cache", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        parse_cache_count, 1,
        "parse cache must not be deleted on rollback"
    );
    let history_table_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='schema_migration_history'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        history_table_count, 0,
        "no v2 table may survive a rolled back migration"
    );
}
