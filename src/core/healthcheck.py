"""CLI 运行环境检查。"""

from __future__ import annotations

import importlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .config import Config, config

TEMPLATE_FILES: tuple[str, ...] = (
    "system_prompt.md",
    "report_template.md",
    "weekly_prompt.md",
    "monthly_prompt.md",
    "weekly_template.md",
    "monthly_template.md",
)

REQUIRED_DEPENDENCIES: list[tuple[str, str]] = [
    ("openai", "openai"),
    ("pydantic", "pydantic"),
    ("dynaconf", "dynaconf"),
    ("PyYAML", "yaml"),
    ("rich", "rich"),
    ("pandas", "pandas"),
    ("openpyxl", "openpyxl"),
    ("python-pptx", "pptx"),
    ("pdfplumber", "pdfplumber"),
    ("jinja2", "jinja2"),
]


@dataclass
class HealthCheckResult:
    """健康检查结果。"""

    info: dict[str, str] = field(default_factory=dict)
    warnings: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)


def _mask_api_key(api_key: str) -> str:
    """返回脱敏后的 API Key。"""

    return f"{api_key[:10]}..."


def _relative_display_path(path: Path, root: Path) -> str:
    """诊断输出统一使用 /，避免 Windows 反斜杠影响测试和复制排查。"""
    return path.relative_to(root).as_posix()


def _append_project_file_checks(result: HealthCheckResult, project_root: Path) -> None:
    """检查项目基础文件。

    这些检查放在前面，是为了先定位最常见的安装和拷贝不完整问题，
    避免后续配置加载异常把真实原因淹没掉。
    """

    config_dir = project_root / "config"
    settings_file = config_dir / Config._settings_file_name()
    secrets_file = config_dir / ".secrets.yaml"

    if not settings_file.exists():
        result.errors.append(
            f"缺少配置文件: {_relative_display_path(settings_file, project_root)}"
        )

    if not secrets_file.exists():
        result.warnings.append(
            f"缺少敏感配置文件: {_relative_display_path(secrets_file, project_root)} "
            "(如已通过环境变量配置 API Key，可忽略)"
        )

    templates_dir = project_root / "templates"
    for template_name in TEMPLATE_FILES:
        template_path = templates_dir / template_name
        if not template_path.exists():
            result.errors.append(
                f"缺少模板文件: {_relative_display_path(template_path, project_root)}"
            )

    data_dir = project_root / "data"
    if not data_dir.exists():
        result.warnings.append(
            f"数据目录不存在: {_relative_display_path(data_dir, project_root)} (将自动创建)"
        )


def _resolve_provider_key(cfg: Any, provider: str) -> tuple[str, str]:
    """返回当前 provider 对应的 API Key 与环境变量名。"""

    if provider == "openai":
        return str(getattr(cfg, "openai_api_key", "")), "OPENAI_API_KEY"
    if provider == "deepseek":
        return str(getattr(cfg, "deepseek_api_key", "")), "DEEPSEEK_API_KEY"
    return "", ""


def _append_runtime_config_checks(result: HealthCheckResult, cfg: Any) -> None:
    """检查运行期配置。

    这里按当前激活的 provider 校验密钥，而不是同时检查所有 provider，
    这样可以避免“配置了备用 provider 但未启用”时产生误报。
    """

    provider = str(getattr(cfg, "llm_provider", "")).strip().lower()
    result.info["LLM Provider"] = provider
    result.info["工作目录"] = str(getattr(cfg, "work_dir"))
    result.info["LLM 模型"] = str(getattr(cfg, "llm_config")["model_id"])
    result.info["最大并发"] = str(getattr(cfg, "scanner_config")["max_workers"])
    result.info["SQLite DB"] = str(Path(getattr(cfg, "db_dir")) / "reports.sqlite3")

    if provider not in {"deepseek", "openai"}:
        result.errors.append(
            f"不支持的 LLM Provider: {provider} (仅支持 deepseek 或 openai)"
        )
        return

    api_key, missing_env = _resolve_provider_key(cfg, provider)
    if not api_key or api_key.startswith("${"):
        result.errors.append(f"未配置 {missing_env}")
        return

    result.info["API Key"] = _mask_api_key(api_key)


def _append_dependency_checks(result: HealthCheckResult) -> None:
    """检查关键依赖是否可导入。"""

    for package_name, import_name in REQUIRED_DEPENDENCIES:
        try:
            importlib.import_module(import_name)
        except ImportError:
            result.errors.append(f"缺少依赖包: {package_name}")


def collect_healthcheck(
    project_root: Path | None = None,
    config_obj: Any | None = None,
) -> HealthCheckResult:
    """汇总运行环境检查结果。"""

    root = project_root or Path(__file__).resolve().parent.parent.parent
    cfg = config_obj or config
    result = HealthCheckResult()

    _append_project_file_checks(result, root)

    try:
        _append_runtime_config_checks(result, cfg)
    except Exception as exc:  # pragma: no cover - 异常文本由调用方展示
        result.errors.append(f"配置加载失败: {exc}")

    _append_dependency_checks(result)
    return result
