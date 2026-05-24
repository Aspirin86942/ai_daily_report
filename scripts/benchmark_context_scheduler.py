"""运行真实 ContextScheduler 链路并输出压缩证据。"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Sequence

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from src.services.context_compressor import CompressedContext  # noqa: E402
from src.services.context_scheduler import (  # noqa: E402
    ContextScheduleRequest,
    ContextScheduler,
)
from src.services.file_scanner import FileScanner  # noqa: E402


def _parse_date(value: str) -> date:
    """解析 CLI 日期参数，固定 YYYY-MM-DD 口径。"""
    return date.fromisoformat(value)


def build_context_scheduler_summary(
    compressed_context: CompressedContext,
) -> dict[str, Any]:
    """返回 ContextScheduler 压缩摘要，保留 warnings 等非数值字段。"""
    return compressed_context.to_summary()


def build_benchmark_payload(
    *,
    compressed_context: CompressedContext,
    context_run: dict[str, Any] | None,
    parameters: dict[str, Any],
    scan_run: dict[str, Any] | None,
) -> dict[str, Any]:
    """组合 benchmark 输出结构，避免 CLI 和测试各自拼字段。"""
    return {
        "parameters": parameters,
        "scan_run": scan_run or {},
        "context_run": context_run or {},
        "context_scheduler_summary": build_context_scheduler_summary(
            compressed_context
        ),
    }


def render_markdown_report(payload: dict[str, Any]) -> str:
    """把 benchmark payload 渲染成人工可读的 Markdown 报告。"""
    parameters = payload.get("parameters", {})
    scan_run = payload.get("scan_run", {})
    context_run = payload.get("context_run", {})
    context_scheduler_summary = payload.get("context_scheduler_summary", {})

    lines = [
        "# Context Scheduler Benchmark Report",
        "",
        "## Parameters",
        "",
        f"- start_date: `{parameters.get('start_date', '')}`",
        f"- end_date: `{parameters.get('end_date', '')}`",
        f"- report_mode: `{parameters.get('report_mode', '')}`",
        "- compression_profile: "
        f"`{parameters.get('compression_profile', '')}`",
        "",
        "## Scan Run",
        "",
        f"- run_id: `{scan_run.get('run_id', '')}`",
        f"- discovered_count: `{scan_run.get('discovered_count', '')}`",
        f"- reused_count: `{scan_run.get('reused_count', '')}`",
        f"- reparsed_count: `{scan_run.get('reparsed_count', '')}`",
        "",
        "## Context Run",
        "",
        f"- context_run_id: `{context_run.get('context_run_id', '')}`",
        f"- status: `{context_run.get('status', '')}`",
        "",
        "## Context Scheduler Summary",
        "",
        "| key | value |",
        "|---|---:|",
    ]
    for key in sorted(context_scheduler_summary):
        lines.append(
            f"| {key} | {_format_markdown_value(context_scheduler_summary[key])} |"
        )

    return "\n".join(lines) + "\n"


def write_report_files(
    payload: dict[str, Any],
    *,
    json_out: Path | None,
    markdown_out: Path | None,
) -> None:
    """按需写出 JSON 和 Markdown benchmark 文件。"""
    if json_out is not None:
        json_out.parent.mkdir(parents=True, exist_ok=True)
        json_out.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )

    if markdown_out is not None:
        markdown_out.parent.mkdir(parents=True, exist_ok=True)
        markdown_out.write_text(
            render_markdown_report(payload),
            encoding="utf-8",
        )


def _format_markdown_value(value: Any) -> str:
    if isinstance(value, (dict, list)):
        return "`" + json.dumps(value, ensure_ascii=False, sort_keys=True) + "`"
    return f"`{value}`"


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Benchmark ai_daily_report ContextScheduler"
    )
    default_end_date = date.today()
    default_start_date = default_end_date - timedelta(days=1)
    parser.add_argument(
        "--start-date",
        type=_parse_date,
        default=default_start_date,
        help="Start date in YYYY-MM-DD format. Default: yesterday.",
    )
    parser.add_argument(
        "--end-date",
        type=_parse_date,
        default=default_end_date,
        help="End date in YYYY-MM-DD format. Default: today.",
    )
    parser.add_argument(
        "--report-mode",
        choices=("daily", "weekly", "monthly"),
        default="weekly",
    )
    parser.add_argument("--compression-profile", default=None)
    parser.add_argument("--json-out", type=Path, default=None)
    parser.add_argument("--markdown-out", type=Path, default=None)
    return parser


def run_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    """运行真实 ContextScheduler，并读取本轮落库指标生成 payload。"""
    scheduler = ContextScheduler()
    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode=args.report_mode,
            source="scan",
            start_date=args.start_date,
            end_date=args.end_date,
            compression_profile=args.compression_profile,
        )
    )

    # benchmark 要绑定刚刚的 ContextScheduler run；这里只读取同一默认 store，
    # 避免第二次 scan 污染 scan run、cache 命中率和压缩审计指标。
    store = FileScanner().scan_index_store
    context_run = (
        None
        if result.context_run_id is None
        else store.get_context_run(result.context_run_id)
    )
    scan_run_id = None if context_run is None else context_run.get("scan_run_id")
    scan_run = (
        None
        if scan_run_id is None
        else store.get_scan_run_detail(int(scan_run_id))
    )
    compression_profile = _resolve_compression_profile(
        context_run=context_run,
        requested_profile=args.compression_profile,
        report_mode=args.report_mode,
    )
    parameters = {
        "start_date": args.start_date.isoformat(),
        "end_date": args.end_date.isoformat(),
        "report_mode": args.report_mode,
        "compression_profile": compression_profile,
    }

    return build_benchmark_payload(
        compressed_context=result.compressed_context,
        context_run=context_run,
        parameters=parameters,
        scan_run=scan_run,
    )


def _resolve_compression_profile(
    *,
    context_run: dict[str, Any] | None,
    requested_profile: str | None,
    report_mode: str,
) -> str:
    if context_run:
        profile = context_run.get("compression_profile")
        if profile:
            return str(profile)
    if requested_profile:
        return requested_profile
    return f"{report_mode}_balanced_v1"


def main(argv: Sequence[str] | None = None) -> int:
    """CLI 入口：stdout 始终输出 JSON，文件输出按参数决定。"""
    parser = _build_parser()
    args = parser.parse_args(argv)
    payload = run_benchmark(args)
    write_report_files(
        payload,
        json_out=args.json_out,
        markdown_out=args.markdown_out,
    )
    print(json.dumps(payload, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
