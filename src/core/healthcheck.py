"""CLI 运行环境检查。"""

from __future__ import annotations

import importlib
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .config import Config, UnknownScannerSettingsError, config
from ..services.native_scanner import NativeScanner, NativeScannerError

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
    ("python-docx", "docx"),
    ("sharepoint-to-text", "sharepoint2text"),
    ("pdfplumber", "pdfplumber"),
    ("jinja2", "jinja2"),
]

STRICT_RUST_PACKAGE_CHECKS: tuple[str, ...] = (
    "scan_db_parent",
    "office_worker_handshake",
    "python_worker_handshake",
)


@dataclass
class HealthCheckResult:
    """健康检查结果。"""

    info: dict[str, str] = field(default_factory=dict)
    warnings: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)


def _relative_display_path(path: Path, root: Path) -> str:
    """诊断输出统一使用 /，避免 Windows 反斜杠影响测试和复制排查。"""
    try:
        return path.relative_to(root).as_posix()
    except ValueError:
        return str(path)


def _append_project_file_checks(
    result: HealthCheckResult,
    project_root: Path,
    cfg: Any,
    *,
    strict: bool = False,
) -> None:
    """检查项目基础文件。

    这些检查放在前面，是为了先定位最常见的安装和拷贝不完整问题，
    避免后续配置加载异常把真实原因淹没掉。
    """

    config_dir = Path(getattr(cfg, "config_dir", project_root / "config"))
    settings_file = config_dir / Config._settings_file_name()
    generic_settings_file = config_dir / "settings.yaml"
    secrets_file = config_dir / ".secrets.yaml"

    if settings_file.exists():
        result.info["本机配置"] = _relative_display_path(
            settings_file,
            project_root,
        )
    elif generic_settings_file.exists():
        result.info["本机配置"] = _relative_display_path(
            generic_settings_file,
            project_root,
        )
        result.warnings.append(
            "检测到通用本机配置: "
            f"{_relative_display_path(generic_settings_file, project_root)}；"
            f"新部署建议迁移到 {_relative_display_path(settings_file, project_root)}"
        )
    elif not strict:
        result.errors.append(
            f"缺少配置文件: {_relative_display_path(settings_file, project_root)}"
        )
    else:
        result.info["本机配置"] = "有效配置（环境变量或调用方）"

    if not secrets_file.exists():
        result.warnings.append(
            f"缺少敏感配置文件: {_relative_display_path(secrets_file, project_root)} "
            "(如已通过环境变量或其他本机配置提供 API Key，可忽略)"
        )

    templates_dir = project_root / "templates"
    for template_name in TEMPLATE_FILES:
        template_path = templates_dir / template_name
        if not template_path.exists():
            result.errors.append(
                f"缺少模板文件: {_relative_display_path(template_path, project_root)}"
            )

    data_dir = Path(getattr(cfg, "data_dir", project_root / "data"))
    if not data_dir.exists():
        result.warnings.append(
            f"数据目录不存在: {_relative_display_path(data_dir, project_root)} (将自动创建)"
        )


def _normalize_optional_text(value: Any) -> str:
    """把 YAML null 与纯空白配置统一收敛为空字符串。"""

    if value is None:
        return ""
    return str(value).strip()


def _resolve_provider_key(cfg: Any, provider: str) -> tuple[str, str]:
    """返回当前 provider 对应的 API Key 与环境变量名。"""

    if provider == "openai":
        return _normalize_optional_text(
            getattr(cfg, "openai_api_key", "")
        ), "OPENAI_API_KEY"
    if provider == "deepseek":
        return _normalize_optional_text(
            getattr(cfg, "deepseek_api_key", "")
        ), "DEEPSEEK_API_KEY"
    return "", ""


def _append_runtime_config_checks(
    result: HealthCheckResult,
    cfg: Any,
    project_root: Path,
    *,
    strict: bool = False,
) -> None:
    """检查运行期配置。

    这里按当前激活的 provider 校验密钥，而不是同时检查所有 provider，
    这样可以避免“配置了备用 provider 但未启用”时产生误报。
    """

    provider = str(getattr(cfg, "llm_provider", "")).strip().lower()
    work_dir = Path(getattr(cfg, "work_dir"))
    model_id = _normalize_optional_text(
        getattr(cfg, "llm_config").get("model_id", "")
    )
    reports_dir = Path(getattr(cfg, "reports_dir"))
    db_dir = Path(getattr(cfg, "db_dir"))
    data_dir = Path(getattr(cfg, "data_dir", project_root / "data"))
    log_dir = Path(getattr(cfg, "log_dir", project_root / "logs"))
    config_dir = Path(getattr(cfg, "config_dir", project_root / "config"))
    installed_mode = bool(getattr(cfg, "installed_mode", False))
    scanner_settings = cfg.scanner_settings()
    result.info["运行模式"] = "installed" if installed_mode else "source"
    if installed_mode:
        result.info["安装根目录"] = str(getattr(cfg, "install_root"))
    result.info["配置目录"] = str(config_dir)
    result.info["数据目录"] = str(data_dir)
    result.info["报告目录"] = str(reports_dir)
    result.info["数据库目录"] = str(db_dir)
    result.info["日志目录"] = str(log_dir)
    config_source = getattr(cfg, "config_source", None)
    if config_source is not None:
        result.info["配置来源"] = str(config_source)
    result.info["LLM Provider"] = provider
    result.info["工作目录"] = str(work_dir)
    result.info["LLM 模型"] = model_id
    max_workers = scanner_settings.get("max_workers")
    result.info["最大并发"] = (
        str(max_workers) if max_workers is not None else "Rust 默认"
    )
    result.info["SQLite DB"] = str(db_dir / "reports.sqlite3")

    _append_work_dir_check(result, work_dir)
    _append_writable_directory_check(result, "报告目录", reports_dir)
    _append_writable_directory_check(result, "数据库目录", db_dir)
    _append_writable_directory_check(result, "日志目录", log_dir)
    if strict:
        _append_installed_containment_checks(result, cfg)
        _append_strict_rust_core_checks(
            result,
            cfg,
            project_root,
        )

    if not model_id:
        result.errors.append("未配置 LLM 模型")

    if provider not in {"deepseek", "openai"}:
        result.errors.append(
            f"不支持的 LLM Provider: {provider} (仅支持 deepseek 或 openai)"
        )
        return

    api_key, missing_env = _resolve_provider_key(cfg, provider)
    if not api_key or api_key.startswith("${"):
        result.errors.append(f"未配置 {missing_env}")
        return

    # doctor 只确认凭据存在，绝不回显任何可用于识别密钥的片段。
    result.info["API Key"] = "已配置"


