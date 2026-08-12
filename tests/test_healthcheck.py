"""Tests for CLI health checks."""

from pathlib import Path
from types import SimpleNamespace

import pytest

from src.core import healthcheck
from src.core.config import UnknownScannerSettingsError
from src.services.native_scanner import NativeScannerError


def _write_templates(root: Path) -> None:
    templates_dir = root / "templates"
    templates_dir.mkdir(parents=True)
    for name in (
        "system_prompt.md",
        "report_template.md",
        "weekly_prompt.md",
        "monthly_prompt.md",
        "weekly_template.md",
        "monthly_template.md",
    ):
        (templates_dir / name).write_text(f"# {name}\n", encoding="utf-8")


def _make_config(provider: str, api_key: str | None, root: Path) -> SimpleNamespace:
    return SimpleNamespace(
        llm_provider=provider,
        work_dir=root / "workspace",
        llm_config={"model_id": "deepseek-chat", "max_tokens": 8192},
        scanner_settings=lambda: {"max_workers": 4},
        reports_dir=root / "data" / "reports",
        db_dir=root / "data" / "db",
        deepseek_api_key=api_key if provider == "deepseek" else "",
        openai_api_key=api_key if provider == "openai" else "",
    )


def _make_strict_rust_config(
    root: Path,
    *,
    allowed_extensions: list[str] | None = None,
) -> SimpleNamespace:
    cfg = _make_config("deepseek", "synthetic-key", root)
    cfg.office_worker_path = "bin/ai-daily-office-parser"
    cfg.index_db_path = "data/db/scan_index_v3.sqlite3"
    cfg.scanner_settings = lambda: {
        "max_workers": 4,
        "allowed_extensions": allowed_extensions or [".txt"],
    }
    return cfg


def _strict_doctor(*checks: tuple[str, str]) -> SimpleNamespace:
    return SimpleNamespace(
        contract="ai_daily_context",
        protocol_version=1,
        engine_version="0.1.0",
        engine_build="sha256-source-v1:synthetic",
        checks=[
            SimpleNamespace(name=name, status=status)
            for name, status in checks
        ],
    )


def _prepare_strict_root(root: Path) -> None:
    (root / "config").mkdir()
    _write_templates(root)
    (root / "data" / "db").mkdir(parents=True)
    (root / "workspace").mkdir()


def _prepare_platform_config_root(root: Path) -> None:
    config_dir = root / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(root)


def test_collect_healthcheck_strict_uses_effective_config_without_local_yaml(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)
    captured: dict[str, object] = {}

    class StubNativeScanner:
        def __init__(self, runtime_config, **kwargs):
            captured["runtime_config"] = runtime_config
            captured.update(kwargs)

        def doctor(self):
            return _strict_doctor(
                ("scan_db_parent", "ok"),
                ("office_worker_handshake", "ok"),
                ("python_worker_handshake", "ok"),
            )

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(
        healthcheck,
        "NativeScanner",
        StubNativeScanner,
        raising=False,
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_strict_rust_config(tmp_path),
        strict=True,
    )

    assert result.errors == []
    assert not any("缺少配置文件" in message for message in result.errors)
    assert result.info["Scanner Interface"] == "native"
    assert result.info["Native Scanner Contract"] == "ai_daily_context/v1"
    assert result.info["Native Scanner Engine"] == "0.1.0"
    assert captured["project_root"] == tmp_path


def test_collect_healthcheck_strict_always_requires_office_worker_package(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)

    class StubNativeScanner:
        def __init__(self, runtime_config, **kwargs):
            pass

        def doctor(self):
            return _strict_doctor(
                ("scan_db_parent", "ok"),
                ("office_worker_handshake", "error"),
                ("python_worker_handshake", "ok"),
            )

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(
        healthcheck,
        "NativeScanner",
        StubNativeScanner,
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_strict_rust_config(
            tmp_path,
            allowed_extensions=[".txt"],
        ),
        strict=True,
    )

    assert "Native strict check failed: office_worker_handshake" in result.errors


def test_collect_healthcheck_strict_reports_safe_scanner_contract_failure(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)

    class FailingNativeScanner:
        def __init__(self, runtime_config, **kwargs):
            pass

        def doctor(self):
            raise NativeScannerError("NATIVE_RESULT_INVALID", "private", False)

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(
        healthcheck,
        "NativeScanner",
        FailingNativeScanner,
        raising=False,
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_strict_rust_config(tmp_path),
        strict=True,
    )

    assert result.errors == [
        "Native scanner doctor failed (NATIVE_RESULT_INVALID)"
    ]


def test_collect_healthcheck_strict_requires_configured_worker_routes(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)

    class StubNativeScanner:
        def __init__(self, runtime_config, **kwargs):
            pass

        def doctor(self):
            return _strict_doctor(
                ("scan_db_parent", "ok"),
                ("office_worker_handshake", "error"),
                ("python_worker_handshake", "error"),
            )

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(
        healthcheck,
        "NativeScanner",
        StubNativeScanner,
        raising=False,
    )
    cfg = _make_strict_rust_config(
        tmp_path,
        allowed_extensions=[".xlsx", ".pdf"],
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=cfg,
        strict=True,
    )

    assert "Native strict check failed: office_worker_handshake" in result.errors
    assert "Native strict check failed: python_worker_handshake" in result.errors


