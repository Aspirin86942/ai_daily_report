"""审计日报生成器主程序 v5.0 — 支持日报/周报/月报"""

import argparse
import sys
from datetime import date, timedelta

# doctor 必须能在 Rich、业务依赖或文件日志不可用时先给出诊断。
if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")


def _run_bootstrap_doctor() -> int:
    """通过轻量导入运行 doctor，避免完整业务栈遮蔽部署错误。"""

    print("\n===== 环境检查 =====\n")
    try:
        from src.core.healthcheck import collect_healthcheck as collect

        result = collect()
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
    and len(sys.argv) == 2
    and sys.argv[1] in {"doctor", "check-config"}
):
    raise SystemExit(_run_bootstrap_doctor())

from rich.console import Console
from rich.markdown import Markdown
from rich.progress import Progress, SpinnerColumn, TextColumn

from src.core.healthcheck import collect_healthcheck
from src.core.logger import setup_logger
from src.core.llm import LLMClient
from src.models.schemas import ScanResult
from src.services.context_scheduler import (
    ContextScheduleRequest,
    ContextScheduleResult,
    ContextScheduler,
)
from src.services.report_gen import ReportGenerator
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
    doctor_parser.set_defaults(subcommand="doctor")

    return parser


def build_file_context(scan_result: ScanResult) -> str:
    """从扫描结果构建文件上下文文本

    Args:
        scan_result: 文件扫描结果

    Returns:
        拼接后的文件上下文字符串
    """
    parts: list[str] = []
    for ctx in scan_result.contexts:
        if ctx.error:
            parts.append(f"文件: {ctx.file_path}\n错误: {ctx.error}")
        else:
            parts.append(f"文件: {ctx.file_path}\n{ctx.content}")

    return "\n\n---\n\n".join(parts) if parts else "无文件证据"


def _print_context_scheduler_warning(context_result: ContextScheduleResult) -> None:
    """打印上下文调度降级警告。"""
    if not context_result.error:
        return

    # 调度器 fallback 仍允许报表继续生成，但必须让操作者看到上下文不完整。
    console.print(f"[yellow]![/yellow] 文件上下文构建降级: {context_result.error}\n")
    logger.warning("文件上下文构建降级: %s", context_result.error)


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


def generate_daily_report(args: argparse.Namespace) -> bool:
    """生成日报"""
    console.print("\n[bold green]===== 审计日报生成器 v5.0 =====[/bold green]\n")

    # 初始化服务
    scheduler = ContextScheduler()
    store = SQLiteStore()
    report_gen = ReportGenerator()
    llm_client = LLMClient()

    # 1. 构建文件上下文
    with Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        console=console,
    ) as progress:
        task = progress.add_task("构建今日文件上下文...", total=None)
        today = date.today()
        # scan 路径统一交给调度器，避免 CLI 继续散落 scanner/parser 细节。
        context_result = scheduler.build_context(
            ContextScheduleRequest(
                report_mode="daily",
                source="scan",
                start_date=today - timedelta(days=1),
                end_date=today,
            )
        )
        progress.update(task, completed=True)

    _print_context_scheduler_warning(context_result)
    if context_result.scan_result is not None:
        scan_result = context_result.scan_result
        console.print(
            f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
        )

    file_context = context_result.file_context

    # 2. 读取昨日计划
    yesterday_plan = store.get_yesterday_plan()
    if yesterday_plan:
        console.print("[green]✓[/green] 已读取昨日计划参考\n")
    else:
        console.print("[yellow]![/yellow] 无昨日计划参考\n")

    # 3. 获取用户输入
    if args.input:
        user_input = args.input
        console.print("[green]✓[/green] 使用命令行输入\n")
    else:
        user_input = get_user_input()

    if not user_input:
        console.print("[red]错误: 未输入工作内容[/red]")
        return False

    # 4. 生成日报
    with Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        console=console,
    ) as progress:
        task = progress.add_task("正在生成日报...", total=None)
        try:
            report_data = llm_client.generate_report(
                user_input=user_input,
                file_context=file_context,
                yesterday_plan=yesterday_plan,
            )
            progress.update(task, completed=True)
        except Exception as e:
            progress.update(task, completed=True)
            console.print(f"[red]✗ 生成失败: {e}[/red]")
            return False

    # 覆盖日期 (支持 --date 参数)
    if args.date:
        report_data.date = args.date

    console.print("[green]✓[/green] 日报生成成功\n")

    # 5. 渲染 Markdown
    markdown_content = report_gen.render_markdown(report_data)

    # 6. 保存
    if not args.no_save:
        store.save_report(report_data)
        report_gen.save_markdown(markdown_content, report_data.date)
        console.print("[green]✓[/green] 日报已保存\n")

    # 7. 预览
    console.print("[bold cyan]===== 日报预览 =====[/bold cyan]\n")
    console.print(Markdown(markdown_content))
    return True


