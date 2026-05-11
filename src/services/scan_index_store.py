"""SQLite 扫描索引与解析缓存存储层。"""

from __future__ import annotations

import sqlite3
from pathlib import Path


class ScanIndexStore:
    """保存扫描库存、解析缓存和扫描运行记录的最小存储层。"""

    def __init__(self, db_path: Path):
        self.db_path = Path(db_path)
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_schema()

    def _connect(self) -> sqlite3.Connection:
        """创建启用 Row 访问的 SQLite 连接。"""
        conn = sqlite3.connect(self.db_path)
        conn.row_factory = sqlite3.Row
        return conn

    def _init_schema(self) -> None:
        """初始化 Task 2 需要的最小表结构。"""
        with self._connect() as conn:
            self._migrate_existing_schema(conn)
            conn.executescript(
                """
                CREATE TABLE IF NOT EXISTS file_inventory (
                    file_identity TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    extension TEXT NOT NULL,
                    modified_date TEXT NOT NULL,
                    size_bytes INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS parse_cache (
                    file_identity TEXT NOT NULL,
                    parser_profile TEXT NOT NULL,
                    source_version TEXT NOT NULL DEFAULT '',
                    content_excerpt TEXT NOT NULL,
                    parse_status TEXT NOT NULL,
                    parse_error TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    PRIMARY KEY (file_identity, parser_profile, source_version)
                );

                CREATE TABLE IF NOT EXISTS scan_runs (
                    run_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    discovered_count INTEGER NOT NULL DEFAULT 0,
                    reused_count INTEGER NOT NULL DEFAULT 0,
                    reparsed_count INTEGER NOT NULL DEFAULT 0
                );
                """
            )

    def _migrate_existing_schema(self, conn: sqlite3.Connection) -> None:
        """迁移早期 Task 2 索引库，避免列名和缓存版本契约漂移。"""
        table_names = self._list_table_names(conn)
        if "file_inventory" in table_names:
            inventory_columns = self._list_column_names(conn, "file_inventory")
            if "modified_at" in inventory_columns or "modified_date" not in inventory_columns:
                modified_date_expr = (
                    "modified_date"
                    if "modified_date" in inventory_columns
                    else "modified_at"
                )
                conn.executescript(
                    f"""
                    ALTER TABLE file_inventory RENAME TO file_inventory_legacy;

                    CREATE TABLE file_inventory (
                        file_identity TEXT PRIMARY KEY,
                        path TEXT NOT NULL,
                        extension TEXT NOT NULL,
                        modified_date TEXT NOT NULL,
                        size_bytes INTEGER NOT NULL
                    );

                    INSERT INTO file_inventory (
                        file_identity,
                        path,
                        extension,
                        modified_date,
                        size_bytes
                    )
                    SELECT
                        file_identity,
                        path,
                        extension,
                        {modified_date_expr},
                        size_bytes
                    FROM file_inventory_legacy;

                    DROP TABLE file_inventory_legacy;
                    """
                )

        if "parse_cache" in table_names:
            cache_columns = self._list_column_names(conn, "parse_cache")
            cache_primary_key = self._list_primary_key_columns(conn, "parse_cache")
            if "source_version" not in cache_columns or cache_primary_key != [
                "file_identity",
                "parser_profile",
                "source_version",
            ]:
                source_version_expr = (
                    "source_version" if "source_version" in cache_columns else "''"
                )
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
                        updated_at
                    )
                    SELECT
                        file_identity,
                        parser_profile,
                        {source_version_expr},
                        content_excerpt,
                        parse_status,
                        parse_error,
                        {updated_at_expr}
                    FROM parse_cache_legacy;

                    DROP TABLE parse_cache_legacy;
                    """
                )

    def _list_table_names(self, conn: sqlite3.Connection) -> set[str]:
        """列出现有表名。"""
        rows = conn.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table'"
        ).fetchall()
        return {str(row["name"]) for row in rows}

    def _list_column_names(self, conn: sqlite3.Connection, table_name: str) -> set[str]:
        """列出指定表字段名。"""
        rows = conn.execute(f"PRAGMA table_info({table_name})").fetchall()
        return {str(row["name"]) for row in rows}

    def _list_primary_key_columns(
        self,
        conn: sqlite3.Connection,
        table_name: str,
    ) -> list[str]:
        """按主键顺序列出字段名。"""
        rows = conn.execute(f"PRAGMA table_info({table_name})").fetchall()
        primary_key_rows = [row for row in rows if int(row["pk"]) > 0]
        primary_key_rows.sort(key=lambda row: int(row["pk"]))
        return [str(row["name"]) for row in primary_key_rows]

    def list_tables(self) -> set[str]:
        """返回当前 SQLite 文件内的表名集合。"""
        with self._connect() as conn:
            rows = conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            ).fetchall()
        return {str(row["name"]) for row in rows}

    def upsert_parse_cache(
        self,
        file_identity: str,
        parser_profile: str,
        content_excerpt: str,
        parse_status: str,
        parse_error: str,
        source_version: str = "",
    ) -> None:
        """按文件身份和 parser profile 写入或更新解析缓存。"""
        with self._connect() as conn:
            conn.execute(
                """
                INSERT INTO parse_cache (
                    file_identity,
                    parser_profile,
                    source_version,
                    content_excerpt,
                    parse_status,
                    parse_error
                )
                VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT(file_identity, parser_profile, source_version) DO UPDATE SET
                    content_excerpt = excluded.content_excerpt,
                    parse_status = excluded.parse_status,
                    parse_error = excluded.parse_error,
                    updated_at = CURRENT_TIMESTAMP
                """,
                (
                    file_identity,
                    parser_profile,
                    source_version,
                    content_excerpt,
                    parse_status,
                    parse_error,
                ),
            )

    def has_fresh_cache(
        self,
        file_identity: str,
        parser_profile: str,
        source_version: str = "",
    ) -> bool:
        """判断指定文件身份、parser profile 和文件版本是否存在缓存。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT 1
                FROM parse_cache
                WHERE file_identity = ?
                    AND parser_profile = ?
                    AND source_version = ?
                """,
                (file_identity, parser_profile, source_version),
            ).fetchone()
        return row is not None

    def load_parse_cache(
        self,
        file_identity: str,
        parser_profile: str,
        source_version: str = "",
    ) -> dict[str, str]:
        """读取解析缓存，缺失时抛出 KeyError。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT content_excerpt, parse_status, parse_error, source_version
                FROM parse_cache
                WHERE file_identity = ?
                    AND parser_profile = ?
                    AND source_version = ?
                """,
                (file_identity, parser_profile, source_version),
            ).fetchone()

        if row is None:
            raise KeyError((file_identity, parser_profile, source_version))

        return {
            "content_excerpt": str(row["content_excerpt"]),
            "parse_status": str(row["parse_status"]),
            "parse_error": str(row["parse_error"]),
        }
