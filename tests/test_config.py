"""测试配置对象的兼容默认值。"""

import pickle
from pathlib import Path
from types import SimpleNamespace

import pytest

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
    """通用本机配置先加载，Windows 与 secrets 可依次覆盖。"""
    config_dir = tmp_path / "config"

    settings_files = Config._settings_files(config_dir, system_name="Windows")

    assert settings_files == [
        str(config_dir / "settings.yaml"),
        str(config_dir / "settings.windows.yaml"),
        str(config_dir / ".secrets.yaml"),
    ]


def test_build_settings_reads_generic_local_yaml(tmp_path):
    """通用配置应与系统配置合并，并由系统配置覆盖同名值。"""
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / "settings.yaml").write_text(
        "\n".join(
            [
                "paths:",
                "  work_dir: D:/audit/work",
                "llm:",
                "  provider: deepseek",
                "  model_id: deepseek-chat",
                "  DEEPSEEK_API_KEY: local-test-key",
            ]
        ),
        encoding="utf-8",
    )
    (config_dir / "settings.windows.yaml").write_text(
        "\n".join(
            [
                "paths:",
                "  work_dir: D:/audit/windows-work",
                "llm:",
                "  provider: deepseek",
                "  model_id: deepseek-reasoner",
            ]
        ),
        encoding="utf-8",
    )

    settings = Config._build_settings(config_dir, system_name="Windows")

    assert settings.paths.work_dir == "D:/audit/windows-work"
    assert settings.llm.model_id == "deepseek-reasoner"
    assert settings.llm.DEEPSEEK_API_KEY == "local-test-key"


def test_deepseek_api_key_accepts_local_llm_key(monkeypatch):
    """兼容现有 settings.yaml 把密钥放在 llm 节点的本机配置。"""
    monkeypatch.delenv("DEEPSEEK_API_KEY", raising=False)
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        llm=SimpleNamespace(DEEPSEEK_API_KEY="local-test-key"),
    )

    assert cfg.deepseek_api_key == "local-test-key"


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
        "rust/target/release/ai-daily-discovery"
    )
    assert scanner_config["discovery_timeout_seconds"] == 30


