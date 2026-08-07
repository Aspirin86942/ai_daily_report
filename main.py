"""审计日报生成器主程序 v5.0 — 支持日报/周报/月报"""

import argparse
import sys
from datetime import date, timedelta

# doctor 必须能在 Rich、业务依赖或文件日志不可用时先给出诊断。
if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")


def _run_bootstrap_doctor(*, strict: bool = False) -> int:
    """通过轻量导入运行 doctor，避免完整业务栈遮蔽部署错误。"""

    print("\n===== 环境检查 =====\n")
    try:
        from src.core.healthcheck import collect_healthcheck as collect

        result = collect(strict=True) if strict else collect()
    except ModuleNotFoundError as exc:
        missing_module = exc.name or "未知模块"
        print("错误:")
        print(f"  [X] doctor 启动依赖缺失: {missing_module}")
        print("\n请先安装 requirements.lock（或 requirements.txt）后重试")
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


if (
    __name__ == "__main__"
    and 2 <= len(sys.argv) <= 3
    and sys.argv[1] in {"doctor", "check-config"}
    and sys.argv[2:] in ([], ["--strict"])
):
    raise SystemExit(
        _run_bootstrap_doctor(strict="--strict" in sys.argv[2:])
    )

from rich.console import Console
from rich.markdown import Markdown
from rich.progress import Progress, SpinnerColumn, TextColumn

from src.core.healthcheck import collect_healthcheck
from src.core.logger import setup_logger
from src.core.llm import LLMClient
from src.services.context_scheduler import (
    ContextBuildResult,
    ContextScheduleRequest,
    ContextScheduler,
)
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
from src.utils.text_tools import parse_week_label, get_month_date_range

logger = setup_logger()
console = Console()


def build_parser() -> argparse.ArgumentParser:
    """构建子命令 CLI 解析器"""
    parser = argparse.ArgumentParser(description="审计日报生成器 v5.0")
    subparsers = parser.add_subparsers(dest="subcommand", help="子命令")

    # daily 子命令
    daily_parser = subparsers.add_parser("daily", help="生成日报")
    daily_parser.add_argument("-i", "--input", type=str, help="直接指定工作内容")
    daily_parser.add_argument("--no-save", action="store_true", help="不保存 (仅预览)")
    daily_parser.add_argument(
        "--date", type=str, metavar="YYYY-MM-DD", help="指定日期 (默认今日)"
    )

    # weekly 子命令
    weekly_parser = subparsers.add_parser("weekly", help="生成周报")
    weekly_parser.add_argument(
        "week", nargs="?", type=str, metavar="YYYY-Wnn", help="ISO 周标签 (默认本周)"
    )
    weekly_parser.add_argument(
        "--source", type=str, required=True, choices=["db", "scan"], help="数据来源"
    )
    weekly_parser.add_argument("-i", "--input", type=str, help="补充说明")
    weekly_parser.add_argument("--no-save", action="store_true", help="不保存 (仅预览)")

    # monthly 子命令
    monthly_parser = subparsers.add_parser("monthly", help="生成月报")
    monthly_parser.add_argument(
        "month", nargs="?", type=str, metavar="YYYY-MM", help="年月 (默认本月)"
    )
    monthly_parser.add_argument(
        "--source", type=str, required=True, choices=["db", "scan"], help="数据来源"
    )
    monthly_parser.add_argument("-i", "--input", type=str, help="补充说明")
    monthly_parser.add_argument(
        "--no-save", action="store_true", help="不保存 (仅预览)"
    )

    # list 子命令
    subparsers.add_parser("list", help="列出已有日报")

    # doctor 子命令
    doctor_parser = subparsers.add_parser(
        "doctor",
        aliases=["check-config"],
        help="检查运行环境和配置",
    )
    doctor_parser.add_argument(
        "--strict",
        action="store_true",
        help="按 Windows Rust 生产部署要求执行严格检查",
    )
    doctor_parser.set_defaults(subcommand="doctor")

    return parser


def _accept_context_result(context_result: ContextBuildResult) -> bool:
    """显示 scanner 状态；error 必须在构造 LLM client 前终止。"""
    summary = context_result.summary
    console.print(
        "[green]✓[/green] 扫描完成: "
        f"{summary.success_count}/{summary.source_file_count} 个文件\n"
    )
    if context_result.status == "error":
        message = (
            context_result.error.message
            if context_result.error is not None
            else "context engine returned an invalid error result"
        )
        console.print(f"[red]✗ 文件上下文构建失败: {message}[/red]\n")
        logger.error("文件上下文构建失败: %s", message)
        return False
    if context_result.status == "partial":
        for warning in context_result.warnings:
            console.print(f"[yellow]![/yellow] 文件上下文不完整: {warning.message}\n")
            logger.warning("文件上下文不完整: %s", warning.message)
    return True


def get_user_input() -> str:
    """获取用户交互输入"""
    console.print("\n[bold cyan]请描述今日工作内容:[/bold cyan]")
    console.print(
        "[dim](输入完成后按 Ctrl+Z (Windows) 或 Ctrl+D (Linux/Mac) 结束)[/dim]\n"
    )

    lines = []
    try:
        while True:
            line = input()
            lines.append(line)
    except EOFError:
        pass

    return "\n".join(lines).strip()


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


def list_reports() -> None:
    """列出已有日报"""
    console.print("\n[bold green]===== 已有日报列表 =====[/bold green]\n")

    store = SQLiteStore()
    dates = store.list_all_reports()

    if not dates:
        console.print("[yellow]暂无日报数据[/yellow]")
        return

    # 按月分组
    from collections import defaultdict

    by_month: dict[str, list[str]] = defaultdict(list)
    for date_str in dates:
        month = date_str[:7]
        by_month[month].append(date_str)

    for month in sorted(by_month.keys(), reverse=True):
        console.print(f"[bold cyan]{month}[/bold cyan]")
        for date_str in sorted(by_month[month], reverse=True):
            console.print(f"  - {date_str}")
        console.print()

    console.print(f"[green]共 {len(dates)} 份日报[/green]")


def run_doctor_cmd(*, strict: bool = False) -> bool:
    """检查运行环境和配置。"""
    console.print("\n[bold green]===== 环境检查 =====[/bold green]\n")

    result = collect_healthcheck(strict=strict)

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
                list_reports()
                return 0
            case "doctor":
                return 0 if run_doctor_cmd(strict=args.strict) else 1

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
