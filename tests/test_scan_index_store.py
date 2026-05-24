"""测试 SQLite 扫描索引存储。"""

import sqlite3
from datetime import date
from pathlib import Path

import pytest

from src.services.scan_index_store import InventoryItem, ScanIndexStore
from src.services.scan_metrics import ExtensionMetrics, ScanRunMetrics


def test_index_store_creates_inventory_and_cache_tables(tmp_path: Path):
    """初始化索引库时应创建库存、解析缓存和扫描运行表。"""
    store = ScanIndexStore(db_path=tmp_path / "nested" / "scan_index.sqlite3")

    table_names = store.list_tables()

    assert store.db_path.exists()
    assert {
        "file_inventory",
        "parse_cache",
        "scan_runs",
        "discovery_checkpoints",
    } <= table_names


def test_parse_cache_round_trip_and_fresh_lookup(tmp_path: Path):
    """相同文件身份和 parser profile 应能命中并读回解析缓存。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    parser_profile = '{"parser_profile_version":"v1"}'

    store.upsert_parse_cache(
        file_identity="vol-1:frn-1",
        parser_profile=parser_profile,
        content_excerpt="hello",
        parse_status="success",
        parse_error="",
    )

    assert store.has_fresh_cache("vol-1:frn-1", parser_profile) is True
    assert (
        store.has_fresh_cache(
            "vol-1:frn-1",
            '{"parser_profile_version":"v2"}',
        )
        is False
    )
    cached = store.load_parse_cache("vol-1:frn-1", parser_profile)
    assert cached == {
        "content_excerpt": "hello",
        "parse_status": "success",
        "parse_error": "",
    }


def test_parse_cache_freshness_requires_matching_source_version(tmp_path: Path):
    """路径身份不变时，文件版本变化必须让缓存失效。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    parser_profile = '{"parser_profile_version":"v1"}'

    store.upsert_parse_cache(
        file_identity="bootstrap:/work/report.txt",
        parser_profile=parser_profile,
        content_excerpt="old content",
        parse_status="success",
        parse_error="",
        source_version="mtime=1:size=10",
    )

    assert (
        store.has_fresh_cache(
            "bootstrap:/work/report.txt",
            parser_profile,
            source_version="mtime=1:size=10",
        )
        is True
    )
    assert (
        store.has_fresh_cache(
            "bootstrap:/work/report.txt",
            parser_profile,
            source_version="mtime=2:size=10",
        )
        is False
    )


def test_error_parse_cache_is_not_treated_as_fresh(tmp_path: Path):
    """error cache 只保留审计记录，不能阻止同版本再次重解析。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    parser_profile = '{"parser_profile_version":"v1"}'

    store.upsert_parse_cache(
        file_identity="bootstrap:/work/report.txt",
        parser_profile=parser_profile,
        content_excerpt="",
        parse_status="error",
        parse_error="boom",
        source_version="mtime=1:size=10",
    )

    assert (
        store.has_fresh_cache(
            "bootstrap:/work/report.txt",
            parser_profile,
            source_version="mtime=1:size=10",
        )
        is False
    )


def test_load_parse_cache_requires_matching_source_version(tmp_path: Path):
    """按版本加载缓存时，版本不匹配应按缺失处理。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    parser_profile = '{"parser_profile_version":"v1"}'

    store.upsert_parse_cache(
        file_identity="bootstrap:/work/report.txt",
        parser_profile=parser_profile,
        content_excerpt="old content",
        parse_status="success",
        parse_error="",
        source_version="mtime=1:size=10",
    )

    with pytest.raises(KeyError):
        store.load_parse_cache(
            "bootstrap:/work/report.txt",
            parser_profile,
            source_version="mtime=2:size=10",
        )