def generate_weekly_report_cmd(args: argparse.Namespace) -> bool:
    """生成周报"""
    # 解析周标签
    if args.week:
        try:
            year, week_num = parse_week_label(args.week)
        except ValueError as e:
            console.print(f"[red]错误: {e}[/red]")
            return False
    else:
        # 默认本周
        today = date.today()
        year, week_num, _ = today.isocalendar()

    week_label = f"{year}-W{week_num:02d}"
    monday = date.fromisocalendar(year, week_num, 1)
    sunday = date.fromisocalendar(year, week_num, 7)

    console.print(
        f"\n[bold green]===== 生成周报 {week_label} ({monday} ~ {sunday}) =====[/bold green]\n"
    )

    # 初始化服务
    store = SQLiteStore()
    report_gen = ReportGenerator()
    llm_client = LLMClient()

    reports = []
    missing_days: list[str] = []
    file_context = "无文件证据"

    match args.source:
        case "db":
            # 从数据库聚合日报
            reports, missing_days = store.get_week_reports(year, week_num)
            if not reports:
                console.print(f"[red]错误: 未找到 {week_label} 的日报数据[/red]")
                return False
            console.print(
                f"[green]✓[/green] 读取 {len(reports)} 份日报, {len(missing_days)} 天缺失\n"
            )

        case "scan":
            # scan 路径统一交给调度器，周报 CLI 只负责传入周期边界。
            with Progress(
                SpinnerColumn(),
                TextColumn("[progress.description]{task.description}"),
                console=console,
            ) as progress:
                task = progress.add_task(
                    f"构建 {monday} ~ {sunday} 文件上下文...", total=None
                )
                context_result = ContextScheduler().build_context(
                    ContextScheduleRequest(
                        report_mode="weekly",
                        source="scan",
                        start_date=monday,
                        end_date=sunday,
                    )
                )
                progress.update(task, completed=True)

            _print_context_scheduler_warning(context_result)
            if context_result.scan_result is not None:
                scan_result = context_result.scan_result
                console.print(
                    f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
                )
            file_context = context_result.file_context

    # 用户补充输入
    if args.input:
        file_context += f"\n\n---\n\n用户补充: {args.input}"

    # 生成周报
    with Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        console=console,
    ) as progress:
        task = progress.add_task("正在生成周报...", total=None)
        try:
            report_data = llm_client.generate_weekly_report(
                reports=reports,
                file_context=file_context,
                year=year,
                week=week_num,
                missing_days=missing_days,
                data_source=args.source,
            )
            progress.update(task, completed=True)
        except Exception as e:
            progress.update(task, completed=True)
            console.print(f"[red]✗ 生成失败: {e}[/red]")
            return False

    console.print("[green]✓[/green] 周报生成成功\n")

    # 渲染 Markdown
    markdown_content = report_gen.render_weekly_markdown(report_data)

    # 保存
    if not args.no_save:
        store.save_weekly_report(report_data)
        report_gen.save_weekly_markdown(markdown_content, year, week_num)
        console.print("[green]✓[/green] 周报已保存\n")

    # 预览
    console.print("[bold cyan]===== 周报预览 =====[/bold cyan]\n")
    console.print(Markdown(markdown_content))
    return True


def generate_monthly_report_cmd(args: argparse.Namespace) -> bool:
    """生成月报"""
    # 解析年月
    if args.month:
        year_month = args.month
    else:
        year_month = date.today().strftime("%Y-%m")

    try:
        start_date, end_date = get_month_date_range(year_month)
    except ValueError as e:
        console.print(f"[red]错误: {e}[/red]")
        return False

    console.print(
        f"\n[bold green]===== 生成月报 {year_month} ({start_date} ~ {end_date}) =====[/bold green]\n"
    )

    # 初始化服务
    store = SQLiteStore()
    report_gen = ReportGenerator()
    llm_client = LLMClient()

    reports = []
    missing_days: list[str] = []
    file_context = "无文件证据"

    match args.source:
        case "db":
            reports, missing_days = store.get_reports_in_range(
                start_date, end_date
            )
            if not reports:
                console.print(f"[red]错误: 未找到 {year_month} 的日报数据[/red]")
                return False
            console.print(
                f"[green]✓[/green] 读取 {len(reports)} 份日报, {len(missing_days)} 天缺失\n"
            )

        case "scan":
            # scan 路径统一交给调度器，月报 CLI 只负责传入月份边界。
            with Progress(
                SpinnerColumn(),
                TextColumn("[progress.description]{task.description}"),
                console=console,
            ) as progress:
                task = progress.add_task(
                    f"构建 {start_date} ~ {end_date} 文件上下文...", total=None
                )
                context_result = ContextScheduler().build_context(
                    ContextScheduleRequest(
                        report_mode="monthly",
                        source="scan",
                        start_date=start_date,
                        end_date=end_date,
                    )
                )
                progress.update(task, completed=True)

            _print_context_scheduler_warning(context_result)
            if context_result.scan_result is not None:
                scan_result = context_result.scan_result
                console.print(
                    f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
                )
            file_context = context_result.file_context

    # 用户补充输入
    if args.input:
        file_context += f"\n\n---\n\n用户补充: {args.input}"

    # 生成月报
    with Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        console=console,
    ) as progress:
        task = progress.add_task("正在生成月报...", total=None)
        try:
            report_data = llm_client.generate_monthly_report(
                reports=reports,
                file_context=file_context,
                year_month=year_month,
                missing_days=missing_days,
                data_source=args.source,
            )
            progress.update(task, completed=True)
        except Exception as e:
            progress.update(task, completed=True)
            console.print(f"[red]✗ 生成失败: {e}[/red]")
            return False

    console.print("[green]✓[/green] 月报生成成功\n")

    # 渲染 Markdown
    markdown_content = report_gen.render_monthly_markdown(report_data)

    # 保存
    if not args.no_save:
        store.save_monthly_report(report_data)
        report_gen.save_monthly_markdown(markdown_content, year_month)
        console.print("[green]✓[/green] 月报已保存\n")

    # 预览
    console.print("[bold cyan]===== 月报预览 =====[/bold cyan]\n")
    console.print(Markdown(markdown_content))
    return True


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


def run_doctor_cmd() -> bool:
    """检查运行环境和配置。"""
    console.print("\n[bold green]===== 环境检查 =====[/bold green]\n")

    result = collect_healthcheck()

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
                return 0 if run_doctor_cmd() else 1

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
