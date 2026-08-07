"""审计日报生成器主程序 v5.0 — 支持日报/周报/月报"""

import sys

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

from src.cli.doctor import run_doctor_cmd
from src.cli.daily import generate_daily_report
from src.cli.list_reports import list_reports
from src.cli.monthly import generate_monthly_report_cmd
from src.cli.parser import build_parser
from src.cli.weekly import generate_weekly_report_cmd
from src.core.healthcheck import collect_healthcheck
from src.core.logger import setup_logger
from src.services.sqlite_store import SQLiteStore

logger = setup_logger()
console = Console()


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
                return 0 if generate_daily_report(args, console=console) else 1
            case "weekly":
                return 0 if generate_weekly_report_cmd(args, console=console) else 1
            case "monthly":
                return 0 if generate_monthly_report_cmd(args, console=console) else 1
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
