"""CLI 运行环境检查。"""

from __future__ import annotations

import importlib
import platform
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .config import Config, config
from ..services.rust_cli_contract import resolve_binary_path
from ..services.rust_context_client import (
    RustContextClient,
    RustContextProbeError,
)

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

RUST_CLI_PROBE_TIMEOUT_SECONDS = 3.0
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
    return path.relative_to(root).as_posix()


def _append_project_file_checks(
    result: HealthCheckResult,
    project_root: Path,
    *,
    strict: bool = False,
) -> None:
    """检查项目基础文件。

    这些检查放在前面，是为了先定位最常见的安装和拷贝不完整问题，
    避免后续配置加载异常把真实原因淹没掉。
    """

    config_dir = project_root / "config"
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

    data_dir = project_root / "data"
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
    scanner_config = getattr(cfg, "scanner_config")
    result.info["LLM Provider"] = provider
    result.info["工作目录"] = str(work_dir)
    result.info["LLM 模型"] = model_id
    result.info["最大并发"] = str(scanner_config["max_workers"])
    result.info["SQLite DB"] = str(db_dir / "reports.sqlite3")

    _append_work_dir_check(result, work_dir)
    _append_writable_directory_check(result, "报告目录", reports_dir)
    _append_writable_directory_check(result, "数据库目录", db_dir)
    _append_writable_directory_check(result, "日志目录", project_root / "logs")
    if strict:
        _append_strict_rust_core_checks(
            result,
            cfg,
            scanner_config,
            project_root,
        )
    else:
        _append_rust_cli_checks(result, scanner_config, project_root)

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
    scanner_config: Any,
    project_root: Path,
) -> None:
    """Validate the effective Rust production path without exposing stderr."""

    engine = str(getattr(cfg, "scanner_engine", "")).strip().lower()
    result.info["Scanner Engine"] = engine
    if engine != "rust_v2":
        result.errors.append("严格模式要求 scanner.engine=rust_v2")
        return

    try:
        configured_timeout = float(
            getattr(cfg, "rust_process_timeout_seconds", 30.0)
        )
        client = RustContextClient(
            config=cfg,
            project_root=project_root,
            scanner_binary=getattr(cfg, "rust_scanner_bin"),
            scan_db_path=getattr(cfg, "rust_index_db_path"),
            office_worker_path=scanner_config.get(
                "rust_office_parser_bin",
                "rust/target/release/ai-daily-office-parser",
            ),
            timeout_seconds=min(configured_timeout, 30.0),
        )
        version = client.version()
    except RustContextProbeError as exc:
        result.errors.append(
            "Rust scanner version/contract check failed "
            f"({exc.kind})"
        )
        return
    except Exception as exc:
        result.errors.append(
            "Rust scanner version/contract check failed "
            f"({type(exc).__name__})"
        )
        return

    result.info["Rust Scanner Contract"] = (
        f"{version.contract}/v{version.protocol_version}"
    )
    result.info["Rust Scanner Engine"] = (
        f"{version.engine_version} ({version.target_triple})"
    )
    result.info["Rust Scanner Build"] = version.engine_build

    try:
        doctor = client.doctor()
    except RustContextProbeError as exc:
        result.errors.append(f"Rust scanner doctor failed ({exc.kind})")
        return
    except Exception as exc:
        result.errors.append(
            f"Rust scanner doctor failed ({type(exc).__name__})"
        )
        return

    if (
        doctor.engine_version != version.engine_version
        or doctor.engine_build != version.engine_build
    ):
        result.errors.append("Rust scanner doctor identity mismatch")
        return

    checks = {check.name: check for check in doctor.checks}
    # 严格部署验证完整生产包；即使当前 profile 暂不使用某个 worker，
    # 也必须在切换配置前证明两个隔离 worker 都可启动且合同匹配。
    for check_name in STRICT_RUST_PACKAGE_CHECKS:
        check = checks.get(check_name)
        if check is None:
            result.errors.append(f"Rust strict check missing: {check_name}")
            continue
        result.info[f"Rust {check_name}"] = str(check.status)
        if check.status != "ok":
            result.errors.append(f"Rust strict check failed: {check_name}")

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


