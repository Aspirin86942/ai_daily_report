"""doctor 子命令：环境检查与轻量 bootstrap 诊断。"""

from __future__ import annotations

from collections.abc import Callable, Mapping, Sequence
from typing import Protocol


class _Console(Protocol):
    def print(self, *objects: object, **kwargs: object) -> None:
        ...


class _HealthResult(Protocol):
    info: Mapping[str, object]
    warnings: Sequence[str]
    errors: Sequence[str]


def run_doctor_cmd(
    *,
    console: _Console,
    collect: Callable[..., _HealthResult],
    strict: bool = False,
) -> bool:
    """检查运行环境和配置。"""
    console.print("\n[bold green]===== 环境检查 =====[/bold green]\n")
    result = collect(strict=strict)

    if result.info:
        console.print("[bold cyan]配置概览[/bold cyan]")
        for label, value in result.info.items():
            console.print(f"  - {label}: {value}")
    if result.warnings:
        console.print("\n[yellow]警告:[/yellow]")
        for message in result.warnings:
            console.print(f"  [!] {message}")
    if result.errors:
        console.print("\n[red]错误:[/red]")
        for message in result.errors:
            console.print(f"  [X] {message}")
        console.print("\n[red]环境检查失败，请修复上述问题[/red]")
        return False

    console.print("\n[green]所有检查通过，可以正常使用[/green]")
    return True


def run_bootstrap_doctor(*, strict: bool = False) -> int:
    """在 Rich 和业务依赖导入前执行轻量 doctor。"""
    print("\n===== 环境检查 =====\n")
    try:
        from src.core.healthcheck import collect_healthcheck as collect

        result = collect(strict=True) if strict else collect()
    except ModuleNotFoundError as exc:
        missing_module = exc.name or "未知模块"
        print("错误:")
        print(f"  [X] doctor 启动依赖缺失: {missing_module}")
        print("\n请先 uv sync 后重试")
        return 1
    except Exception as exc:
        print("错误:")
        print(
            f"  [X] doctor 无法启动 ({type(exc).__name__})；"
            "请检查依赖与本机配置格式"
        )
        return 1

    if result.info:
        print("配置概览")
        for label, value in result.info.items():
            print(f"  - {label}: {value}")
    if result.warnings:
        print("\n警告:")
        for message in result.warnings:
            print(f"  [!] {message}")
    if result.errors:
        print("\n错误:")
        for message in result.errors:
            print(f"  [X] {message}")
        print("\n环境检查失败，请修复上述问题")
        return 1

    print("\n所有检查通过，可以正常使用")
    return 0