def test_scanner_config_exposes_office_parser_defaults_when_keys_absent():
    """Office parser 缺省时应优先 Rust，并保留 Python fallback。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".docx"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["office_parser_backend"] == "rust_office_oxide_v1"
    assert scanner_config["pdf_parser_backend"] == "pdf_text_v1"
    assert scanner_config["rust_office_parser_bin"] == (
        "rust/target/release/ai-daily-office-parser"
    )
    assert scanner_config["office_parser_fallback_enabled"] is True
    assert scanner_config["office_parser_fallback_order"] == [
        "python_office_v1",
        "python_sharepoint_text_v1",
    ]
    assert scanner_config["office_fallback_after_timeout"] is False
    assert scanner_config["office_external_fallback"] == "disabled"
    assert scanner_config["office_legacy_extensions_enabled"] is False


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


def test_scanner_config_normalizes_empty_excluded_dirs_to_list(tmp_path):
    """YAML 空 excluded_dirs 应按空列表处理，避免 Rust discovery 收到 null。"""
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / "settings.linux.yaml").write_text(
        "\n".join(
            [
                "scanner:",
                "  allowed_extensions:",
                "    - .md",
                "  ignored_patterns: []",
                "  excluded_dirs:",
                "  max_workers: 1",
                "  excel_max_rows: 50",
                "  pdf_max_pages: 5",
                "  text_max_chars: 6000",
            ]
        ),
        encoding="utf-8",
    )
    cfg = object.__new__(Config)
    cfg._settings = Config._build_settings(config_dir, system_name="Linux")

    scanner_config = cfg.scanner_config

    assert scanner_config["excluded_dirs"] == []
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
                "  rust_discovery_bin: rust/target/release/ai-daily-discovery",
                "  discovery_timeout_seconds: 12",
                "  office_parser_backend: rust_office_oxide_v1",
                "  pdf_parser_backend: custom_pdf_v2",
                "  rust_office_parser_bin: rust/target/release/ai-daily-office-parser",
                "  office_parser_fallback_enabled: true",
                "  office_parser_fallback_order:",
                "    - python_office_v1",
                "    - python_sharepoint_text_v1",
                "  office_fallback_after_timeout: false",
                "  office_external_fallback: disabled",
                "  office_legacy_extensions_enabled: false",
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
        "rust/target/release/ai-daily-discovery"
    )
    assert scanner_config["discovery_timeout_seconds"] == 12
    assert scanner_config["office_parser_backend"] == "rust_office_oxide_v1"
    assert scanner_config["pdf_parser_backend"] == "custom_pdf_v2"
    assert isinstance(scanner_config["office_parser_fallback_order"], list)
    assert scanner_config["office_parser_fallback_enabled"] is True
    assert scanner_config["office_fallback_after_timeout"] is False

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


def test_scanner_config_exposes_office_fallback_policy_version(tmp_path: Path):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / "settings.linux.yaml").write_text(
        "\n".join(
            [
                "scanner:",
                "  allowed_extensions:",
                "    - .docx",
                "  ignored_patterns: []",
                "  max_workers: 1",
                "  excel_max_rows: 50",
                "  pdf_max_pages: 5",
                "  text_max_chars: 6000",
                "  office_fallback_policy_version: hybrid_v2",
            ]
        ),
        encoding="utf-8",
    )
    cfg = object.__new__(Config)
    cfg._settings = Config._build_settings(config_dir, system_name="Linux")

    assert cfg.scanner_config["office_fallback_policy_version"] == "hybrid_v2"


def test_scanner_config_defaults_empty_office_fallback_policy_version(
    tmp_path: Path,
):
    """YAML 空值不能进入 parser profile，否则 cache key 会意外分裂。"""
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / "settings.linux.yaml").write_text(
        "\n".join(
            [
                "scanner:",
                "  allowed_extensions:",
                "    - .docx",
                "  ignored_patterns: []",
                "  max_workers: 1",
                "  excel_max_rows: 50",
                "  pdf_max_pages: 5",
                "  text_max_chars: 6000",
                "  office_fallback_policy_version:",
            ]
        ),
        encoding="utf-8",
    )
    cfg = object.__new__(Config)
    cfg._settings = Config._build_settings(config_dir, system_name="Linux")

    assert cfg.scanner_config["office_fallback_policy_version"] == "hybrid_v1"


def test_scanner_config_defaults_blank_office_fallback_policy_version():
    """只含空白的显式配置也应等价于未配置。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".docx"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
            office_fallback_policy_version="   ",
        )
    )

    assert cfg.scanner_config["office_fallback_policy_version"] == "hybrid_v1"


def test_scanner_engine_defaults_keep_python_legacy_until_cutover():
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(scanner=SimpleNamespace())

    assert cfg.scanner_engine == "python_legacy"
    assert cfg.rust_scanner_bin == "rust/target/release/ai-daily-scanner"
    assert cfg.rust_index_db_path == "data/db/scan_index_v2.sqlite3"
    assert cfg.rust_process_timeout_seconds == 900.0


def test_scanner_engine_reads_explicit_rust_v2_infrastructure_only():
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            engine="RUST_V2",
            rust_scanner_bin="bin/scanner",
            rust_index_db_path="state/scan_index_v2.sqlite3",
            rust_process_timeout_seconds="45.5",
        )
    )

    assert cfg.scanner_engine == "rust_v2"
    assert cfg.rust_scanner_bin == "bin/scanner"
    assert cfg.rust_index_db_path == "state/scan_index_v2.sqlite3"
    assert cfg.rust_process_timeout_seconds == 45.5


@pytest.mark.parametrize("value", ["automatic", "", "python"])
def test_scanner_engine_rejects_implicit_or_unknown_selection(value: str):
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(scanner=SimpleNamespace(engine=value))

    with pytest.raises(ValueError, match="unsupported scanner engine"):
        _ = cfg.scanner_engine