def test_collect_healthcheck_accepts_env_only_provider_key(tmp_path, monkeypatch):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    (tmp_path / "data").mkdir()
    (tmp_path / "workspace").mkdir()

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [("rich", "rich")])
    monkeypatch.setattr(
        healthcheck.importlib,
        "import_module",
        lambda module_name: object(),
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_config("deepseek", "sk-test-123456", tmp_path),
    )

    assert result.errors == []
    assert any("缺少敏感配置文件" in message for message in result.warnings)
    assert any("config/.secrets.yaml" in message for message in result.warnings)
    assert result.info["LLM Provider"] == "deepseek"
    assert result.info["API Key"] == "已配置"
    assert "sk-test-123456" not in repr(result)


def test_collect_healthcheck_accepts_legacy_local_settings(tmp_path, monkeypatch):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / "settings.yaml").write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    (tmp_path / "data").mkdir()
    (tmp_path / "workspace").mkdir()

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_config("deepseek", "sk-test", tmp_path),
    )

    assert not any("缺少配置文件" in message for message in result.errors)
    assert result.info["本机配置"] == "config/settings.yaml"
    assert any(
        "settings.yaml" in message and "迁移" in message
        for message in result.warnings
    )


def test_collect_healthcheck_reports_missing_work_dir(tmp_path, monkeypatch):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    (tmp_path / "data").mkdir()

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [("rich", "rich")])
    monkeypatch.setattr(
        healthcheck.importlib,
        "import_module",
        lambda module_name: object(),
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_config("deepseek", "sk-test-123456", tmp_path),
    )

    assert f"工作目录不存在: {tmp_path / 'workspace'}" in result.errors


def test_collect_healthcheck_redacts_runtime_config_exception(tmp_path, monkeypatch):
    """配置解析异常可能包含密钥原文，结果中只能保留异常类型。"""
    _prepare_platform_config_root(tmp_path)

    fake_secret = "dummy-secret-must-not-leak"

    class ExplodingConfig:
        @property
        def llm_provider(self):
            raise ValueError(fake_secret)

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=ExplodingConfig(),
    )

    assert "配置加载失败 (ValueError)，请检查本机配置格式" in result.errors
    assert fake_secret not in repr(result)


def test_collect_healthcheck_reports_unknown_scanner_settings(
    tmp_path,
    monkeypatch,
):
    """受控的 scanner 字段错误应直接指出字段名，避免只得到通用提示。"""
    _prepare_platform_config_root(tmp_path)

    cfg = _make_config("deepseek", "synthetic-key", tmp_path)
    cfg.scanner_settings = lambda: (_ for _ in ()).throw(
        UnknownScannerSettingsError(
            ("worker_lane_mode", "discovery_backend")
        )
    )
    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=cfg,
    )

    assert result.errors == [
        "配置校验失败: unknown scanner settings: "
        "discovery_backend, worker_lane_mode"
    ]


@pytest.mark.parametrize("api_key", [None, "", "   ", "  ${OPENAI_API_KEY}"])
def test_collect_healthcheck_reports_missing_provider_api_key(
    tmp_path,
    monkeypatch,
    api_key,
):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: openai\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    (tmp_path / "data").mkdir()

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [("rich", "rich")])
    monkeypatch.setattr(
        healthcheck.importlib,
        "import_module",
        lambda module_name: object(),
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_config("openai", api_key, tmp_path),
    )

    assert "未配置 OPENAI_API_KEY" in result.errors


@pytest.mark.parametrize(
    ("missing_module", "package_name"),
    [
        ("docx", "python-docx"),
        ("sharepoint2text", "sharepoint-to-text"),
    ],
)
def test_collect_healthcheck_reports_missing_office_dependency(
    tmp_path,
    monkeypatch,
    missing_module,
    package_name,
):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    (tmp_path / "data").mkdir()
    (tmp_path / "workspace").mkdir()

    def import_module(module_name):
        if module_name == missing_module:
            raise ImportError(module_name)
        return object()

    monkeypatch.setattr(healthcheck.importlib, "import_module", import_module)

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_config("deepseek", "sk-test", tmp_path),
    )

    assert f"缺少依赖包: {package_name}" in result.errors


@pytest.mark.parametrize("model_id", [None, "", "   "])
def test_collect_healthcheck_reports_blank_llm_model(
    tmp_path,
    monkeypatch,
    model_id,
):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    (tmp_path / "data").mkdir()
    (tmp_path / "workspace").mkdir()
    config_obj = _make_config("deepseek", "sk-test", tmp_path)
    config_obj.llm_config["model_id"] = model_id

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=config_obj,
    )

    assert "未配置 LLM 模型" in result.errors