def test_file_inventory_schema_uses_modified_date_column(tmp_path: Path):
    """库存表列名应与 Task 3 计划契约保持一致。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    with store._connect() as conn:
        rows = conn.execute("PRAGMA table_info(file_inventory)").fetchall()

    column_names = {row["name"] for row in rows}
    assert "modified_date" in column_names
    assert "modified_at" not in column_names


def test_file_inventory_schema_includes_source_version(tmp_path: Path):
    """库存表应保存发现阶段计算的文件版本。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    with store._connect() as conn:
        rows = conn.execute("PRAGMA table_info(file_inventory)").fetchall()

    column_names = {row["name"] for row in rows}
    assert "source_version" in column_names


def test_replace_inventory_and_query_inventory(tmp_path: Path):
    """库存快照应能整体替换并按日期范围读回 typed item。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    store.replace_inventory(
        [
            {
                "file_identity": "bootstrap:/work/report.txt",
                "path": "/work/report.txt",
                "extension": ".txt",
                "modified_date": "2026-05-10",
                "size_bytes": 10,
                "source_version": "mtime=1:size=10",
            }
        ]
    )

    items = store.query_inventory(date(2026, 5, 9), date(2026, 5, 11))

    assert items == [
        InventoryItem(
            file_identity="bootstrap:/work/report.txt",
            path=Path("/work/report.txt"),
            extension=".txt",
            modified_date=date(2026, 5, 10),
            size_bytes=10,
            source_version="mtime=1:size=10",
        )
    ]


def test_query_inventory_filters_date_range(tmp_path: Path):
    """库存查询应按 modified_date 闭区间过滤。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.replace_inventory(
        [
            {
                "file_identity": "old",
                "path": "/work/old.txt",
                "extension": ".txt",
                "modified_date": "2026-05-08",
                "size_bytes": 1,
                "source_version": "mtime=1:size=1",
            },
            {
                "file_identity": "inside",
                "path": "/work/inside.txt",
                "extension": ".txt",
                "modified_date": "2026-05-10",
                "size_bytes": 2,
                "source_version": "mtime=2:size=2",
            },
            {
                "file_identity": "new",
                "path": "/work/new.txt",
                "extension": ".txt",
                "modified_date": "2026-05-12",
                "size_bytes": 3,
                "source_version": "mtime=3:size=3",
            },
        ]
    )

    items = store.query_inventory(date(2026, 5, 9), date(2026, 5, 11))

    assert [item.file_identity for item in items] == ["inside"]


def test_existing_task2_database_is_migrated_to_source_version_schema(tmp_path: Path):
    """旧 Task 2 索引库应迁移到版本化缓存和 modified_date 列。"""
    db_path = tmp_path / "scan_index.sqlite3"
    with sqlite3.connect(db_path) as conn:
        conn.executescript(
            """
            CREATE TABLE file_inventory (
                file_identity TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                extension TEXT NOT NULL,
                modified_at TEXT NOT NULL,
                size_bytes INTEGER NOT NULL
            );

            CREATE TABLE parse_cache (
                file_identity TEXT NOT NULL,
                parser_profile TEXT NOT NULL,
                content_excerpt TEXT NOT NULL,
                parse_status TEXT NOT NULL,
                parse_error TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (file_identity, parser_profile)
            );
            """
        )
        conn.execute(
            """
            INSERT INTO file_inventory (
                file_identity, path, extension, modified_at, size_bytes
            )
            VALUES ('file-1', 'a.txt', '.txt', '2026-05-11', 10)
            """
        )
        conn.execute(
            """
            INSERT INTO parse_cache (
                file_identity, parser_profile, content_excerpt, parse_status, parse_error
            )
            VALUES ('file-1', '{}', 'cached', 'success', '')
            """
        )

    store = ScanIndexStore(db_path=db_path)

    with store._connect() as conn:
        inventory_columns = {
            row["name"] for row in conn.execute("PRAGMA table_info(file_inventory)")
        }
        cache_columns = {
            row["name"] for row in conn.execute("PRAGMA table_info(parse_cache)")
        }
        inventory_row = conn.execute(
            "SELECT modified_date FROM file_inventory WHERE file_identity = 'file-1'"
        ).fetchone()

    assert "modified_date" in inventory_columns
    assert "modified_at" not in inventory_columns
    assert "source_version" in inventory_columns
    assert "source_version" in cache_columns
    assert inventory_row["modified_date"] == "2026-05-11"
    assert store.has_fresh_cache("file-1", "{}", source_version="") is True
    assert store.has_fresh_cache("file-1", "{}", source_version="changed") is False


