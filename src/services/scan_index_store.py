"""SQLite 扫描索引与解析缓存存储层。"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass
from datetime import date
from pathlib import Path

from .context_compressor import ContextDecision
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


@dataclass(frozen=True, slots=True)
class CacheProbe:
    """解释一次 parse cache freshness 判断结果。"""

    file_identity: str
    parser_profile: str
    source_version: str
    cache_status: str
    cache_miss_reason: str
    previous_source_version: str | None = None


class ScanIndexStore:
    """保存扫描库存、解析缓存和扫描运行记录的最小存储层。"""

    def __init__(self, db_path: Path):
        self.db_path = Path(db_path)
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_schema()

    def _connect(self) -> sqlite3.Connection:
        """创建启用 Row 访问和外键约束的 SQLite 连接。"""
        conn = sqlite3.connect(self.db_path)
        # SQLite 每个连接默认不执行已声明的 FK；context 审计记录必须阻止
        # 孤儿 run/decision 行，否则后续 CLI 复盘会拿到不可追溯的数据。
        conn.execute("PRAGMA foreign_keys = ON")
        conn.row_factory = sqlite3.Row
        return conn

    def _init_schema(self) -> None:
        """初始化扫描索引、缓存和 checkpoint 占位表。"""
        with self._connect() as conn:
            self._migrate_existing_schema(conn)
            self._migrate_parse_cache_schema(conn)
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

    def _migrate_parse_cache_schema(self, conn: sqlite3.Connection) -> None:
        """迁移 parse_cache，补齐 source_version 主键和 parser metadata 字段。"""
        table_names = self._list_table_names(conn)
        if "parse_cache" not in table_names:
            return

        cache_columns = self._list_column_names(conn, "parse_cache")
        cache_primary_key = self._list_primary_key_columns(conn, "parse_cache")
        expected_primary_key = [
            "file_identity",
            "parser_profile",
            "source_version",
        ]
        if "source_version" not in cache_columns or cache_primary_key != expected_primary_key:
            source_version_expr = (
                "source_version" if "source_version" in cache_columns else "''"
            )
            parser_backend_expr = (
                "parser_backend" if "parser_backend" in cache_columns else "''"
            )
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

    def _non_negative_int(self, value: int) -> int:
        """把审计计数归一为非负整数，避免异常路径污染后续统计。"""
        return max(0, int(value))

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

    def save_context_run(
        self,
        *,
        report_mode: str,
        start_date: date,
        end_date: date,
        compression_profile: str,
        context_profile_key: str,
        scan_run_id: int | None,
        source_file_count: int,
        included_file_count: int,
        omitted_file_count: int,
        metadata_only_count: int,
        compressed_file_count: int,
        error_file_count: int,
        truncated_file_count: int,
        input_chars: int,
        output_chars: int,
        duration_ms: int,
        status: str = "success",
        error: str = "",
    ) -> int:
        """保存一次 ContextScheduler 运行级审计记录并返回 run id。"""
        with self._connect() as conn:
            return self._insert_context_run(
                conn,
                report_mode=report_mode,
                start_date=start_date,
                end_date=end_date,
                compression_profile=compression_profile,
                context_profile_key=context_profile_key,
                scan_run_id=scan_run_id,
                source_file_count=source_file_count,
                included_file_count=included_file_count,
                omitted_file_count=omitted_file_count,
                metadata_only_count=metadata_only_count,
                compressed_file_count=compressed_file_count,
                error_file_count=error_file_count,
                truncated_file_count=truncated_file_count,
                input_chars=input_chars,
                output_chars=output_chars,
                duration_ms=duration_ms,
                status=status,
                error=error,
            )

    def save_context_run_with_decisions(
        self,
        *,
        report_mode: str,
        start_date: date,
        end_date: date,
        compression_profile: str,
        context_profile_key: str,
        scan_run_id: int | None,
        source_file_count: int,
        included_file_count: int,
        omitted_file_count: int,
        metadata_only_count: int,
        compressed_file_count: int,
        error_file_count: int,
        truncated_file_count: int,
        input_chars: int,
        output_chars: int,
        duration_ms: int,
        decisions: list[ContextDecision],
        status: str = "success",
        error: str = "",
    ) -> int:
        """在同一事务内保存 context run 和逐文件决策。"""
        with self._connect() as conn:
            # success run 与逐文件 decisions 必须原子写入；否则 decisions 失败时，
            # 审计表会显示本次上下文构建成功，实际却缺少解释文件取舍的明细。
            context_run_id = self._insert_context_run(
                conn,
                report_mode=report_mode,
                start_date=start_date,
                end_date=end_date,
                compression_profile=compression_profile,
                context_profile_key=context_profile_key,
                scan_run_id=scan_run_id,
                source_file_count=source_file_count,
                included_file_count=included_file_count,
                omitted_file_count=omitted_file_count,
                metadata_only_count=metadata_only_count,
                compressed_file_count=compressed_file_count,
                error_file_count=error_file_count,
                truncated_file_count=truncated_file_count,
                input_chars=input_chars,
                output_chars=output_chars,
                duration_ms=duration_ms,
                status=status,
                error=error,
            )
            self._insert_context_decisions(conn, context_run_id, decisions)
            return context_run_id

    def save_context_decisions(
        self,
        context_run_id: int,
        decisions: list[ContextDecision],
    ) -> None:
        """保存一次 ContextScheduler 的逐文件决策审计明细。"""
        with self._connect() as conn:
            self._insert_context_decisions(conn, context_run_id, decisions)

    def _insert_context_run(
        self,
        conn: sqlite3.Connection,
        *,
        report_mode: str,
        start_date: date,
        end_date: date,
        compression_profile: str,
        context_profile_key: str,
        scan_run_id: int | None,
        source_file_count: int,
        included_file_count: int,
        omitted_file_count: int,
        metadata_only_count: int,
        compressed_file_count: int,
        error_file_count: int,
        truncated_file_count: int,
        input_chars: int,
        output_chars: int,
        duration_ms: int,
        status: str = "success",
        error: str = "",
    ) -> int:
        normalized_scan_run_id = None if scan_run_id is None else int(scan_run_id)
        # 即使本次上下文构建没有文件或最终失败，也要先落 run 级记录；
        # 这是 CLI 单次运行能追溯输入规模、压缩策略和失败原因的依据。
        cursor = conn.execute(
            """
            INSERT INTO context_runs (
                report_mode,
                start_date,
                end_date,
                compression_profile,
                context_profile_key,
                scan_run_id,
                source_file_count,
                included_file_count,
                omitted_file_count,
                metadata_only_count,
                compressed_file_count,
                error_file_count,
                truncated_file_count,
                input_chars,
                output_chars,
                duration_ms,
                status,
                error
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                report_mode,
                start_date.isoformat(),
                end_date.isoformat(),
                compression_profile,
                context_profile_key,
                normalized_scan_run_id,
                self._non_negative_int(source_file_count),
                self._non_negative_int(included_file_count),
                self._non_negative_int(omitted_file_count),
                self._non_negative_int(metadata_only_count),
                self._non_negative_int(compressed_file_count),
                self._non_negative_int(error_file_count),
                self._non_negative_int(truncated_file_count),
                self._non_negative_int(input_chars),
                self._non_negative_int(output_chars),
                self._non_negative_int(duration_ms),
                status,
                error or "",
            ),
        )
        return int(cursor.lastrowid)

    def _insert_context_decisions(
        self,
        conn: sqlite3.Connection,
        context_run_id: int,
        decisions: list[ContextDecision],
    ) -> None:
        # 决策明细保留 keep/compress/omit/error 的原始原因，后续 benchmark
        # 才能解释一次 CLI 输出为何包含或省略某个文件。
        conn.executemany(
            """
            INSERT INTO context_decisions (
                context_run_id,
                file_identity,
                path,
                extension,
                size_bytes,
                parser_backend,
                worker_lane,
                cache_status,
                action,
                reason,
                priority,
                input_chars,
                output_chars,
                truncated,
                error
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            [
                (
                    int(context_run_id),
                    "",
                    decision.file_path,
                    decision.extension,
                    self._non_negative_int(decision.size_bytes),
                    decision.parser_backend or "",
                    decision.worker_lane or "",
                    decision.cache_status or "",
                    decision.action,
                    decision.reason,
                    self._non_negative_int(decision.priority),
                    self._non_negative_int(decision.input_chars),
                    self._non_negative_int(decision.output_chars),
                    int(bool(decision.truncated)),
                    decision.error or "",
                )
                for decision in decisions
            ],
        )

    def latest_context_run(self) -> dict[str, int | str | None] | None:
        """读取最新一条 context run；缺失时返回 None。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT
                    context_run_id,
                    report_mode,
                    start_date,
                    end_date,
                    compression_profile,
                    context_profile_key,
                    scan_run_id,
                    source_file_count,
                    included_file_count,
                    omitted_file_count,
                    metadata_only_count,
                    compressed_file_count,
                    error_file_count,
                    truncated_file_count,
                    input_chars,
                    output_chars,
                    duration_ms,
                    status,
                    error
                FROM context_runs
                ORDER BY context_run_id DESC
                LIMIT 1
                """
            ).fetchone()

        return None if row is None else self._context_run_row_to_dict(row)

    def get_context_run(
        self,
        context_run_id: int,
    ) -> dict[str, int | str | None] | None:
        """按 id 读取指定 context run；缺失时返回 None。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT
                    context_run_id,
                    report_mode,
                    start_date,
                    end_date,
                    compression_profile,
                    context_profile_key,
                    scan_run_id,
                    source_file_count,
                    included_file_count,
                    omitted_file_count,
                    metadata_only_count,
                    compressed_file_count,
                    error_file_count,
                    truncated_file_count,
                    input_chars,
                    output_chars,
                    duration_ms,
                    status,
                    error
                FROM context_runs
                WHERE context_run_id = ?
                LIMIT 1
                """,
                (int(context_run_id),),
            ).fetchone()

        return None if row is None else self._context_run_row_to_dict(row)

    def _context_run_row_to_dict(
        self,
        row: sqlite3.Row,
    ) -> dict[str, int | str | None]:
        """把 context run 行统一转为审计字典，避免 latest/by-id 字段漂移。"""
        scan_run_id = row["scan_run_id"]
        return {
            "context_run_id": int(row["context_run_id"]),
            "report_mode": str(row["report_mode"]),
            "start_date": str(row["start_date"]),
            "end_date": str(row["end_date"]),
            "compression_profile": str(row["compression_profile"]),
            "context_profile_key": str(row["context_profile_key"]),
            "scan_run_id": None if scan_run_id is None else int(scan_run_id),
            "source_file_count": int(row["source_file_count"]),
            "included_file_count": int(row["included_file_count"]),
            "omitted_file_count": int(row["omitted_file_count"]),
            "metadata_only_count": int(row["metadata_only_count"]),
            "compressed_file_count": int(row["compressed_file_count"]),
            "error_file_count": int(row["error_file_count"]),
            "truncated_file_count": int(row["truncated_file_count"]),
            "input_chars": int(row["input_chars"]),
            "output_chars": int(row["output_chars"]),
            "duration_ms": int(row["duration_ms"]),
            "status": str(row["status"]),
            "error": str(row["error"]),
        }

    def list_context_decisions(
        self,
        context_run_id: int,
    ) -> list[dict[str, int | str | bool | None]]:
        """按插入顺序读取某次 context run 的逐文件决策。"""
        with self._connect() as conn:
            rows = conn.execute(
                """
                SELECT
                    context_run_id,
                    file_identity,
                    path,
                    extension,
                    size_bytes,
                    parser_backend,
                    worker_lane,
                    cache_status,
                    action,
                    reason,
                    priority,
                    input_chars,
                    output_chars,
                    truncated,
                    error
                FROM context_decisions
                WHERE context_run_id = ?
                ORDER BY context_decision_id
                """,
                (int(context_run_id),),
            ).fetchall()

        return [
            {
                "context_run_id": int(row["context_run_id"]),
                "file_identity": str(row["file_identity"]),
                "path": str(row["path"]),
                "extension": str(row["extension"]),
                "size_bytes": (
                    None if row["size_bytes"] is None else int(row["size_bytes"])
                ),
                "parser_backend": str(row["parser_backend"]),
                "worker_lane": str(row["worker_lane"]),
                "cache_status": str(row["cache_status"]),
                "action": str(row["action"]),
                "reason": str(row["reason"]),
                "priority": int(row["priority"]),
                "input_chars": int(row["input_chars"]),
                "output_chars": int(row["output_chars"]),
                "truncated": bool(int(row["truncated"])),
                "error": str(row["error"]),
            }
            for row in rows
        ]

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

        return self._scan_run_detail_row_to_dict(row)

    def get_scan_run_detail(self, run_id: int) -> dict[str, int] | None:
        """按 id 读取指定 scan run detail；缺失时返回 None。"""
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
                WHERE run_id = ?
                LIMIT 1
                """,
                (int(run_id),),
            ).fetchone()

        return None if row is None else self._scan_run_detail_row_to_dict(row)

    def _scan_run_detail_row_to_dict(self, row: sqlite3.Row) -> dict[str, int]:
        """把 scan run 行统一转为完整指标字典，避免 latest/by-id 字段漂移。"""
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
        parser_backend: str = "",
        truncated: bool = False,
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
                    parse_error,
                    parser_backend,
                    truncated
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(file_identity, parser_profile, source_version) DO UPDATE SET
                    content_excerpt = excluded.content_excerpt,
                    parse_status = excluded.parse_status,
                    parse_error = excluded.parse_error,
                    parser_backend = excluded.parser_backend,
                    truncated = excluded.truncated,
                    updated_at = CURRENT_TIMESTAMP
                """,
                (
                    file_identity,
                    parser_profile,
                    source_version,
                    content_excerpt,
                    parse_status,
                    parse_error,
                    parser_backend,
                    int(bool(truncated)),
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

    def probe_parse_cache(
        self,
        file_identity: str,
        parser_profile: str,
        source_version: str = "",
    ) -> CacheProbe:
        """解释 parse cache 是否 fresh，以及不 fresh 的原因。"""
        with self._connect() as conn:
            rows = conn.execute(
                """
                SELECT parser_profile, source_version, parse_status, updated_at
                FROM parse_cache
                WHERE file_identity = ?
                ORDER BY updated_at DESC, rowid DESC
                """,
                (file_identity,),
            ).fetchall()

        if not rows:
            return CacheProbe(
                file_identity=file_identity,
                parser_profile=parser_profile,
                source_version=source_version,
                cache_status="miss",
                cache_miss_reason="new_file",
            )

        exact_rows = [
            row
            for row in rows
            if str(row["parser_profile"]) == parser_profile
            and str(row["source_version"]) == source_version
        ]
        if exact_rows:
            latest_exact = exact_rows[0]
            if str(latest_exact["parse_status"]) == "success":
                return CacheProbe(
                    file_identity=file_identity,
                    parser_profile=parser_profile,
                    source_version=source_version,
                    cache_status="fresh",
                    cache_miss_reason="",
                )
            return CacheProbe(
                file_identity=file_identity,
                parser_profile=parser_profile,
                source_version=source_version,
                cache_status="miss",
                cache_miss_reason="error_cache",
                previous_source_version=str(latest_exact["source_version"]),
            )

        same_profile_rows = [
            row
            for row in rows
            if str(row["parser_profile"]) == parser_profile
        ]
        same_profile_success = [
            row for row in same_profile_rows if str(row["parse_status"]) == "success"
        ]
        if same_profile_success:
            return CacheProbe(
                file_identity=file_identity,
                parser_profile=parser_profile,
                source_version=source_version,
                cache_status="miss",
                cache_miss_reason="source_version_changed",
                previous_source_version=str(same_profile_success[0]["source_version"]),
            )

        if same_profile_rows:
            return CacheProbe(
                file_identity=file_identity,
                parser_profile=parser_profile,
                source_version=source_version,
                cache_status="miss",
                cache_miss_reason="source_version_changed",
                previous_source_version=str(same_profile_rows[0]["source_version"]),
            )

        return CacheProbe(
            file_identity=file_identity,
            parser_profile=parser_profile,
            source_version=source_version,
            cache_status="miss",
            cache_miss_reason="parser_profile_changed",
            previous_source_version=str(rows[0]["source_version"]),
        )

    def load_parse_cache(
        self,
        file_identity: str,
        parser_profile: str,
        source_version: str = "",
    ) -> dict[str, str | bool]:
        """读取解析缓存，缺失时抛出 KeyError。"""
        with self._connect() as conn:
            row = conn.execute(
                """
                SELECT
                    content_excerpt,
                    parse_status,
                    parse_error,
                    parser_backend,
                    truncated,
                    source_version
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
            "parser_backend": str(row["parser_backend"]),
            "truncated": bool(int(row["truncated"])),
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