def test_collect_healthcheck_reports_unwritable_report_directory(
    tmp_path,
    monkeypatch,
):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    data_dir = tmp_path / "data"
    data_dir.mkdir()
    (data_dir / "reports").write_text("occupied", encoding="utf-8")
    (tmp_path / "workspace").mkdir()

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_config("deepseek", "sk-test", tmp_path),
    )

    assert any(
        message.startswith(f"报告目录不可写: {data_dir / 'reports'}")
        for message in result.errors
    )


def test_collect_healthcheck_reports_unwritable_database_directory(
    tmp_path,
    monkeypatch,
):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    data_dir = tmp_path / "data"
    data_dir.mkdir()
    (data_dir / "db").write_text("occupied", encoding="utf-8")
    (tmp_path / "workspace").mkdir()

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_config("deepseek", "sk-test", tmp_path),
    )

    assert any(
        message.startswith(f"数据库目录不可写: {data_dir / 'db'}")
        for message in result.errors
    )


def test_collect_healthcheck_reports_unwritable_log_directory(
    tmp_path,
    monkeypatch,
):
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)
    (tmp_path / "data").mkdir()
    (tmp_path / "workspace").mkdir()
    (tmp_path / "logs").write_text("occupied", encoding="utf-8")

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_config("deepseek", "sk-test", tmp_path),
    )

    assert any(
        message.startswith(f"日志目录不可写: {tmp_path / 'logs'}")
        for message in result.errors
    )


def test_strict_installed_healthcheck_reports_all_shared_runtime_paths(
    tmp_path,
    monkeypatch,
):
    release = tmp_path / "install" / "releases" / "版本-a"
    release.mkdir(parents=True)
    _prepare_strict_root(release)
    shared = tmp_path / "install" / "shared"
    config_dir = shared / "config"
    data_dir = shared / "data"
    reports_dir = data_dir / "reports"
    db_dir = data_dir / "db"
    log_dir = shared / "logs"
    for directory in (config_dir, reports_dir, db_dir, log_dir):
        directory.mkdir(parents=True, exist_ok=True)
    settings = config_dir / "settings.windows.yaml"
    settings.write_text("llm:\n  provider: deepseek\n", encoding="utf-8")
    cfg = _make_strict_rust_config(release)
    cfg.installed_mode = True
    cfg.install_root = tmp_path / "install"
    cfg.config_dir = config_dir
    cfg.config_source = settings
    cfg.data_dir = data_dir
    cfg.reports_dir = reports_dir
    cfg.db_dir = db_dir
    cfg.log_dir = log_dir

    class StubNativeScanner:
        def __init__(self, runtime_config, **kwargs):
            pass

        def doctor(self):
            return _strict_doctor(
                ("scan_db_parent", "ok"),
                ("office_worker_handshake", "ok"),
                ("python_worker_handshake", "ok"),
            )

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(healthcheck, "NativeScanner", StubNativeScanner)

    result = healthcheck.collect_healthcheck(
        project_root=release,
        config_obj=cfg,
        strict=True,
    )

    assert result.errors == []
    assert result.info["运行模式"] == "installed"
    assert result.info["配置来源"] == str(settings)
    assert result.info["数据目录"] == str(data_dir)
    assert result.info["报告目录"] == str(reports_dir)
    assert result.info["数据库目录"] == str(db_dir)
    assert result.info["日志目录"] == str(log_dir)


def test_strict_installed_healthcheck_rejects_version_local_log_path(
    tmp_path,
    monkeypatch,
):
    release = tmp_path / "install" / "releases" / "v1"
    release.mkdir(parents=True)
    _prepare_strict_root(release)
    cfg = _make_strict_rust_config(release)
    cfg.installed_mode = True
    cfg.install_root = tmp_path / "install"
    cfg.config_dir = tmp_path / "install" / "shared" / "config"
    cfg.data_dir = tmp_path / "install" / "shared" / "data"
    cfg.reports_dir = cfg.data_dir / "reports"
    cfg.db_dir = cfg.data_dir / "db"
    cfg.log_dir = release / "logs"
    for directory in (
        cfg.config_dir,
        cfg.data_dir,
        cfg.reports_dir,
        cfg.db_dir,
        cfg.log_dir,
    ):
        directory.mkdir(parents=True, exist_ok=True)

    class StubNativeScanner:
        def __init__(self, runtime_config, **kwargs):
            pass

        def doctor(self):
            return _strict_doctor(
                ("scan_db_parent", "ok"),
                ("office_worker_handshake", "ok"),
                ("python_worker_handshake", "ok"),
            )

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(healthcheck, "NativeScanner", StubNativeScanner)

    result = healthcheck.collect_healthcheck(
        project_root=release,
        config_obj=cfg,
        strict=True,
    )

    assert any(
        message.startswith("installed mode 日志目录 escaped install-root/shared")
        for message in result.errors
    )
