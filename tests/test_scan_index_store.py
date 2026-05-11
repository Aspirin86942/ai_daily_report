"""测试 SQLite 扫描索引存储。"""

from pathlib import Path

import pytest

from src.services.scan_index_store import ScanIndexStore


def test_index_store_creates_inventory_and_cache_tables(tmp_path: Path):
    """初始化索引库时应创建库存、解析缓存和扫描运行表。"""
    store = ScanIndexStore(db_path=tmp_path / "nested" / "scan_index.sqlite3")

    table_names = store.list_tables()

    assert store.db_path.exists()
    assert {"file_inventory", "parse_cache", "scan_runs"} <= table_names


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


def test_load_parse_cache_missing_raises_key_error(tmp_path: Path):
    """缺失缓存必须显式抛 KeyError，避免调用方误用空结果。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    with pytest.raises(KeyError):
        store.load_parse_cache("missing-file", '{"parser_profile_version":"v1"}')
