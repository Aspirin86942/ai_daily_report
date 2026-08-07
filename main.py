"""审计日报生成器 v5.0 — 主入口（子命令逻辑见 src/cli/）。"""

import sys

# doctor 必须能在 Rich、业务依赖或文件日志不可用时先给出诊断。
if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")

from src.cli.doctor import run_bootstrap_doctor


def main() -> int:
    """解析参数并把子命令分派到对应 CLI adapter。"""
    from src.cli.parser import build_parser

    parser = build_parser()
    args = parser.parse_args()

    if not args.subcommand:
        parser.print_help()
        return 0

    from rich.console import Console

    from src.core.logger import setup_logger

    console = Console()
    logger = setup_logger()
    try:
        match args.subcommand:
            case "daily":
                from src.cli.daily import generate_daily_report

                return 0 if generate_daily_report(args, console=console) else 1
            case "weekly":
                from src.cli.weekly import generate_weekly_report_cmd

                return 0 if generate_weekly_report_cmd(args, console=console) else 1
            case "monthly":
                from src.cli.monthly import generate_monthly_report_cmd

                return 0 if generate_monthly_report_cmd(args, console=console) else 1
            case "list":
                from src.cli.list_reports import list_reports
                from src.services.sqlite_store import SQLiteStore

                list_reports(console, store=SQLiteStore())
                return 0
            case "doctor":
                from src.cli.doctor import run_doctor_cmd
                from src.core.healthcheck import collect_healthcheck

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
    except Exception as exc:
        logger.error("程序异常: %s", exc, exc_info=True)
        console.print(f"\n[red]错误: {exc}[/red]")
        return 1

    return 0


if (
    __name__ == "__main__"
    and 2 <= len(sys.argv) <= 3
    and sys.argv[1] in {"doctor", "check-config"}
    and sys.argv[2:] in ([], ["--strict"])
):
    raise SystemExit(run_bootstrap_doctor(strict="--strict" in sys.argv[2:]))

if __name__ == "__main__":
    raise SystemExit(main())