def test_load_parse_cache_missing_raises_key_error(tmp_path: Path):
    """缺失缓存必须显式抛 KeyError，避免调用方误用空结果。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    with pytest.raises(KeyError):
        store.load_parse_cache("missing-file", '{"parser_profile_version":"v1"}')


def test_save_and_load_discovery_checkpoint(tmp_path: Path):
    """checkpoint placeholder 应支持覆盖写入与读回。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    store.save_checkpoint("bootstrap", "2026-05-11T10:00:00")
    store.save_checkpoint("bootstrap", "2026-05-11T10:05:00")

    assert store.load_checkpoint("bootstrap") == "2026-05-11T10:05:00"


def test_load_checkpoint_returns_none_when_missing(tmp_path: Path):
    """缺失 checkpoint 时应返回 None，便于上层保留 placeholder 流程。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    assert store.load_checkpoint("missing") is None


def test_save_scan_run_metrics(tmp_path: Path):
    """扫描运行指标应写入 scan_runs，并可按最新一条读回。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    store.save_scan_run_metrics(
        discovered_count=5,
        reused_count=3,
        reparsed_count=2,
    )
    store.save_scan_run_metrics(
        discovered_count=8,
        reused_count=6,
        reparsed_count=2,
    )

    assert store.latest_scan_run() == {
        "discovered_count": 8,
        "reused_count": 6,
        "reparsed_count": 2,
    }


def test_scan_runs_schema_includes_performance_columns(tmp_path: Path):
    """scan_runs 应保存阶段耗时和结果计数，同时保留旧三字段。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    with store._connect() as conn:
        rows = conn.execute("PRAGMA table_info(scan_runs)").fetchall()

    column_names = {row["name"] for row in rows}
    assert {
        "total_duration_ms",
        "discovery_duration_ms",
        "inventory_cache_duration_ms",
        "parse_duration_ms",
        "aggregation_duration_ms",
        "success_count",
        "error_count",
        "timeout_count",
    } <= column_names


def test_save_scan_run_metrics_persists_detail_and_extension_metrics(tmp_path: Path):
    """完整指标应写入 scan_runs detail 和 scan_extension_metrics 明细表。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    run_metrics = ScanRunMetrics(
        total_duration_ms=120,
        discovery_duration_ms=10,
        inventory_cache_duration_ms=20,
        parse_duration_ms=70,
        aggregation_duration_ms=5,
        discovered_count=4,
        reused_count=1,
        reparsed_count=3,
        success_count=2,
        error_count=1,
        timeout_count=1,
        extension_metrics=[
            ExtensionMetrics(
                extension=".pdf",
                file_count=2,
                parse_duration_ms=60,
                success_count=1,
                error_count=1,
                timeout_count=1,
            ),
            ExtensionMetrics(
                extension=".txt",
                file_count=1,
                parse_duration_ms=10,
                success_count=1,
                error_count=0,
                timeout_count=0,
            ),
        ],
    )

    run_id = store.save_scan_run_metrics(run_metrics=run_metrics)

    assert store.latest_scan_run() == {
        "discovered_count": 4,
        "reused_count": 1,
        "reparsed_count": 3,
    }
    assert store.latest_scan_run_detail() == {
        "run_id": run_id,
        "discovered_count": 4,
        "reused_count": 1,
        "reparsed_count": 3,
        "total_duration_ms": 120,
        "discovery_duration_ms": 10,
        "inventory_cache_duration_ms": 20,
        "parse_duration_ms": 70,
        "aggregation_duration_ms": 5,
        "success_count": 2,
        "error_count": 1,
        "timeout_count": 1,
    }
    assert store.list_extension_metrics(run_id) == run_metrics.extension_metrics


