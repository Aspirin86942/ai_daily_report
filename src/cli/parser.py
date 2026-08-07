"""argparse 子命令定义。"""

from __future__ import annotations

import argparse


def build_parser() -> argparse.ArgumentParser:
    """构建子命令 CLI 解析器。"""
    parser = argparse.ArgumentParser(description="审计日报生成器 v5.0")
    subparsers = parser.add_subparsers(dest="subcommand", help="子命令")

    daily_parser = subparsers.add_parser("daily", help="生成日报")
    daily_parser.add_argument("-i", "--input", type=str, help="直接指定工作内容")
    daily_parser.add_argument("--no-save", action="store_true", help="不保存 (仅预览)")
    daily_parser.add_argument(
        "--date", type=str, metavar="YYYY-MM-DD", help="指定日期 (默认今日)"
    )

    weekly_parser = subparsers.add_parser("weekly", help="生成周报")
    weekly_parser.add_argument(
        "week", nargs="?", type=str, metavar="YYYY-Wnn", help="ISO 周标签 (默认本周)"
    )
    weekly_parser.add_argument(
        "--source", type=str, required=True, choices=["db", "scan"], help="数据来源"
    )
    weekly_parser.add_argument("-i", "--input", type=str, help="补充说明")
    weekly_parser.add_argument("--no-save", action="store_true", help="不保存 (仅预览)")

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

    subparsers.add_parser("list", help="列出已有日报")

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
