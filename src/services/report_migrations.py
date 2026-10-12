"""Atomic report-schema migration with a verified SQLite backup."""

from __future__ import annotations

from datetime import UTC, datetime
from hashlib import file_digest
import json
from pathlib import Path
import sqlite3
from time import monotonic
from uuid import uuid4

REPORT_DB_VERSION = 1


def execute_schema_script(connection: sqlite3.Connection, script: str) -> None:
    """Execute static DDL without executescript's implicit transaction commit."""
    statement = ""
    for line in script.splitlines(keepends=True):
        statement += line
        if sqlite3.complete_statement(statement):
            connection.execute(statement)
            statement = ""
    if statement.strip():
        raise ValueError("incomplete schema statement")


def _sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return file_digest(stream, "sha256").hexdigest()


def prepare_migration(connection: sqlite3.Connection, db_path: Path) -> None:
    """Caller holds BEGIN IMMEDIATE; a separate reader backs up committed data."""
    tables = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    if "report_schema_migrations" in tables:
        version = connection.execute("SELECT COALESCE(MAX(version), 0) FROM report_schema_migrations").fetchone()[0]
        if version > REPORT_DB_VERSION:
            raise RuntimeError("REPORT_SCHEMA_NEWER_THAN_APPLICATION")
        if version == REPORT_DB_VERSION:
            return
    if not tables:
        return
    directory = db_path.resolve().parent / "backups"
    directory.mkdir(parents=True, exist_ok=True)
    label = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    backup = directory / f"{db_path.stem}-before-report-v{REPORT_DB_VERSION}-{label}-{uuid4().hex}.sqlite3"
    backup.touch(exist_ok=False)
    started = monotonic()

    def progress(_status, _remaining, _total):
        if monotonic() - started > 30:
            raise TimeoutError("REPORT_BACKUP_DEADLINE")

    source = sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True)
    destination = sqlite3.connect(backup)
    try:
        source.backup(destination, pages=128, progress=progress, sleep=0.05)
        if destination.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise RuntimeError("REPORT_BACKUP_INTEGRITY_FAILED")
    finally:
        destination.close()
        source.close()
    metadata = {
        "migration_target_version": REPORT_DB_VERSION,
        "source_main_file_sha256": _sha256(db_path),
        "backup_snapshot_sha256": _sha256(backup),
        "integrity_check": "ok", "created_at": datetime.now(UTC).isoformat(),
        "source_hash_scope": "main_file_only; backup includes committed WAL data",
    }
    with backup.with_suffix(".json").open("x", encoding="utf-8", newline="\n") as stream:
        stream.write(json.dumps(metadata, ensure_ascii=False, indent=2) + "\n")


def migrate_evidence_schema(connection: sqlite3.Connection) -> None:
    execute_schema_script(connection, """
        CREATE TABLE IF NOT EXISTS report_schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS report_metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS report_runs (
            run_id TEXT PRIMARY KEY,
            scope_id TEXT NOT NULL CHECK(scope_id='personal_work'),
            target_date TEXT NOT NULL,
            timezone TEXT NOT NULL CHECK(timezone='Asia/Shanghai'),
            run_kind TEXT NOT NULL DEFAULT 'report',
            context_json TEXT NOT NULL,
            config_fingerprint TEXT NOT NULL,
            input_fingerprint TEXT,
            publication_fingerprint TEXT,
            generation_id TEXT NOT NULL,
            regeneration_reason TEXT,
            phase TEXT NOT NULL DEFAULT 'created',
            status TEXT NOT NULL DEFAULT 'running',
            quality TEXT,
            snapshot_path TEXT,
            snapshot_sha256 TEXT,
            report_json TEXT,
            markdown TEXT,
            output_path TEXT,
            expected_sha256 TEXT,
            previous_sha256 TEXT,
            previous_run_id TEXT,
            error_code TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            finished_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_report_runs_business_key
            ON report_runs(scope_id, target_date, created_at);
        CREATE INDEX IF NOT EXISTS idx_report_runs_fingerprint
            ON report_runs(input_fingerprint, status);
        CREATE TABLE IF NOT EXISTS report_run_sources (
            run_id TEXT NOT NULL REFERENCES report_runs(run_id) ON DELETE CASCADE,
            source_id TEXT NOT NULL,
            metadata_json TEXT NOT NULL,
            PRIMARY KEY(run_id, source_id)
        );
        CREATE TABLE IF NOT EXISTS report_model_calls (
            call_id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT NOT NULL REFERENCES report_runs(run_id) ON DELETE CASCADE,
            generation_id TEXT NOT NULL,
            request_fingerprint TEXT NOT NULL,
            logical_step TEXT NOT NULL,
            attempt INTEGER NOT NULL,
            call_date TEXT NOT NULL,
            status TEXT NOT NULL,
            request_path TEXT,
            request_sha256 TEXT,
            response_path TEXT,
            response_sha256 TEXT,
            usage_json TEXT,
            error_code TEXT,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            duration_ms INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_report_model_calls_day ON report_model_calls(call_date);
        CREATE INDEX IF NOT EXISTS idx_report_model_calls_reuse
            ON report_model_calls(generation_id, request_fingerprint, status);
    """)
    columns = {row[1] for row in connection.execute("PRAGMA table_info(daily_reports)")}
    for name, declaration in {
        "schema_version": "INTEGER NOT NULL DEFAULT 1",
        "report_kind": "TEXT NOT NULL DEFAULT 'legacy_work_diary'",
        "current_run_id": "TEXT REFERENCES report_runs(run_id) ON DELETE RESTRICT",
    }.items():
        if name not in columns:
            connection.execute(f"ALTER TABLE daily_reports ADD COLUMN {name} {declaration}")
    connection.execute("INSERT OR IGNORE INTO report_metadata(key,value) VALUES ('scope_id','personal_work')")
    connection.execute("INSERT OR IGNORE INTO report_metadata(key,value) VALUES ('timezone','Asia/Shanghai')")
    metadata = dict(connection.execute("SELECT key,value FROM report_metadata WHERE key IN ('scope_id','timezone')"))
    if metadata != {"scope_id": "personal_work", "timezone": "Asia/Shanghai"}:
        raise RuntimeError("REPORT_SCOPE_OR_TIMEZONE_MISMATCH")
    connection.execute("INSERT OR IGNORE INTO report_schema_migrations(version,applied_at) VALUES (?,?)",
                       (REPORT_DB_VERSION, datetime.now(UTC).isoformat()))
