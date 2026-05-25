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
from src.services.file_scanner import (  # noqa: E402
    FileScanner,
    NOT_PARSED_PARSER_BACKEND,
)
from src.services.light_text_parser import LIGHT_TEXT_PARSER_BACKEND  # noqa: E402
from src.services.scan_metrics import ExtensionMetrics, ReparseDetail  # noqa: E402


SUBPROCESS_PARSER_BACKEND = "subprocess"
TRUNCATED_SUMMARY_KEY = "truncated"
SUMMARY_BACKEND_KEYS = (
    LIGHT_TEXT_PARSER_BACKEND,
    SUBPROCESS_PARSER_BACKEND,
    NOT_PARSED_PARSER_BACKEND,
)


def _new_extension_backend_summary() -> dict[str, int]:
    return {
        LIGHT_TEXT_PARSER_BACKEND: 0,
        SUBPROCESS_PARSER_BACKEND: 0,
        NOT_PARSED_PARSER_BACKEND: 0,
        TRUNCATED_SUMMARY_KEY: 0,
    }


def _sort_extension_backend_summary(item: dict[str, int]) -> dict[str, int]:
    ordered: dict[str, int] = {
        LIGHT_TEXT_PARSER_BACKEND: item.get(LIGHT_TEXT_PARSER_BACKEND, 0),
        SUBPROCESS_PARSER_BACKEND: item.get(SUBPROCESS_PARSER_BACKEND, 0),
        NOT_PARSED_PARSER_BACKEND: item.get(NOT_PARSED_PARSER_BACKEND, 0),
    }
    extra_backends = sorted(
        key
        for key in item
        if key not in SUMMARY_BACKEND_KEYS and key != TRUNCATED_SUMMARY_KEY
    )
    for backend in extra_backends:
        ordered[backend] = item[backend]
    ordered[TRUNCATED_SUMMARY_KEY] = item.get(TRUNCATED_SUMMARY_KEY, 0)
    return ordered


def _iter_backend_summary_rows(item: dict[str, int]) -> list[str]:
    extra_backends = sorted(
        key
        for key, count in item.items()
        if key not in SUMMARY_BACKEND_KEYS
        and key != TRUNCATED_SUMMARY_KEY
        and count > 0
    )
    standard_backend_rows = [
        LIGHT_TEXT_PARSER_BACKEND,
        NOT_PARSED_PARSER_BACKEND,
    ]
    visible_rows = [
        backend
        for backend in (*standard_backend_rows, *extra_backends)
        if item.get(backend, 0) > 0
    ]
    if visible_rows:
        return visible_rows
    return [
        backend
        for backend in (SUBPROCESS_PARSER_BACKEND,)
        if item.get(backend, 0) > 0
    ]


def build_parser_backend_summary(
    reparse_details: list[ReparseDetail],
) -> dict[str, Any]:
    """按解析后端聚合本轮重解析文件数量。"""
    summary: dict[str, Any] = {
        "direct_count": 0,
        "not_parsed_count": 0,
        "subprocess_count": 0,
        "truncated_count": 0,
        "by_extension": {},
    }
    for detail in reparse_details:
        backend = detail.parser_backend or NOT_PARSED_PARSER_BACKEND
        worker_lane = _resolve_worker_lane(detail, backend)
        extension = detail.extension
        by_extension = summary["by_extension"].setdefault(
            extension,
            _new_extension_backend_summary(),
        )
        if worker_lane == SUBPROCESS_PARSER_BACKEND:
            summary["subprocess_count"] += 1
            by_extension[SUBPROCESS_PARSER_BACKEND] += 1
        elif worker_lane == NOT_PARSED_PARSER_BACKEND:
            summary["not_parsed_count"] += 1
        else:
            summary["direct_count"] += 1
        if backend != SUBPROCESS_PARSER_BACKEND:
            by_extension[backend] = by_extension.get(backend, 0) + 1
        if detail.truncated:
            summary["truncated_count"] += 1
            by_extension[TRUNCATED_SUMMARY_KEY] += 1

    summary["by_extension"] = {
        extension: _sort_extension_backend_summary(item)
        for extension, item in sorted(summary["by_extension"].items())
    }
    return summary


def _resolve_worker_lane(detail: ReparseDetail, backend: str) -> str:
    """优先使用显式执行通道，兼容旧 reparse detail 的 backend 推断。"""
    worker_lane = getattr(detail, "worker_lane", "") or ""
    if worker_lane:
        return worker_lane
    if backend in {SUBPROCESS_PARSER_BACKEND, NOT_PARSED_PARSER_BACKEND}:
        return backend
    return "direct"


