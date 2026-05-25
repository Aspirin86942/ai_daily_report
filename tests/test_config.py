"""测试配置对象的兼容默认值。"""

import pickle
from types import SimpleNamespace

from src.core.config import Config


def test_build_settings_reads_platform_yaml_and_yaml_secrets(tmp_path):
    """配置加载应按平台读取 YAML，并继续让敏感配置覆盖非敏感配置。"""
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / "settings.linux.yaml").write_text(
        "\n".join(
            [
                "paths:",
                "  work_dir: /home/george/work",
                "llm:",
                "  provider: deepseek",
                "api:",
                "  deepseek_api_key: from-settings",
            ]
        ),
        encoding="utf-8",
    )
    (config_dir / ".secrets.yaml").write_text(
        "\n".join(
            [
                "api:",
                "  deepseek_api_key: from-secrets",
                "proxy:",
                "  http_proxy: http://127.0.0.1:10808",
            ]
        ),
        encoding="utf-8",
    )

    settings = Config._build_settings(config_dir, system_name="Linux")

    assert settings.paths.work_dir == "/home/george/work"
    assert settings.llm.provider == "deepseek"
    assert settings.api.deepseek_api_key == "from-secrets"
    assert settings.proxy.http_proxy == "http://127.0.0.1:10808"


def test_config_selects_windows_yaml_on_windows(tmp_path):
    """Windows 运行时必须读取 Windows 专用配置，避免误用 Linux 路径。"""
    config_dir = tmp_path / "config"

    settings_files = Config._settings_files(config_dir, system_name="Windows")

    assert settings_files == [
        str(config_dir / "settings.windows.yaml"),
        str(config_dir / ".secrets.yaml"),
    ]


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
    assert scanner_config["excluded_dirs"] == []


def test_scanner_config_exposes_discovery_backend_defaults_when_keys_absent():
    """配置缺省时应优先走 Rust；Rust 失败时由 discovery 层 fallback。"""
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

    assert scanner_config["discovery_backend"] == "rust"
    assert scanner_config["rust_discovery_bin"] == (
        "rust/discovery/target/release/ai-daily-discovery"
    )
    assert scanner_config["discovery_timeout_seconds"] == 30


def test_scanner_config_passes_excluded_dirs_as_builtin_list():
    """excluded_dirs 必须从 settings 透传到 scanner，并转成普通 list。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".txt"],
            ignored_patterns=[],
            excluded_dirs=["D:\\work\\skip", "D:\\work\\logs"],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["excluded_dirs"] == ["D:\\work\\skip", "D:\\work\\logs"]
    assert isinstance(scanner_config["excluded_dirs"], list)


def test_scanner_config_passes_direct_text_max_bytes_when_present():
    """direct_text_max_bytes 是扫描 lane 安全阈值，应从 settings 透传给 scanner。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".txt"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
            direct_text_max_bytes=8192,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["direct_text_max_bytes"] == 8192


def test_scanner_config_uses_builtin_containers_and_is_picklable(tmp_path):
    """扫描配置必须可 pickle，确保 Windows spawn 能把参数安全传给子进程。"""
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / "settings.linux.yaml").write_text(
        "\n".join(
            [
                "scanner:",
                "  allowed_extensions:",
                "    - .txt",
                "  ignored_patterns:",
                "    - '~$*'",
                "  excluded_dirs:",
                "    - /tmp/skip",
                "  max_workers: 1",
                "  excel_max_rows: 50",
                "  pdf_max_pages: 5",
                "  text_max_chars: 6000",
                "  discovery_backend: rust",
                "  rust_discovery_bin: rust/discovery/target/release/ai-daily-discovery",
                "  discovery_timeout_seconds: 12",
                "  file_timeout_by_extension:",
                "    .pdf: 45",
            ]
        ),
        encoding="utf-8",
    )
    cfg = object.__new__(Config)
    cfg._settings = Config._build_settings(config_dir, system_name="Linux")

    scanner_config = cfg.scanner_config

    assert isinstance(scanner_config["allowed_extensions"], list)
    assert isinstance(scanner_config["ignored_patterns"], list)
    assert isinstance(scanner_config["excluded_dirs"], list)
    assert isinstance(scanner_config["file_timeout_by_extension"], dict)
    assert scanner_config["discovery_backend"] == "rust"
    assert scanner_config["rust_discovery_bin"] == (
        "rust/discovery/target/release/ai-daily-discovery"
    )
    assert scanner_config["discovery_timeout_seconds"] == 12

    pickle.dumps(scanner_config)


def test_scanner_config_passes_light_text_parser_options_when_present():
    """轻量文本解析预算应从 settings 透传到 scanner 配置。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".md"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
            direct_text_read_bytes=131072,
            log_tail_read_bytes=65536,
            text_excerpt_max_chars=3000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["direct_text_read_bytes"] == 131072
    assert scanner_config["log_tail_read_bytes"] == 65536
    assert scanner_config["text_excerpt_max_chars"] == 3000
