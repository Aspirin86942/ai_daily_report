"""SQLite 扫描索引与解析缓存存储层。"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass
from datetime import date
from pathlib import Path

from .scan_metrics import ExtensionMetrics, ScanRunMetrics


@dataclass(slots=True)
class InventoryItem:
    """库存查询返回的 typed 文件元数据。"""

    file_identity: str
    path: Path
    extension: str
    modified_date: date
    size_bytes: int
    source_version: str


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
        """初始化扫描索引、缓存和 checkpoint 占位表。"""
        with self._connect() as conn:
            self._migrate_existing_schema(conn)
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
                """
            )
            self._migrate_scan_metrics_schema(conn)

    def _migrate_existing_schema(self, conn: sqlite3.Connection) -> None:
        """迁移早期 Task 2 索引库，避免列名和缓存版本契约漂移。"""
        table_names = self._list_table_names(conn)
        if "file_inventory" in table_names:
            inventory_columns = self._list_column_names(conn, "file_inventory")
            if (
                "modified_at" in inventory_columns
                or "modified_date" not in inventory_columns
                or "source_version" not in inventory_columns
            ):
                modified_date_expr = (
                    "modified_date"
                    if "modified_date" in inventory_columns
                    else "modified_at"
                )
                source_version_expr = (
                    "source_version" if "source_version" in inventory_columns else "''"
                )
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

    def _migrate_scan_metrics_schema(self, conn: sqlite3.Connection) -> None:
        """为旧 scan_runs 表补齐性能指标列。"""
        table_names = self._list_table_names(conn)
        if "scan_runs" not in table_names:
            return

        existing_columns = self._list_column_names(conn, "scan_runs")
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
                conn.execute(
                    f"ALTER TABLE scan_runs ADD COLUMN {column_name} {column_def}"
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

    def save_checkpoint(self, discovery_key: str, checkpoint_value: str) -> None:
        """保存 discovery checkpoint 占位值，后续任务可在此接入增量扫描。"""
        with self._connect() as conn:
            conn.execute(
                """
                INSERT INTO discovery_checkpoints (discovery_key, checkpoint_value)
                VALUES (?, ?)
                ON CONFLICT(discovery_key) DO UPDATE SET
                    checkpoint_value = excluded.checkpoint_value
                """,
                (discovery_key, checkpoint_value),
            )

    def save_scan_run_metrics(
        self,
        discovered_count: int | None = None,
        reused_count: int | None = None,
        reparsed_count: int | None = None,
        *,
        run_metrics: ScanRunMetrics | None = None,
        extension_metrics: list[ExtensionMetrics] | None = None,
    ) -> int:
        """保存单次扫描指标；兼容旧三计数调用并支持完整性能指标。"""
        if run_metrics is None:
            if (
                discovered_count is None
                or reused_count is None
                or reparsed_count is None
            ):
                raise TypeError(
                    "save_scan_run_metrics requires either run_metrics "
                    "or discovered/reused/reparsed counts"
                )
            run_metrics = ScanRunMetrics(
                discovered_count=discovered_count,
                reused_count=reused_count,
                reparsed_count=reparsed_count,
            )

        detail_extension_metrics = (
            extension_metrics
            if extension_metrics is not None
            else run_metrics.extension_metrics
        )
        with self._connect() as conn:
            cursor = conn.execute(
                """
                INSERT INTO scan_runs (
                    discovered_count,
                    reused_count,
                    reparsed_count,
                    total_duration_ms,
                    discovery_duration_ms,
                    inventory_cache_duration_ms,
                    parse_duration_ms,
                    aggregation_duration_ms,
                    success_count,
                    error_count,
                    timeout_count
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    run_metrics.discovered_count,
                    run_metrics.reused_count,
                    run_metrics.reparsed_count,
                    run_metrics.total_duration_ms,
                    run_metrics.discovery_duration_ms,
                    run_metrics.inventory_cache_duration_ms,
                    run_metrics.parse_duration_ms,
                    run_metrics.aggregation_duration_ms,
                    run_metrics.success_count,
                    run_metrics.error_count,
                    run_metrics.timeout_count,
                ),
            )
            run_id = int(cursor.lastrowid)
            conn.executemany(
                """
                INSERT INTO scan_extension_metrics (
                    run_id,
                    extension,
                    file_count,
                    parse_duration_ms,
                    success_count,
                    error_count,
                    timeout_count
                )
                VALUES (?, ?, ?, ?, ?, ?, ?)
                """,
                [
                    (
                        run_id,
                        item.extension,
                        item.file_count,
                        item.parse_duration_ms,
                        item.success_count,
                        item.error_count,
                        item.timeout_count,
                    )
                    for item in detail_extension_metrics
                ],
            )
        return run_id

    def latest_scan_run(self) -> dict[str, int]:
        """读取最新一条扫描运行指标；缺失时显式抛 KeyError。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT discovered_count, reused_count, reparsed_count
                FROM scan_runs
                ORDER BY run_id DESC
                LIMIT 1
                """
            ).fetchone()

        if row is None:
            raise KeyError("scan_runs")

        return {
            "discovered_count": int(row["discovered_count"]),
            "reused_count": int(row["reused_count"]),
            "reparsed_count": int(row["reparsed_count"]),
        }

    def latest_scan_run_detail(self) -> dict[str, int]:
        """读取最新一条完整扫描指标，保留 run_id 便于查询扩展名明细。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT
                    run_id,
                    discovered_count,
                    reused_count,
                    reparsed_count,
                    total_duration_ms,
                    discovery_duration_ms,
                    inventory_cache_duration_ms,
                    parse_duration_ms,
                    aggregation_duration_ms,
                    success_count,
                    error_count,
                    timeout_count
                FROM scan_runs
                ORDER BY run_id DESC
                LIMIT 1
                """
            ).fetchone()

        if row is None:
            raise KeyError("scan_runs")

        return {
            "run_id": int(row["run_id"]),
            "discovered_count": int(row["discovered_count"]),
            "reused_count": int(row["reused_count"]),
            "reparsed_count": int(row["reparsed_count"]),
            "total_duration_ms": int(row["total_duration_ms"]),
            "discovery_duration_ms": int(row["discovery_duration_ms"]),
            "inventory_cache_duration_ms": int(row["inventory_cache_duration_ms"]),
            "parse_duration_ms": int(row["parse_duration_ms"]),
            "aggregation_duration_ms": int(row["aggregation_duration_ms"]),
            "success_count": int(row["success_count"]),
            "error_count": int(row["error_count"]),
            "timeout_count": int(row["timeout_count"]),
        }

    def list_extension_metrics(self, run_id: int) -> list[ExtensionMetrics]:
        """按扩展名读取某次 scan run 的重解析明细。"""
        with self._connect() as conn:
            rows = conn.execute(
                """
                SELECT
                    extension,
                    file_count,
                    parse_duration_ms,
                    success_count,
                    error_count,
                    timeout_count
                FROM scan_extension_metrics
                WHERE run_id = ?
                ORDER BY extension
                """,
                (run_id,),
            ).fetchall()

        return [
            ExtensionMetrics(
                extension=str(row["extension"]),
                file_count=int(row["file_count"]),
                parse_duration_ms=int(row["parse_duration_ms"]),
                success_count=int(row["success_count"]),
                error_count=int(row["error_count"]),
                timeout_count=int(row["timeout_count"]),
            )
            for row in rows
        ]

    def load_checkpoint(self, discovery_key: str) -> str | None:
        """读取 checkpoint；缺失时返回 None，避免上层误判为空字符串。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT checkpoint_value
                FROM discovery_checkpoints
                WHERE discovery_key = ?
                """,
                (discovery_key,),
            ).fetchone()

        if row is None:
            return None
        return str(row["checkpoint_value"])

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
        """只把成功解析缓存视为 fresh，error cache 仅保留审计用途。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT 1
                FROM parse_cache
                WHERE file_identity = ?
                    AND parser_profile = ?
                    AND source_version = ?
                    AND parse_status = 'success'
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

    def replace_inventory(self, items: list[dict[str, object]]) -> None:
        """用一次 bootstrap 快照整体替换当前库存。"""
        with self._connect() as conn:
            conn.execute("DELETE FROM file_inventory")
            conn.executemany(
                """
                INSERT INTO file_inventory (
                    file_identity,
                    path,
                    extension,
                    modified_date,
                    size_bytes,
                    source_version
                )
                VALUES (?, ?, ?, ?, ?, ?)
                """,
                [
                    (
                        str(item["file_identity"]),
                        str(item["path"]),
                        str(item["extension"]),
                        str(item["modified_date"]),
                        int(item["size_bytes"]),
                        str(item.get("source_version", "")),
                    )
                    for item in items
                ],
            )

    def query_inventory(
        self,
        start_date: date,
        end_date: date,
    ) -> list[InventoryItem]:
        """按修改日期闭区间读取库存快照。"""
        with self._connect() as conn:
            rows = conn.execute(
                """
                SELECT
                    file_identity,
                    path,
                    extension,
                    modified_date,
                    size_bytes,
                    source_version
                FROM file_inventory
                WHERE modified_date >= ? AND modified_date <= ?
                ORDER BY path, file_identity
                """,
                (start_date.isoformat(), end_date.isoformat()),
            ).fetchall()

        return [
            InventoryItem(
                file_identity=str(row["file_identity"]),
                path=Path(str(row["path"])),
                extension=str(row["extension"]),
                modified_date=date.fromisoformat(str(row["modified_date"])),
                size_bytes=int(row["size_bytes"]),
                source_version=str(row["source_version"]),
            )
            for row in rows
        ]
