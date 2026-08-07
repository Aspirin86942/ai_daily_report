"""审计日报生成器主程序 v5.0 — 支持日报/周报/月报"""

import argparse
import sys
from datetime import date

# doctor 必须能在 Rich、业务依赖或文件日志不可用时先给出诊断。
if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")

from src.cli.doctor import run_bootstrap_doctor


if (
    __name__ == "__main__"
    and 2 <= len(sys.argv) <= 3
    and sys.argv[1] in {"doctor", "check-config"}
    and sys.argv[2:] in ([], ["--strict"])
):
    raise SystemExit(run_bootstrap_doctor(strict="--strict" in sys.argv[2:]))

from rich.console import Console
from rich.markdown import Markdown

from src.cli.doctor import run_doctor_cmd
from src.cli.list_reports import list_reports
from src.cli.parser import build_parser
from src.core.healthcheck import collect_healthcheck
from src.core.logger import setup_logger
from src.core.llm import LLMClient
from src.services.context_scheduler import ContextScheduler
from src.services.report_gen import ReportGenerator
from src.services.report_runner import (
    DailyReportRunRequest,
    MonthlyReportRunRequest,
    ReportRunFailure,
    ReportRunSuccess,
    ReportRunner,
    WeeklyReportRunRequest,
)
from src.services.report_runner.input_adapter import ConsoleDailyInputAdapter
from src.services.report_runner.model_port import LLMModelPort
from src.services.report_runner.outcomes import (
    DatabaseEvidence,
    ErrorCode,
    ScanEvidence,
)
from src.services.sqlite_store import SQLiteStore

logger = setup_logger()
console = Console()


def _build_report_runner() -> ReportRunner:
    """装配 production 依赖；报告命令只负责 request/outcome 映射。"""
    return ReportRunner(
        scheduler=ContextScheduler(),
        store=SQLiteStore(),
        renderer=ReportGenerator(),
        model_port=LLMModelPort(client_factory=LLMClient),
        daily_input=ConsoleDailyInputAdapter(console=console),
    )


def _present_report_outcome(
    outcome: ReportRunSuccess | ReportRunFailure,
    label: str,
) -> bool:
    """把 typed outcome 映射为既有提示、预览与 bool 退出语义。"""
    period = outcome.period
    if period is not None and label == "周报":
        console.print(
            f"\n[bold green]===== 生成周报 {period.display_label} "
            f"({period.start_date} ~ {period.end_date}) =====[/bold green]\n"
        )
    elif period is not None and label == "月报":
        console.print(
            f"\n[bold green]===== 生成月报 {period.display_label} "
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
    console.print(Markdown(outcome.markdown))
    return True


def generate_daily_report(args: argparse.Namespace) -> bool:
    """映射 daily CLI 参数并展示 ReportRunner outcome。"""
    console.print("\n[bold green]===== 审计日报生成器 v5.0 =====[/bold green]\n")
    outcome = _build_report_runner().run(
        DailyReportRunRequest(
            as_of_date=date.today(),
            save=not args.no_save,
            user_input=args.input,
            report_date_override=args.date,
        )
    )
    return _present_report_outcome(outcome, "日报")


def generate_weekly_report_cmd(args: argparse.Namespace) -> bool:
    """映射 weekly CLI 参数并展示 ReportRunner outcome。"""
    outcome = _build_report_runner().run(
        WeeklyReportRunRequest(
            as_of_date=date.today(),
            source=args.source,
            save=not args.no_save,
            week_label=args.week,
            supplemental_input=args.input,
        )
    )
    return _present_report_outcome(outcome, "周报")


def generate_monthly_report_cmd(args: argparse.Namespace) -> bool:
    """映射 monthly CLI 参数并展示 ReportRunner outcome。"""
    outcome = _build_report_runner().run(
        MonthlyReportRunRequest(
            as_of_date=date.today(),
            source=args.source,
            save=not args.no_save,
            year_month=args.month,
            supplemental_input=args.input,
        )
    )
    return _present_report_outcome(outcome, "月报")


def main() -> int:
    """主函数"""
    parser = build_parser()
    args = parser.parse_args()

    if not args.subcommand:
        parser.print_help()
        return 0

    try:
        match args.subcommand:
            case "daily":
                return 0 if generate_daily_report(args) else 1
            case "weekly":
                return 0 if generate_weekly_report_cmd(args) else 1
            case "monthly":
                return 0 if generate_monthly_report_cmd(args) else 1
            case "list":
                list_reports(console, store=SQLiteStore())
                return 0
            case "doctor":
                return (
                    0
                    if run_doctor_cmd(
                        console=console,
                        collect=collect_healthcheck,
                        strict=args.strict,
                    )
                    else 1
                )

    except KeyboardInterrupt:
        console.print("\n[yellow]操作已取消[/yellow]")
        return 130
    except Exception as e:
        logger.error(f"程序异常: {e}", exc_info=True)
        console.print(f"\n[red]错误: {e}[/red]")
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
