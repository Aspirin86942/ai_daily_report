"""weekly 子命令。"""

from __future__ import annotations

from argparse import Namespace
from datetime import date

from src.services.report_runner.requests import WeeklyReportRunRequest

from .common import (
    ConsolePort,
    MarkdownFactory,
    RunnerFactory,
    build_default_report_runner,
    present_report_outcome,
)


def generate_weekly_report_cmd(
    args: Namespace,
    *,
    console: ConsolePort,
    runner_factory: RunnerFactory | None = None,
    markdown: MarkdownFactory | None = None,
) -> bool:
    """映射 weekly 参数并展示 ReportRunner outcome。"""
    if markdown is None:
        from rich.markdown import Markdown

        markdown = Markdown
    runner = runner_factory() if runner_factory else build_default_report_runner(console)
    outcome = runner.run(
        WeeklyReportRunRequest(
            as_of_date=date.today(),
            source=args.source,
            save=not args.no_save,
            week_label=args.week,
            supplemental_input=args.input,
        )
    )
    return present_report_outcome(
        outcome,
        label="周报",
        console=console,
        markdown=markdown,
    )
