"""运行真实 scanner 链路并输出性能证据。"""

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

from src.models.schemas import ScanResult  # noqa: E402
from src.services.file_scanner import FileScanner  # noqa: E402
from src.services.scan_metrics import ExtensionMetrics, ReparseDetail  # noqa: E402


def build_benchmark_payload(
    scan_result: ScanResult,
    run_detail: dict[str, int],
    extension_metrics: list[ExtensionMetrics],
    reparse_details: list[ReparseDetail],
    start_date: date,
    end_date: date,
    summary_mode: bool,
) -> dict[str, Any]:
    """组合 benchmark 输出结构，避免 CLI 和测试各自拼字段。"""
    return {
        "parameters": {
            "start_date": start_date.isoformat(),
            "end_date": end_date.isoformat(),
            "summary_mode": summary_mode,
        },
        "scan_result": {
            "total_files": scan_result.total_files,
            "success_count": scan_result.success_count,
            "error_count": scan_result.error_count,
        },
        "metrics": dict(run_detail),
        "extension_metrics": [item.to_dict() for item in extension_metrics],
        "reparse_details": [item.to_dict() for item in reparse_details],
    }


def render_markdown_report(payload: dict[str, Any]) -> str:
    """把 JSON payload 渲染成便于人工 review 的 Markdown 报告。"""
    parameters = payload["parameters"]
    scan_result = payload["scan_result"]
    metrics = payload["metrics"]
    extension_metrics = payload["extension_metrics"]
    reparse_details = payload.get("reparse_details", [])

    lines = [
        "# Scanner Benchmark Report",
        "",
        "## Parameters",
        "",
        f"- start_date: `{parameters['start_date']}`",
        f"- end_date: `{parameters['end_date']}`",
        f"- summary_mode: `{parameters['summary_mode']}`",
        "",
        "## Scan Result",
        "",
        f"- total_files: `{scan_result['total_files']}`",
        f"- success_count: `{scan_result['success_count']}`",
        f"- error_count: `{scan_result['error_count']}`",
        "",
        "## Stage Durations",
        "",
        "| stage | duration_ms |",
        "|---|---:|",
        f"| total | {metrics['total_duration_ms']} |",
        f"| discovery | {metrics['discovery_duration_ms']} |",
        f"| inventory/cache | {metrics['inventory_cache_duration_ms']} |",
        f"| parse | {metrics['parse_duration_ms']} |",
        f"| aggregation | {metrics['aggregation_duration_ms']} |",
        "",
        "## Counts",
        "",
        f"- discovered_count: `{metrics['discovered_count']}`",
        f"- reused_count: `{metrics['reused_count']}`",
        f"- reparsed_count: `{metrics['reparsed_count']}`",
        f"- timeout_count: `{metrics['timeout_count']}`",
        "",
        "## Extension Metrics",
        "",
        "| extension | file_count | parse_duration_ms | success_count | error_count | timeout_count |",
        "|---|---:|---:|---:|---:|---:|",
    ]

    if extension_metrics:
        for item in extension_metrics:
            lines.append(
                "| {extension} | {file_count} | {parse_duration_ms} | "
                "{success_count} | {error_count} | {timeout_count} |".format(
                    **item
                )
            )
    else:
        lines.append("| (none) | 0 | 0 | 0 | 0 | 0 |")

    lines.extend(
        [
            "",
            "## Reparse Details",
            "",
            "| extension | cache_miss_reason | parse_duration_ms | parse_status | path |",
            "|---|---|---:|---|---|",
        ]
    )
    if reparse_details:
        for item in reparse_details:
            lines.append(
                "| {extension} | {cache_miss_reason} | {parse_duration_ms} | "
                "{parse_status} | {path} |".format(**item)
            )
    else:
        lines.append("| (none) |  | 0 |  |  |")

    return "\n".join(lines) + "\n"


def write_report_files(
    payload: dict[str, Any],
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


def _parse_date(value: str) -> date:
    """解析 CLI 日期参数，固定 YYYY-MM-DD 口径。"""
    return date.fromisoformat(value)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Benchmark ai_daily_report scanner")
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
        "--summary-mode",
        action="store_true",
        help="Use scanner summary parsing limits.",
    )
    parser.add_argument("--json-out", type=Path, default=None)
    parser.add_argument("--markdown-out", type=Path, default=None)
    return parser


def run_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    """运行真实 FileScanner，并读取本轮落库指标生成 payload。"""
    scanner = FileScanner()
    scan_result = scanner.scan_files(
        start_date=args.start_date,
        end_date=args.end_date,
        summary_mode=args.summary_mode,
    )
    run_detail = scanner.scan_index_store.latest_scan_run_detail()
    extension_metrics = scanner.scan_index_store.list_extension_metrics(
        run_detail["run_id"]
    )
    return build_benchmark_payload(
        scan_result=scan_result,
        run_detail=run_detail,
        extension_metrics=extension_metrics,
        reparse_details=scanner.last_reparse_details,
        start_date=args.start_date,
        end_date=args.end_date,
        summary_mode=args.summary_mode,
    )


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
