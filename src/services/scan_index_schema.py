"""SQLite schema and migration helpers for scan index storage."""

from __future__ import annotations

import sqlite3


def init_scan_index_schema(conn: sqlite3.Connection) -> None:
    """初始化扫描索引、缓存、运行指标和上下文审计表。"""
    migrate_file_inventory_schema(conn)
    migrate_parse_cache_schema(conn)
    conn.executescript(
        """
        CREATE TABLE IF NOT EXISTS file_inventory (
            file_identity TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            extension TEXT NOT NULL,
            modified_date TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            source_version TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS parse_cache (
            file_identity TEXT NOT NULL,
            parser_profile TEXT NOT NULL,
            source_version TEXT NOT NULL DEFAULT '',
            content_excerpt TEXT NOT NULL,
            parse_status TEXT NOT NULL,
            parse_error TEXT NOT NULL,
            parser_backend TEXT NOT NULL DEFAULT '',
            truncated INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (file_identity, parser_profile, source_version)
        );

        CREATE TABLE IF NOT EXISTS scan_runs (
            run_id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            discovered_count INTEGER NOT NULL DEFAULT 0,
            reused_count INTEGER NOT NULL DEFAULT 0,
            reparsed_count INTEGER NOT NULL DEFAULT 0,
            total_duration_ms INTEGER NOT NULL DEFAULT 0,
            discovery_duration_ms INTEGER NOT NULL DEFAULT 0,
            inventory_cache_duration_ms INTEGER NOT NULL DEFAULT 0,
            parse_duration_ms INTEGER NOT NULL DEFAULT 0,
            aggregation_duration_ms INTEGER NOT NULL DEFAULT 0,
            success_count INTEGER NOT NULL DEFAULT 0,
            error_count INTEGER NOT NULL DEFAULT 0,
            timeout_count INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS scan_extension_metrics (
            run_id INTEGER NOT NULL,
            extension TEXT NOT NULL,
            file_count INTEGER NOT NULL DEFAULT 0,
            parse_duration_ms INTEGER NOT NULL DEFAULT 0,
            success_count INTEGER NOT NULL DEFAULT 0,
            error_count INTEGER NOT NULL DEFAULT 0,
            timeout_count INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (run_id, extension),
            FOREIGN KEY (run_id) REFERENCES scan_runs(run_id)
        );

        CREATE TABLE IF NOT EXISTS discovery_checkpoints (
            discovery_key TEXT PRIMARY KEY,
            checkpoint_value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS context_runs (
            context_run_id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            report_mode TEXT NOT NULL,
            start_date TEXT NOT NULL,
            end_date TEXT NOT NULL,
            compression_profile TEXT NOT NULL,
            context_profile_key TEXT NOT NULL,
            scan_run_id INTEGER,
            source_file_count INTEGER NOT NULL DEFAULT 0,
            included_file_count INTEGER NOT NULL DEFAULT 0,
            omitted_file_count INTEGER NOT NULL DEFAULT 0,
            metadata_only_count INTEGER NOT NULL DEFAULT 0,
            compressed_file_count INTEGER NOT NULL DEFAULT 0,
            error_file_count INTEGER NOT NULL DEFAULT 0,
            truncated_file_count INTEGER NOT NULL DEFAULT 0,
            input_chars INTEGER NOT NULL DEFAULT 0,
            output_chars INTEGER NOT NULL DEFAULT 0,
            duration_ms INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'success',
            error TEXT NOT NULL DEFAULT '',
            FOREIGN KEY (scan_run_id) REFERENCES scan_runs(run_id)
        );

        CREATE TABLE IF NOT EXISTS context_decisions (
            context_decision_id INTEGER PRIMARY KEY AUTOINCREMENT,
            context_run_id INTEGER NOT NULL,
            file_identity TEXT NOT NULL DEFAULT '',
            path TEXT NOT NULL,
            extension TEXT NOT NULL,
            size_bytes INTEGER,
            parser_backend TEXT NOT NULL DEFAULT '',
            worker_lane TEXT NOT NULL DEFAULT '',
            cache_status TEXT NOT NULL DEFAULT '',
            action TEXT NOT NULL,
            reason TEXT NOT NULL,
            priority INTEGER NOT NULL DEFAULT 0,
            input_chars INTEGER NOT NULL DEFAULT 0,
            output_chars INTEGER NOT NULL DEFAULT 0,
            truncated INTEGER NOT NULL DEFAULT 0,
            error TEXT NOT NULL DEFAULT '',
            FOREIGN KEY (context_run_id)
                REFERENCES context_runs(context_run_id)
        );
        """
    )
    migrate_scan_metrics_schema(conn)


