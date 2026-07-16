"""Tests for CLI health checks."""

from pathlib import Path
from types import SimpleNamespace

import pytest

from src.core import healthcheck
from src.services.rust_context_client import RustContextProbeError


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
        scanner_contract_profile=lambda: {
            "schema_version": "scanner_profile_v1",
            "max_workers": 4,
        },
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
    cfg.scanner_engine = "rust_v2"
    cfg.rust_scanner_bin = "bin/ai-daily-scanner"
    cfg.rust_office_parser_bin = "bin/ai-daily-office-parser"
    cfg.rust_index_db_path = "data/db/scan_index_v2.sqlite3"
    cfg.rust_process_timeout_seconds = 30.0
    cfg.scanner_contract_profile = lambda: {
        "schema_version": "scanner_profile_v1",
        "max_workers": 4,
        "allowed_extensions": allowed_extensions or [".txt"],
    }
    return cfg


def _strict_version() -> SimpleNamespace:
    return SimpleNamespace(
        contract="ai_daily_context",
        protocol_version=1,
        engine_version="0.1.0",
        engine_build="sha256-source-v1:synthetic",
        target_triple="x86_64-pc-windows-msvc",
    )


def _strict_doctor(*checks: tuple[str, str]) -> SimpleNamespace:
    return SimpleNamespace(
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


def test_collect_healthcheck_strict_uses_effective_config_without_local_yaml(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)
    captured: dict[str, object] = {}

    class StubRustContextClient:
        def __init__(self, **kwargs):
            captured.update(kwargs)

        def version(self):
            return _strict_version()

        def doctor(self):
            return _strict_doctor(
                ("scan_db_parent", "ok"),
                ("office_worker_handshake", "ok"),
                ("python_worker_handshake", "ok"),
            )

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(
        healthcheck,
        "RustContextClient",
        StubRustContextClient,
        raising=False,
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_strict_rust_config(tmp_path),
        strict=True,
    )

    assert result.errors == []
    assert not any("缺少配置文件" in message for message in result.errors)
    assert result.info["Scanner Engine"] == "rust_v2"
    assert result.info["Rust Scanner Contract"] == "ai_daily_context/v1"
    assert result.info["Rust Scanner Engine"] == (
        "0.1.0 (x86_64-pc-windows-msvc)"
    )
    assert captured["scanner_binary"] == "bin/ai-daily-scanner"
    assert captured["scan_db_path"] == "data/db/scan_index_v2.sqlite3"
    assert captured["office_worker_path"] == "bin/ai-daily-office-parser"


def test_collect_healthcheck_strict_always_requires_office_worker_package(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)

    class StubRustContextClient:
        def __init__(self, **kwargs):
            pass

        def version(self):
            return _strict_version()

        def doctor(self):
            return _strict_doctor(
                ("scan_db_parent", "ok"),
                ("office_worker_handshake", "error"),
                ("python_worker_handshake", "ok"),
            )

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(
        healthcheck,
        "RustContextClient",
        StubRustContextClient,
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_strict_rust_config(
            tmp_path,
            allowed_extensions=[".txt"],
        ),
        strict=True,
    )

    assert "Rust strict check failed: office_worker_handshake" in result.errors


def test_collect_healthcheck_strict_rejects_non_rust_effective_engine(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)
    cfg = _make_strict_rust_config(tmp_path)
    cfg.scanner_engine = "retired"
    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=cfg,
        strict=True,
    )

    assert "严格模式要求 scanner.engine=rust_v2" in result.errors


def test_collect_healthcheck_strict_reports_safe_scanner_contract_failure(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)

    class FailingRustContextClient:
        def __init__(self, **kwargs):
            pass

        def version(self):
            raise RustContextProbeError("version", "invalid_response")

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(
        healthcheck,
        "RustContextClient",
        FailingRustContextClient,
        raising=False,
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=_make_strict_rust_config(tmp_path),
        strict=True,
    )

    assert result.errors == [
        "Rust scanner version/contract check failed (invalid_response)"
    ]


def test_collect_healthcheck_strict_requires_configured_worker_routes(
    tmp_path,
    monkeypatch,
):
    _prepare_strict_root(tmp_path)

    class StubRustContextClient:
        def __init__(self, **kwargs):
            pass

        def version(self):
            return _strict_version()

        def doctor(self):
            return _strict_doctor(
                ("scan_db_parent", "ok"),
                ("office_worker_handshake", "error"),
                ("python_worker_handshake", "error"),
            )

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(
        healthcheck,
        "RustContextClient",
        StubRustContextClient,
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

    assert "Rust strict check failed: office_worker_handshake" in result.errors
    assert "Rust strict check failed: python_worker_handshake" in result.errors


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
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    (config_dir / healthcheck.Config._settings_file_name()).write_text(
        "llm:\n  provider: deepseek\n",
        encoding="utf-8",
    )
    _write_templates(tmp_path)

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
