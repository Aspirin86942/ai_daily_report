"""Tests for CLI health checks."""

from pathlib import Path
from types import SimpleNamespace

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


def _make_config(provider: str, api_key: str, root: Path) -> SimpleNamespace:
    return SimpleNamespace(
        llm_provider=provider,
        work_dir=root / "workspace",
        llm_config={"model_id": "deepseek-chat", "max_tokens": 8192},
        scanner_config={"max_workers": 4},
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
    assert result.info["API Key"] == "sk-test-12..."


def test_collect_healthcheck_reports_missing_provider_api_key(tmp_path, monkeypatch):
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
        config_obj=_make_config("openai", "", tmp_path),
    )

    assert "未配置 OPENAI_API_KEY" in result.errors