def migrate_file_inventory_schema(conn: sqlite3.Connection) -> None:
    """迁移早期 Task 2 索引库，避免列名和缓存版本契约漂移。"""
    table_names = list_table_names(conn)
    if "file_inventory" not in table_names:
        return

    inventory_columns = list_column_names(conn, "file_inventory")
    if (
        "modified_at" not in inventory_columns
        and "modified_date" in inventory_columns
        and "source_version" in inventory_columns
    ):
        return

    modified_date_expr = (
        "modified_date" if "modified_date" in inventory_columns else "modified_at"
    )
    source_version_expr = "source_version" if "source_version" in inventory_columns else "''"
    conn.executescript(
        f"""
        ALTER TABLE file_inventory RENAME TO file_inventory_legacy;

        CREATE TABLE file_inventory (
            file_identity TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            extension TEXT NOT NULL,
            modified_date TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            source_version TEXT NOT NULL DEFAULT ''
        );

        INSERT INTO file_inventory (
            file_identity,
            path,
            extension,
            modified_date,
            size_bytes,
            source_version
        )
        SELECT
            file_identity,
            path,
            extension,
            {modified_date_expr},
            size_bytes,
            {source_version_expr}
        FROM file_inventory_legacy;

        DROP TABLE file_inventory_legacy;
        """
    )


def migrate_parse_cache_schema(conn: sqlite3.Connection) -> None:
    """迁移 parse_cache，补齐 source_version 主键和 parser metadata 字段。"""
    table_names = list_table_names(conn)
    if "parse_cache" not in table_names:
        return

    cache_columns = list_column_names(conn, "parse_cache")
    cache_primary_key = list_primary_key_columns(conn, "parse_cache")
    expected_primary_key = [
        "file_identity",
        "parser_profile",
        "source_version",
    ]
    if "source_version" not in cache_columns or cache_primary_key != expected_primary_key:
        source_version_expr = "source_version" if "source_version" in cache_columns else "''"
        parser_backend_expr = "parser_backend" if "parser_backend" in cache_columns else "''"
        truncated_expr = "truncated" if "truncated" in cache_columns else "0"
        updated_at_expr = (
            "updated_at" if "updated_at" in cache_columns else "CURRENT_TIMESTAMP"
        )
        conn.executescript(
            f"""
            ALTER TABLE parse_cache RENAME TO parse_cache_legacy;

            CREATE TABLE parse_cache (
                file_identity TEXT NOT NULL,
                parser_profile TEXT NOT NULL,
                source_version TEXT NOT NULL DEFAULT '',
                content_excerpt TEXT NOT NULL,
                parse_status TEXT NOT NULL,
                parse_error TEXT NOT NULL,
                parser_backend TEXT NOT NULL DEFAULT '',
                truncated INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (file_identity, parser_profile, source_version)
            );

            INSERT INTO parse_cache (
                file_identity,
                parser_profile,
                source_version,
                content_excerpt,
                parse_status,
                parse_error,
                parser_backend,
                truncated,
                updated_at
            )
            SELECT
                file_identity,
                parser_profile,
                {source_version_expr},
                content_excerpt,
                parse_status,
                parse_error,
                {parser_backend_expr},
                {truncated_expr},
                {updated_at_expr}
            FROM parse_cache_legacy;

            DROP TABLE parse_cache_legacy;
            """
        )
        return

    if "parser_backend" not in cache_columns:
        conn.execute(
            "ALTER TABLE parse_cache ADD COLUMN parser_backend TEXT NOT NULL DEFAULT ''"
        )
    if "truncated" not in cache_columns:
        conn.execute(
            "ALTER TABLE parse_cache ADD COLUMN truncated INTEGER NOT NULL DEFAULT 0"
        )


def migrate_scan_metrics_schema(conn: sqlite3.Connection) -> None:
    """为旧 scan_runs 表补齐性能指标列。"""
    table_names = list_table_names(conn)
    if "scan_runs" not in table_names:
        return

    existing_columns = list_column_names(conn, "scan_runs")
    required_columns = {
        "total_duration_ms": "INTEGER NOT NULL DEFAULT 0",
        "discovery_duration_ms": "INTEGER NOT NULL DEFAULT 0",
        "inventory_cache_duration_ms": "INTEGER NOT NULL DEFAULT 0",
        "parse_duration_ms": "INTEGER NOT NULL DEFAULT 0",
        "aggregation_duration_ms": "INTEGER NOT NULL DEFAULT 0",
        "success_count": "INTEGER NOT NULL DEFAULT 0",
        "error_count": "INTEGER NOT NULL DEFAULT 0",
        "timeout_count": "INTEGER NOT NULL DEFAULT 0",
    }
    for column_name, column_def in required_columns.items():
        if column_name not in existing_columns:
            conn.execute(f"ALTER TABLE scan_runs ADD COLUMN {column_name} {column_def}")


def list_table_names(conn: sqlite3.Connection) -> set[str]:
    """列出现有表名。"""
    rows = conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'").fetchall()
    return {str(row["name"]) for row in rows}


def list_column_names(conn: sqlite3.Connection, table_name: str) -> set[str]:
    """列出指定表字段名。"""
    rows = conn.execute(f"PRAGMA table_info({table_name})").fetchall()
    return {str(row["name"]) for row in rows}


def list_primary_key_columns(
    conn: sqlite3.Connection,
    table_name: str,
) -> list[str]:
    """按主键顺序列出字段名。"""
    rows = conn.execute(f"PRAGMA table_info({table_name})").fetchall()
    primary_key_rows = [row for row in rows if int(row["pk"]) > 0]
    primary_key_rows.sort(key=lambda row: int(row["pk"]))
    return [str(row["name"]) for row in primary_key_rows]
