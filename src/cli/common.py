"""CLI 共享装配与 ReportRunOutcome 呈现。"""

from __future__ import annotations

from collections.abc import Callable
from typing import Protocol

from src.core.logger import setup_logger
from src.services.report_runner.outcomes import (
    DatabaseEvidence,
    ErrorCode,
    ReportRunFailure,
    ReportRunOutcome,
    ScanEvidence,
)
from src.services.report_runner.runner import ReportRunner

logger = setup_logger()


class ConsolePort(Protocol):
    def print(self, *objects: object, **kwargs: object) -> None:
        ...


MarkdownFactory = Callable[[str], object]
RunnerFactory = Callable[[], ReportRunner]


def build_default_report_runner(console: ConsolePort) -> ReportRunner:
    """按需装配 production ReportRunner 依赖。"""
    from src.core.llm import LLMClient
    from src.core.config import config
    from src.services.native_scanner import NativeScanner
    from src.services.report_gen import ReportGenerator
    from src.services.report_runner.input_adapter import ConsoleDailyInputAdapter
    from src.services.report_runner.model_port import LLMModelPort
    from src.services.sqlite_store import SQLiteStore

    return ReportRunner(
        scanner=NativeScanner(config),
        store=SQLiteStore(),
        renderer=ReportGenerator(),
        model_port=LLMModelPort(client_factory=LLMClient),
        daily_input=ConsoleDailyInputAdapter(console=console),
    )


def present_report_outcome(
    outcome: ReportRunOutcome,
    *,
    label: str,
    console: ConsolePort,
    markdown: MarkdownFactory,
) -> bool:
    """把 typed outcome 映射为既有提示、预览与 bool 退出语义。"""
    period = outcome.period
    if period is not None and label in {"周报", "月报"}:
        console.print(
            f"\n[bold green]===== 生成{label} {period.display_label} "
            f"({period.start_date} ~ {period.end_date}) =====[/bold green]\n"
        )

    evidence = outcome.source_evidence
    if isinstance(evidence, ScanEvidence):
        console.print(
            "[green]✓[/green] 扫描完成: "
            f"{evidence.success_count}/{evidence.source_file_count} 个文件\n"
        )
    elif isinstance(evidence, DatabaseEvidence):
        console.print(
            f"[green]✓[/green] 读取 {evidence.report_count} 份日报, "
            f"{len(evidence.missing_days)} 天缺失\n"
        )

    for warning in outcome.warnings:
        console.print(f"[yellow]![/yellow] 文件上下文不完整: {warning.message}\n")
        logger.warning("文件上下文不完整: %s", warning.message)

    if isinstance(outcome, ReportRunFailure):
        message = outcome.error.message
        if outcome.error.error_code is ErrorCode.SCANNER_FAILED:
            console.print(f"[red]✗ 文件上下文构建失败: {message}[/red]\n")
            logger.error("文件上下文构建失败: %s", message)
        elif outcome.phase == "generation":
            console.print(f"[red]✗ 生成失败: {message}[/red]")
        else:
            console.print(f"[red]错误: {message}[/red]")
        return False

    console.print(f"[green]✓[/green] {label}生成成功\n")
    if outcome.publication.requested:
        console.print(f"[green]✓[/green] {label}已保存\n")
    console.print(f"[bold cyan]===== {label}预览 =====[/bold cyan]\n")
    console.print(markdown(outcome.markdown))
    return True
