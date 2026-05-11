"""测试 SQLite 扫描索引存储。"""

import sqlite3
from datetime import date
from pathlib import Path

import pytest

from src.services.scan_index_store import InventoryItem, ScanIndexStore


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


def test_latest_scan_run_raises_when_missing(tmp_path: Path):
    """缺失 scan_runs 数据时应抛稳定 KeyError，避免上层误读默认值。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    with pytest.raises(KeyError, match="scan_runs"):
        store.latest_scan_run()