def test_existing_scan_runs_table_is_migrated_to_performance_schema(tmp_path: Path):
    """旧 scan_runs 表只有三项计数时，应无损补齐性能列。"""
    db_path = tmp_path / "scan_index.sqlite3"
    with sqlite3.connect(db_path) as conn:
        conn.executescript(
            """
            CREATE TABLE scan_runs (
                run_id INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                discovered_count INTEGER NOT NULL DEFAULT 0,
                reused_count INTEGER NOT NULL DEFAULT 0,
                reparsed_count INTEGER NOT NULL DEFAULT 0
            );
            """
        )
        conn.execute(
            """
            INSERT INTO scan_runs (discovered_count, reused_count, reparsed_count)
            VALUES (3, 1, 2)
            """
        )

    store = ScanIndexStore(db_path=db_path)

    assert store.latest_scan_run_detail()["total_duration_ms"] == 0
    assert store.latest_scan_run_detail()["discovered_count"] == 3


def test_latest_scan_run_raises_when_missing(tmp_path: Path):
    """缺失 scan_runs 数据时应抛稳定 KeyError，避免上层误读默认值。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    with pytest.raises(KeyError, match="scan_runs"):
        store.latest_scan_run()


def test_probe_parse_cache_returns_fresh_for_exact_success(tmp_path: Path):
    """完全匹配的 success cache 应解释为 fresh。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="cached",
        parse_status="success",
        parse_error="",
    )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-a",
        source_version="mtime=1:size=10",
    )

    assert probe.cache_status == "fresh"
    assert probe.cache_miss_reason == ""
    assert probe.previous_source_version is None


def test_probe_parse_cache_returns_new_file_when_no_history(tmp_path: Path):
    """完全无历史 cache 时应解释为 new_file。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    probe = store.probe_parse_cache(
        "missing",
        "profile-a",
        source_version="mtime=1:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "new_file"
    assert probe.previous_source_version is None


def test_probe_parse_cache_returns_source_version_changed(tmp_path: Path):
    """同身份同 profile 但 source_version 不同时，应解释为 source_version_changed。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="old",
        parse_status="success",
        parse_error="",
    )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-a",
        source_version="mtime=2:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "source_version_changed"
    assert probe.previous_source_version == "mtime=1:size=10"


def test_probe_parse_cache_uses_latest_inserted_history_when_updated_at_ties(
    tmp_path: Path,
):
    """同秒写入多条历史时，应按插入顺序稳定选择最近历史。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="old",
        parse_status="success",
        parse_error="",
    )
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=2:size=10",
        content_excerpt="newer",
        parse_status="success",
        parse_error="",
    )
    with store._connect() as conn:
        conn.execute(
            """
            UPDATE parse_cache
            SET updated_at = '2026-01-01 00:00:00'
            WHERE file_identity = ?
            """,
            ("file-1",),
        )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-a",
        source_version="mtime=3:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "source_version_changed"
    assert probe.previous_source_version == "mtime=2:size=10"


def test_probe_parse_cache_returns_parser_profile_changed(tmp_path: Path):
    """同身份存在 cache 但 profile 不同时，应解释为 parser_profile_changed。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="old",
        parse_status="success",
        parse_error="",
    )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-b",
        source_version="mtime=1:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "parser_profile_changed"
    assert probe.previous_source_version == "mtime=1:size=10"


def test_probe_parse_cache_returns_error_cache_for_exact_error(tmp_path: Path):
    """同版本只有 error cache 时，应解释为 error_cache 而不是 fresh。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="",
        parse_status="error",
        parse_error="boom",
    )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-a",
        source_version="mtime=1:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "error_cache"
    assert probe.previous_source_version == "mtime=1:size=10"
