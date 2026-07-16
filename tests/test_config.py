"""测试配置对象的兼容默认值。"""

from pathlib import Path
from types import SimpleNamespace

import pytest
import yaml

from src.core.config import Config


def _installed_env(root: Path) -> dict[str, str]:
    shared = root / "shared"
    paths = {
        "DAILY_REPORT_INSTALL_ROOT": root,
        "DAILY_REPORT_CONFIG_DIR": shared / "config",
        "DAILY_REPORT_DATA_DIR": shared / "data",
        "DAILY_REPORT_REPORTS_DIR": shared / "data" / "reports",
        "DAILY_REPORT_DB_DIR": shared / "data" / "db",
        "DAILY_REPORT_LOG_DIR": shared / "logs",
    }
    for path in paths.values():
        Path(path).mkdir(parents=True, exist_ok=True)
    return {name: str(path) for name, path in paths.items()}


def test_example_settings_selects_the_windows_rust_core_defaults():
    project_root = Path(__file__).resolve().parents[1]
    settings = yaml.safe_load(
        (project_root / "config" / "settings.example.yaml").read_text(
            encoding="utf-8"
        )
    )

    assert settings["scanner"]["engine"] == "rust_v2"
    assert settings["scanner"]["rust_scanner_bin"] == (
        "rust/target/release/ai-daily-scanner"
    )
    assert settings["scanner"]["rust_index_db_path"] == (
        "data/db/scan_index_v2.sqlite3"
    )


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


def test_scanner_engine_defaults_to_rust_v2_after_cutover():
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(scanner=SimpleNamespace())

    assert cfg.scanner_engine == "rust_v2"
    assert cfg.rust_scanner_bin == "rust/target/release/ai-daily-scanner"
    assert cfg.rust_office_parser_bin == (
        "rust/target/release/ai-daily-office-parser"
    )
    assert cfg.rust_index_db_path == "data/db/scan_index_v2.sqlite3"
    assert cfg.rust_process_timeout_seconds == 900.0


def test_scanner_engine_reads_explicit_rust_v2_infrastructure_only():
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            engine="RUST_V2",
            rust_scanner_bin="bin/scanner",
            rust_office_parser_bin="bin/office-worker",
            rust_index_db_path="state/scan_index_v2.sqlite3",
            rust_process_timeout_seconds="45.5",
        )
    )

    assert cfg.scanner_engine == "rust_v2"
    assert cfg.rust_scanner_bin == "bin/scanner"
    assert cfg.rust_office_parser_bin == "bin/office-worker"
    assert cfg.rust_index_db_path == "state/scan_index_v2.sqlite3"
    assert cfg.rust_process_timeout_seconds == 45.5


@pytest.mark.parametrize("value", ["automatic", "", "python"])
def test_scanner_engine_rejects_implicit_or_unknown_selection(value: str):
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(scanner=SimpleNamespace(engine=value))

    with pytest.raises(ValueError, match="unsupported scanner engine"):
        _ = cfg.scanner_engine


def test_installed_paths_accept_absolute_drive_space_and_non_ascii(tmp_path):
    install_root = tmp_path / "安装 根"
    environ = _installed_env(install_root)

    resolved = Config._resolve_installed_paths(environ)

    assert resolved is not None
    assert resolved["install_root"] == install_root.resolve()
    assert resolved["reports_dir"] == (
        install_root / "shared" / "data" / "reports"
    ).resolve()


@pytest.mark.parametrize(
    ("mutate", "message"),
    [
        (
            lambda env, root: env.pop("DAILY_REPORT_LOG_DIR"),
            "installed mode requires DAILY_REPORT_LOG_DIR",
        ),
        (
            lambda env, root: env.__setitem__("DAILY_REPORT_DB_DIR", "relative/db"),
            "DAILY_REPORT_DB_DIR must be absolute",
        ),
        (
            lambda env, root: env.__setitem__(
                "DAILY_REPORT_REPORTS_DIR",
                str((root / "shared" / "missing-reports").resolve()),
            ),
            "DAILY_REPORT_REPORTS_DIR must be an existing directory",
        ),
        (
            lambda env, root: env.__setitem__(
                "DAILY_REPORT_LOG_DIR",
                str((root / "releases" / "v1" / "logs").resolve()),
            ),
            "DAILY_REPORT_LOG_DIR must stay under install-root/shared",
        ),
    ],
)
def test_installed_paths_reject_missing_relative_or_version_local_values(
    tmp_path,
    mutate,
    message,
):
    install_root = tmp_path / "install"
    environ = _installed_env(install_root)
    (install_root / "releases" / "v1" / "logs").mkdir(parents=True)
    mutate(environ, install_root)

    with pytest.raises(ValueError, match=message):
        Config._resolve_installed_paths(environ)


def test_installed_mode_derives_release_binaries_and_shared_scan_db(tmp_path):
    install_root = tmp_path / "install"
    paths = Config._resolve_installed_paths(_installed_env(install_root))
    assert paths is not None
    release_root = install_root / "releases" / "版本 a"
    release_root.mkdir(parents=True)
    work_dir = install_root / "synthetic work"
    work_dir.mkdir()
    cfg = object.__new__(Config)
    cfg._project_root = release_root.resolve()
    cfg._install_root = paths["install_root"]
    cfg._config_dir = paths["config_dir"]
    cfg._data_dir = paths["data_dir"]
    cfg._reports_dir = paths["reports_dir"]
    cfg._db_dir = paths["db_dir"]
    cfg._log_dir = paths["log_dir"]
    cfg._settings = SimpleNamespace(
        paths=SimpleNamespace(work_dir=str(work_dir)),
        scanner=SimpleNamespace(),
    )

    assert Path(cfg.rust_scanner_bin).is_absolute()
    assert Path(cfg.rust_scanner_bin).is_relative_to(release_root)
    assert Path(cfg.rust_office_parser_bin).is_relative_to(release_root)
    assert Path(cfg.rust_index_db_path) == paths["db_dir"] / "scan_index_v2.sqlite3"
    assert cfg.log_dir == paths["log_dir"]


def test_installed_mode_rejects_relative_business_work_dir(tmp_path):
    cfg = object.__new__(Config)
    cfg._install_root = tmp_path.resolve()
    cfg._settings = SimpleNamespace(
        paths=SimpleNamespace(work_dir="relative/business"),
    )

    with pytest.raises(ValueError, match="paths.work_dir must be absolute"):
        _ = cfg.work_dir
