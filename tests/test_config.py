"""测试配置对象的兼容默认值。"""

import pickle
from types import SimpleNamespace

from src.core.config import Config, config


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


def test_scanner_config_uses_builtin_containers_and_is_picklable():
    """扫描配置必须可 pickle，确保 Windows spawn 能把参数安全传给子进程。"""
    scanner_config = config.scanner_config

    assert isinstance(scanner_config["allowed_extensions"], list)
    assert isinstance(scanner_config["ignored_patterns"], list)
    assert isinstance(scanner_config["file_timeout_by_extension"], dict)

    pickle.dumps(scanner_config)
