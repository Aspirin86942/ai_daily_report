"""审计日报生成器主程序 v5.0 — 支持日报/周报/月报"""

import argparse
import sys
from datetime import date
from rich.console import Console
from rich.markdown import Markdown
from rich.progress import Progress, SpinnerColumn, TextColumn

from src.core.logger import setup_logger
from src.core.llm import LLMClient
from src.models.schemas import ScanResult
from src.services.file_scanner import FileScanner
from src.services.report_gen import ReportGenerator
from src.services.sqlite_store import SQLiteStore
from src.utils.text_tools import parse_week_label, get_month_date_range

# 修复 Windows 控制台编码问题
if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")

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


def generate_daily_report(args: argparse.Namespace) -> None:
    """生成日报"""
    console.print("\n[bold green]===== 审计日报生成器 v5.0 =====[/bold green]\n")

    # 初始化服务
    scanner = FileScanner()
    store = SQLiteStore()
    report_gen = ReportGenerator()
    llm_client = LLMClient()

    # 1. 扫描文件
    with Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        console=console,
    ) as progress:
        task = progress.add_task("扫描今日修改的文件...", total=None)
        scan_result = scanner.scan_today_files()
        progress.update(task, completed=True)

    console.print(
        f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
    )

    # 构建文件上下文
    file_context = build_file_context(scan_result)

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
        return

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
            return

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


def generate_weekly_report_cmd(args: argparse.Namespace) -> None:
    """生成周报"""
    # 解析周标签
    if args.week:
        try:
            year, week_num = parse_week_label(args.week)
        except ValueError as e:
            console.print(f"[red]错误: {e}[/red]")
            return
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
                return
            console.print(
                f"[green]✓[/green] 读取 {len(reports)} 份日报, {len(missing_days)} 天缺失\n"
            )

        case "scan":
            # 扫描文件
            scanner = FileScanner()
            with Progress(
                SpinnerColumn(),
                TextColumn("[progress.description]{task.description}"),
                console=console,
            ) as progress:
                task = progress.add_task(
                    f"扫描 {monday} ~ {sunday} 文件...", total=None
                )
                scan_result = scanner.scan_files(monday, sunday, summary_mode=True)
                progress.update(task, completed=True)

            console.print(
                f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
            )
            file_context = build_file_context(scan_result)

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
            return

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


def generate_monthly_report_cmd(args: argparse.Namespace) -> None:
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
        return

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
                return
            console.print(
                f"[green]✓[/green] 读取 {len(reports)} 份日报, {len(missing_days)} 天缺失\n"
            )

        case "scan":
            scanner = FileScanner()
            with Progress(
                SpinnerColumn(),
                TextColumn("[progress.description]{task.description}"),
                console=console,
            ) as progress:
                task = progress.add_task(
                    f"扫描 {start_date} ~ {end_date} 文件...", total=None
                )
                scan_result = scanner.scan_files(
                    start_date, end_date, summary_mode=True
                )
                progress.update(task, completed=True)

            console.print(
                f"[green]✓[/green] 扫描完成: {scan_result.success_count}/{scan_result.total_files} 个文件\n"
            )
            file_context = build_file_context(scan_result)

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
            return

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


def main() -> None:
    """主函数"""
    parser = build_parser()
    args = parser.parse_args()

    if not args.subcommand:
        parser.print_help()
        return

    try:
        match args.subcommand:
            case "daily":
                generate_daily_report(args)
            case "weekly":
                generate_weekly_report_cmd(args)
            case "monthly":
                generate_monthly_report_cmd(args)
            case "list":
                list_reports()

    except KeyboardInterrupt:
        console.print("\n[yellow]操作已取消[/yellow]")
    except Exception as e:
        logger.error(f"程序异常: {e}", exc_info=True)
        console.print(f"\n[red]错误: {e}[/red]")


if __name__ == "__main__":
    main()