def build_benchmark_payload(
    scan_result: ScanResult,
    run_detail: dict[str, int],
    extension_metrics: list[ExtensionMetrics],
    reparse_details: list[ReparseDetail],
    start_date: date,
    end_date: date,
    summary_mode: bool,
    discovery_backend: str,
) -> dict[str, Any]:
    """组合 benchmark 输出结构，避免 CLI 和测试各自拼字段。"""
    return {
        "parameters": {
            "start_date": start_date.isoformat(),
            "end_date": end_date.isoformat(),
            "summary_mode": summary_mode,
            "discovery_backend": discovery_backend,
        },
        "scan_result": {
            "total_files": scan_result.total_files,
            "success_count": scan_result.success_count,
            "error_count": scan_result.error_count,
        },
        "metrics": dict(run_detail),
        "extension_metrics": [item.to_dict() for item in extension_metrics],
        "reparse_details": [item.to_dict() for item in reparse_details],
        "parser_backend_summary": build_parser_backend_summary(reparse_details),
    }


def render_markdown_report(payload: dict[str, Any]) -> str:
    """把 JSON payload 渲染成便于人工 review 的 Markdown 报告。"""
    parameters = payload["parameters"]
    scan_result = payload["scan_result"]
    metrics = payload["metrics"]
    extension_metrics = payload["extension_metrics"]
    reparse_details = payload.get("reparse_details", [])
    parser_backend_summary = payload.get(
        "parser_backend_summary",
        {
            "direct_count": 0,
            "not_parsed_count": 0,
            "subprocess_count": 0,
            "truncated_count": 0,
            "by_extension": {},
        },
    )

    lines = [
        "# Scanner Benchmark Report",
        "",
        "## Parameters",
        "",
        f"- start_date: `{parameters['start_date']}`",
        f"- end_date: `{parameters['end_date']}`",
        f"- summary_mode: `{parameters['summary_mode']}`",
        f"- discovery_backend: `{parameters.get('discovery_backend', 'rust')}`",
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
            "## Parser Backend Summary",
            "",
            f"- direct_count: `{parser_backend_summary.get('direct_count', 0)}`",
            "- not_parsed_count: "
            f"`{parser_backend_summary.get('not_parsed_count', 0)}`",
            "- subprocess_count: "
            f"`{parser_backend_summary.get('subprocess_count', 0)}`",
            "- truncated_count: "
            f"`{parser_backend_summary.get('truncated_count', 0)}`",
            "",
            "| extension | backend | backend_count | subprocess_count | extension_truncated_count |",
            "|---|---|---:|---:|---:|",
        ]
    )
    by_extension = parser_backend_summary.get("by_extension", {})
    if by_extension:
        for extension in sorted(by_extension):
            item = by_extension[extension]
            for backend in _iter_backend_summary_rows(item):
                lines.append(
                    "| {extension} | {backend} | {backend_count} | "
                    "{subprocess_count} | {truncated_count} |".format(
                        extension=extension,
                        backend=backend,
                        backend_count=item[backend],
                        subprocess_count=item.get(SUBPROCESS_PARSER_BACKEND, 0),
                        truncated_count=item.get(TRUNCATED_SUMMARY_KEY, 0),
                    )
                )
    else:
        lines.append("| (none) | - | 0 | 0 | 0 |")

    lines.extend(
        [
            "",
            "## Reparse Details",
            "",
            "| extension | cache_miss_reason | parse_duration_ms | parse_status | "
            "attempted_backend | fallback_backend | fallback_reason | "
            "rust_duration_ms | fallback_duration_ms | path |",
            "|---|---|---:|---|---|---|---|---:|---:|---|",
        ]
    )
    if reparse_details:
        for item in reparse_details:
            fallback_reason = str(item.get("fallback_reason", "")).replace("|", "/")
            lines.append(
                "| {extension} | {cache_miss_reason} | {parse_duration_ms} | "
                "{parse_status} | {attempted_backend} | {fallback_backend} | "
                "{fallback_reason} | {rust_duration_ms} | "
                "{fallback_duration_ms} | {path} |".format(
                    extension=item.get("extension", ""),
                    cache_miss_reason=item.get("cache_miss_reason", ""),
                    parse_duration_ms=item.get("parse_duration_ms", 0),
                    parse_status=item.get("parse_status", ""),
                    attempted_backend=item.get("attempted_backend", ""),
                    fallback_backend=item.get("fallback_backend", ""),
                    fallback_reason=fallback_reason,
                    rust_duration_ms=item.get("rust_duration_ms", 0),
                    fallback_duration_ms=item.get("fallback_duration_ms", 0),
                    path=item.get("path", ""),
                )
            )
    else:
        lines.append("| (none) |  | 0 |  |  |  |  | 0 | 0 |  |")

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
        discovery_backend=str(scanner.scanner_cfg.get("discovery_backend", "rust")),
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
