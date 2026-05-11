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
            conn.executescript(
                """
                CREATE TABLE IF NOT EXISTS file_inventory (
                    file_identity TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    extension TEXT NOT NULL,
                    modified_at TEXT NOT NULL,
                    size_bytes INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS parse_cache (
                    file_identity TEXT NOT NULL,
                    parser_profile TEXT NOT NULL,
                    content_excerpt TEXT NOT NULL,
                    parse_status TEXT NOT NULL,
                    parse_error TEXT NOT NULL,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    PRIMARY KEY (file_identity, parser_profile)
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
    ) -> None:
        """按文件身份和 parser profile 写入或更新解析缓存。"""
        with self._connect() as conn:
            conn.execute(
                """
                INSERT INTO parse_cache (
                    file_identity,
                    parser_profile,
                    content_excerpt,
                    parse_status,
                    parse_error
                )
                VALUES (?, ?, ?, ?, ?)
                ON CONFLICT(file_identity, parser_profile) DO UPDATE SET
                    content_excerpt = excluded.content_excerpt,
                    parse_status = excluded.parse_status,
                    parse_error = excluded.parse_error,
                    updated_at = CURRENT_TIMESTAMP
                """,
                (
                    file_identity,
                    parser_profile,
                    content_excerpt,
                    parse_status,
                    parse_error,
                ),
            )

    def has_fresh_cache(self, file_identity: str, parser_profile: str) -> bool:
        """判断指定文件身份和 parser profile 是否存在缓存。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT 1
                FROM parse_cache
                WHERE file_identity = ? AND parser_profile = ?
                """,
                (file_identity, parser_profile),
            ).fetchone()
        return row is not None

    def load_parse_cache(
        self,
        file_identity: str,
        parser_profile: str,
    ) -> dict[str, str]:
        """读取解析缓存，缺失时抛出 KeyError。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT content_excerpt, parse_status, parse_error
                FROM parse_cache
                WHERE file_identity = ? AND parser_profile = ?
                """,
                (file_identity, parser_profile),
            ).fetchone()

        if row is None:
            raise KeyError(file_identity)

        return {
            "content_excerpt": str(row["content_excerpt"]),
            "parse_status": str(row["parse_status"]),
            "parse_error": str(row["parse_error"]),
        }
