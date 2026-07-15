"""Tests for CLI health checks."""

from pathlib import Path
from types import SimpleNamespace

import pytest

from src.core import healthcheck


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
        scanner_config={"max_workers": 4},
        reports_dir=root / "data" / "reports",
        db_dir=root / "data" / "db",
        deepseek_api_key=api_key if provider == "deepseek" else "",
        openai_api_key=api_key if provider == "openai" else "",
    )


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


def test_collect_healthcheck_resolves_windows_rust_executables(
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

    release_dir = tmp_path / "rust" / "target" / "release"
    discovery_bin = release_dir / "ai-daily-discovery.exe"
    office_bin = release_dir / "ai-daily-office-parser.exe"
    release_dir.mkdir(parents=True)
    discovery_bin.write_bytes(b"test-rust-binary")
    office_bin.write_bytes(b"test-rust-binary")

    config_obj = _make_config("deepseek", "sk-test", tmp_path)
    config_obj.scanner_config = {
        "max_workers": 4,
        "discovery_backend": "rust",
        "rust_discovery_bin": (
            "rust/target/release/ai-daily-discovery"
        ),
        "office_parser_backend": "rust_office_oxide_v1",
        "rust_office_parser_bin": (
            "rust/target/release/ai-daily-office-parser"
        ),
        "office_parser_fallback_enabled": True,
        "office_parser_fallback_order": ["python_office_v1"],
    }

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(healthcheck.platform, "system", lambda: "Windows")
    monkeypatch.setattr(
        healthcheck.subprocess,
        "run",
        lambda *args, **kwargs: SimpleNamespace(
            returncode=1,
            stdout="",
            stderr="error: EOF while parsing a value",
        ),
    )

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=config_obj,
    )

    assert result.info["Rust Discovery CLI"] == str(discovery_bin)
    assert result.info["Rust Office Parser CLI"] == str(office_bin)
    assert result.info["Rust Discovery CLI 状态"] == "可启动"
    assert result.info["Rust Office Parser CLI 状态"] == "可启动"
    assert not any("Rust" in message for message in result.errors)
    assert not any("Rust" in message for message in result.warnings)


def test_collect_healthcheck_warns_when_rust_cli_file_cannot_start(
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
    discovery_bin = tmp_path / "bin" / "discovery.exe"
    discovery_bin.parent.mkdir()
    discovery_bin.write_bytes(b"")

    config_obj = _make_config("deepseek", "sk-test", tmp_path)
    config_obj.scanner_config = {
        "max_workers": 4,
        "discovery_backend": "rust",
        "rust_discovery_bin": "bin/discovery",
        "office_parser_backend": "python_office_v1",
    }

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(healthcheck.platform, "system", lambda: "Windows")

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=config_obj,
    )

    assert not any("Rust" in message for message in result.errors)
    assert result.info["Rust Discovery CLI 状态"] == (
        "无法启动，将回退 Python discovery"
    )
    assert any(
        "Rust Discovery CLI 无法启动" in message
        and "回退 Python discovery" in message
        for message in result.warnings
    )


def test_collect_healthcheck_warns_when_missing_rust_clis_have_fallback(
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

    config_obj = _make_config("deepseek", "sk-test", tmp_path)
    config_obj.scanner_config = {
        "max_workers": 4,
        "discovery_backend": "rust",
        "rust_discovery_bin": "bin/discovery",
        "office_parser_backend": "rust_office_oxide_v1",
        "rust_office_parser_bin": "bin/office-parser",
        "office_parser_fallback_enabled": True,
    }

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(healthcheck.platform, "system", lambda: "Windows")

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=config_obj,
    )

    assert not any("Rust" in message for message in result.errors)
    assert any(
        "Rust Discovery CLI 不存在" in message
        and "回退 Python discovery" in message
        for message in result.warnings
    )
    assert any(
        "Rust Office Parser CLI 不存在" in message
        and "回退 Python Office parser" in message
        for message in result.warnings
    )
    assert result.info["Rust Discovery CLI 状态"] == "缺失，将回退 Python discovery"
    assert result.info["Rust Office Parser CLI 状态"] == (
        "缺失，将回退 Python Office parser"
    )


def test_collect_healthcheck_errors_when_missing_office_cli_has_no_fallback(
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

    config_obj = _make_config("deepseek", "sk-test", tmp_path)
    config_obj.scanner_config = {
        "max_workers": 4,
        "discovery_backend": "python",
        "office_parser_backend": "rust_office_oxide_v1",
        "rust_office_parser_bin": "bin/office-parser",
        "office_parser_fallback_enabled": False,
        "office_parser_fallback_order": [],
    }

    monkeypatch.setattr(healthcheck, "REQUIRED_DEPENDENCIES", [])
    monkeypatch.setattr(healthcheck.platform, "system", lambda: "Windows")

    result = healthcheck.collect_healthcheck(
        project_root=tmp_path,
        config_obj=config_obj,
    )

    assert any(
        "Rust Office Parser CLI 不存在" in message
        and "无可用 fallback" in message
        for message in result.errors
    )
    assert result.info["Rust Office Parser CLI 状态"] == "缺失，无可用 fallback"
