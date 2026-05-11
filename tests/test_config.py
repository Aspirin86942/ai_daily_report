"""测试配置对象的兼容默认值。"""

from types import SimpleNamespace

from src.core.config import Config


def test_scanner_config_exposes_scan_index_defaults_when_keys_absent():
    """旧配置缺少新增键时，应使用扫描索引和 parser profile 默认值。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".txt"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["index_db_path"] == "data/db/scan_index.sqlite3"
    assert scanner_config["parser_profile_version"] == "v1"
    assert scanner_config["worker_lane_mode"] == "direct"