def _append_strict_rust_core_checks(
    result: HealthCheckResult,
    cfg: Any,
    project_root: Path,
) -> None:
    """验证原生 scanner 与两个隔离 worker，不暴露底层错误内容。"""

    result.info["Scanner Interface"] = "native"

    try:
        doctor = NativeScanner(
            cfg,
            project_root=project_root,
        )
        doctor = doctor.doctor()
    except NativeScannerError as exc:
        result.errors.append(f"Native scanner doctor failed ({exc.error_code})")
        return
    except Exception as exc:
        result.errors.append(
            f"Native scanner doctor failed ({type(exc).__name__})"
        )
        return

    result.info["Native Scanner Contract"] = (
        f"{doctor.contract}/v{doctor.protocol_version}"
    )
    result.info["Native Scanner Engine"] = doctor.engine_version
    result.info["Native Scanner Build"] = doctor.engine_build

    checks = {check.name: check for check in doctor.checks}
    # 严格部署验证完整生产包；即使当前 profile 暂不使用某个 worker，
    # 也必须在切换配置前证明两个隔离 worker 都可启动且合同匹配。
    for check_name in STRICT_RUST_PACKAGE_CHECKS:
        check = checks.get(check_name)
        if check is None:
            result.errors.append(f"Native strict check missing: {check_name}")
            continue
        result.info[f"Scanner {check_name}"] = str(check.status)
        if check.status != "ok":
            result.errors.append(f"Native strict check failed: {check_name}")


def _append_installed_containment_checks(
    result: HealthCheckResult,
    cfg: Any,
) -> None:
    """Strict doctor proves installed runtime state stays below shared/."""

    if not bool(getattr(cfg, "installed_mode", False)):
        return
    install_root = Path(getattr(cfg, "install_root")).resolve()
    shared_root = (install_root / "shared").resolve()
    for label, attribute in (
        ("配置目录", "config_dir"),
        ("数据目录", "data_dir"),
        ("报告目录", "reports_dir"),
        ("数据库目录", "db_dir"),
        ("日志目录", "log_dir"),
    ):
        path = Path(getattr(cfg, attribute)).resolve()
        if not path.is_relative_to(shared_root):
            result.errors.append(
                f"installed mode {label} escaped install-root/shared: {path}"
            )


def _append_work_dir_check(result: HealthCheckResult, work_dir: Path) -> None:
    """校验扫描根目录，避免路径不可达被后续扫描误判为空结果。"""

    try:
        if not work_dir.exists():
            result.errors.append(f"工作目录不存在: {work_dir}")
            return
        if not work_dir.is_dir():
            result.errors.append(f"工作目录不是目录: {work_dir}")
    except OSError as exc:
        result.errors.append(f"工作目录不可访问: {work_dir} ({exc})")


def _append_writable_directory_check(
    result: HealthCheckResult,
    label: str,
    directory: Path,
) -> None:
    """确认运行目录能被创建并写入，临时探针在检查结束时自动删除。"""

    result.info[label] = str(directory)
    try:
        directory.mkdir(parents=True, exist_ok=True)
        if not directory.is_dir():
            raise NotADirectoryError("路径不是目录")
        with tempfile.NamedTemporaryFile(prefix=".doctor-", dir=directory):
            pass
    except OSError as exc:
        result.errors.append(f"{label}不可写: {directory} ({exc})")

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
    *,
    strict: bool = False,
) -> HealthCheckResult:
    """汇总运行环境检查结果。"""

    root = project_root or Path(__file__).resolve().parent.parent.parent
    cfg = config_obj or config
    result = HealthCheckResult()

    _append_project_file_checks(result, root, cfg, strict=strict)

    try:
        _append_runtime_config_checks(result, cfg, root, strict=strict)
    except UnknownScannerSettingsError as exc:
        # 该异常只包含配置字段名，可安全展示，帮助定位过期或拼错的配置。
        result.errors.append(f"配置校验失败: {exc}")
    except Exception as exc:
        # YAML 解析异常可能携带出错原文；这里只保留类型，避免回显密钥。
        result.errors.append(
            f"配置加载失败 ({type(exc).__name__})，请检查本机配置格式"
        )

    _append_dependency_checks(result)
    return result
