"""daily 子命令。"""

from __future__ import annotations

from argparse import Namespace
from datetime import date

from src.services.report_runner.requests import DailyReportRunRequest

from .common import (
    ConsolePort,
    MarkdownFactory,
    RunnerFactory,
    build_default_report_runner,
    present_report_outcome,
)


def generate_daily_report(
    args: Namespace,
    *,
    console: ConsolePort,
    runner_factory: RunnerFactory | None = None,
    markdown: MarkdownFactory | None = None,
) -> bool:
    """映射 daily 参数并展示 ReportRunner outcome。"""
    console.print("\n[bold green]===== 审计日报生成器 v5.0 =====[/bold green]\n")
    if markdown is None:
        from rich.markdown import Markdown

        markdown = Markdown
    runner = runner_factory() if runner_factory else build_default_report_runner(console)
    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date.today(),
            save=not args.no_save,
            user_input=args.input,
            report_date_override=args.date,
        )
    )
    return present_report_outcome(
        outcome,
        label="日报",
        console=console,
        markdown=markdown,
    )