def _probe_rust_cli(binary_path: Path) -> str | None:
    """用空 stdin 验证 Rust JSON CLI 可启动且仍遵循统一错误前缀。"""

    try:
        completed = subprocess.run(
            [str(binary_path)],
            input="",
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            timeout=RUST_CLI_PROBE_TIMEOUT_SECONDS,
            check=False,
        )
    except (subprocess.TimeoutExpired, TimeoutError):
        return f"启动检查超过 {RUST_CLI_PROBE_TIMEOUT_SECONDS:g} 秒"
    except OSError as exc:
        return f"启动失败: {exc}"

    stderr = completed.stderr if isinstance(completed.stderr, str) else ""
    if completed.returncode == 1 and stderr.lstrip().startswith("error:"):
        return None
    return f"空请求契约异常 (exit code {completed.returncode})"


def _append_rust_cli_checks(
    result: HealthCheckResult,
    scanner_config: Any,
    project_root: Path,
) -> None:
    """展示当前实际会启动的 Rust helper 路径。"""

    if str(scanner_config.get("discovery_backend", "rust")).strip().lower() == "rust":
        discovery_path = resolve_binary_path(
            scanner_config.get(
                "rust_discovery_bin",
                "rust/target/release/ai-daily-discovery",
            ),
            project_root=project_root,
            system_name=platform.system(),
        )
        result.info["Rust Discovery CLI"] = str(discovery_path)
        if not discovery_path.is_file():
            result.info["Rust Discovery CLI 状态"] = "缺失，将回退 Python discovery"
            result.warnings.append(
                f"Rust Discovery CLI 不存在: {discovery_path}；"
                "将回退 Python discovery"
            )
        elif probe_error := _probe_rust_cli(discovery_path):
            result.info["Rust Discovery CLI 状态"] = (
                "无法启动，将回退 Python discovery"
            )
            result.warnings.append(
                f"Rust Discovery CLI 无法启动: {discovery_path} ({probe_error})；"
                "将回退 Python discovery"
            )
        else:
            result.info["Rust Discovery CLI 状态"] = "可启动"

    office_backend = str(
        scanner_config.get("office_parser_backend", "rust_office_oxide_v1")
    ).strip()
    if office_backend == "rust_office_oxide_v1":
        office_path = resolve_binary_path(
            scanner_config.get(
                "rust_office_parser_bin",
                "rust/target/release/ai-daily-office-parser",
            ),
            project_root=project_root,
            system_name=platform.system(),
        )
        result.info["Rust Office Parser CLI"] = str(office_path)
        office_exists = office_path.is_file()
        office_probe_error = None if not office_exists else _probe_rust_cli(office_path)
        if office_exists and office_probe_error is None:
            result.info["Rust Office Parser CLI 状态"] = "可启动"
        else:
            issue = "不存在" if not office_exists else "无法启动"
            status = "缺失" if not office_exists else "无法启动"
            probe_detail = f" ({office_probe_error})" if office_probe_error else ""
            if _has_office_fallback(scanner_config):
                result.info["Rust Office Parser CLI 状态"] = (
                    f"{status}，将回退 Python Office parser"
                )
                result.warnings.append(
                    f"Rust Office Parser CLI {issue}: {office_path}"
                    f"{probe_detail}；将按配置回退 Python Office parser"
                )
            else:
                result.info["Rust Office Parser CLI 状态"] = (
                    f"{status}，无可用 fallback"
                )
                result.errors.append(
                    f"Rust Office Parser CLI {issue}: {office_path}"
                    f"{probe_detail}；无可用 fallback"
                )


def _has_office_fallback(scanner_config: Any) -> bool:
    """配置中至少有一个已支持的 Python Office fallback 才视为可降级。"""

    if not bool(scanner_config.get("office_parser_fallback_enabled", True)):
        return False
    order = scanner_config.get(
        "office_parser_fallback_order",
        ["python_office_v1", "python_sharepoint_text_v1"],
    )
    if isinstance(order, str):
        order = [order]
    try:
        return any(
            str(backend) in {"python_office_v1", "python_sharepoint_text_v1"}
            for backend in order
        )
    except TypeError:
        return False


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

    _append_project_file_checks(result, root, strict=strict)

    try:
        _append_runtime_config_checks(result, cfg, root, strict=strict)
    except Exception as exc:
        # YAML 解析异常可能携带出错原文；这里只保留类型，避免回显密钥。
        result.errors.append(
            f"配置加载失败 ({type(exc).__name__})，请检查本机配置格式"
        )

    _append_dependency_checks(result)
    return result
