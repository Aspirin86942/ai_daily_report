"""argparse 子命令定义。"""

from __future__ import annotations

import argparse


def build_parser() -> argparse.ArgumentParser:
    """构建子命令 CLI 解析器。"""
    parser = argparse.ArgumentParser(description="审计日报生成器 v5.0")
    subparsers = parser.add_subparsers(dest="subcommand", help="子命令")

    daily_parser = subparsers.add_parser("daily", help="生成日报")
    daily_parser.add_argument("-i", "--input", type=str, help="主动发送给模型的用户陈述；本地附注请写入日期附注文件")
    daily_parser.add_argument("--no-save", action="store_true", help="模型预览；保留调用预算和恢复审计，不发布日报")
    dates = daily_parser.add_mutually_exclusive_group()
    dates.add_argument("--target-date", metavar="YYYY-MM-DD", help="目标自然日（默认昨日）")
    dates.add_argument("--date", metavar="YYYY-MM-DD", help="--target-date 的弃用别名")
    dates.add_argument("--scheduled", action="store_true", help="处理最近已到交付时间的日期")
    daily_parser.add_argument("--timezone", choices=["Asia/Shanghai"], help="业务时区")
    daily_parser.add_argument("--dry-run", action="store_true", help="本地扫描预检，保存清单，模型调用为零")
    daily_parser.add_argument("--regenerate", action="store_true", help="显式重新调用模型生成；必须同时填写 --reason")
    daily_parser.add_argument("--reason", help="强制重新生成的原因")

    repair_parser = subparsers.add_parser("repair-output", help="重放已存日报正文修复输出，零扫描、零模型调用")
    repair_parser.add_argument("--target-date", required=True, metavar="YYYY-MM-DD")
    runs_parser = subparsers.add_parser("runs", help="查看运行阶段、质量、调用次数及错误码")
    runs_parser.add_argument("--target-date", metavar="YYYY-MM-DD")
    runs_parser.add_argument("--limit", type=int, default=50)

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
    doctor_parser.add_argument("--check-model", action="store_true", help="向已配置网关发送合成文本检查，计入调用预算")

    return parser
