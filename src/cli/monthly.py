"""monthly 子命令。"""

from __future__ import annotations

from argparse import Namespace
from datetime import date

from src.services.report_runner.requests import MonthlyReportRunRequest

from .common import (
    ConsolePort,
    MarkdownFactory,
    RunnerFactory,
    build_report_dependencies,
    present_report_outcome,
)


def generate_monthly_report_cmd(
    args: Namespace,
    *,
    console: ConsolePort,
    runner_factory: RunnerFactory | None = None,
    markdown: MarkdownFactory | None = None,
) -> bool:
    """映射 monthly 参数并展示 ReportRunner outcome。"""
    runner, markdown = build_report_dependencies(console, runner_factory, markdown)
    outcome = runner.run(
        MonthlyReportRunRequest(
            as_of_date=date.today(),
            source=args.source,
            save=not args.no_save,
            year_month=args.month,
            supplemental_input=args.input,
        )
    )
    return present_report_outcome(
        outcome,
        label="月报",
        console=console,
        markdown=markdown,
    )
