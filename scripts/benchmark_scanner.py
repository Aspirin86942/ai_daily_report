"""运行 native scanner，并从本次结果输出完整性能证据。"""

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

from src.core.config import config  # noqa: E402
from src.models.scanner_contract import (  # noqa: E402
    ContextEnvelope,
    FileAuditV2,
    InspectRunResponseV2,
)
from src.services.native_scanner import NativeScanner, ScanRequest  # noqa: E402


def calculate_files_per_second(*, file_count: int, duration_ms: int) -> float:
    """按 scanner 汇总耗时计算文件吞吐；零工作量或零耗时不虚构结果。"""
    if file_count <= 0 or duration_ms <= 0:
        return 0.0
    return round(file_count * 1000 / duration_ms, 3)


def build_parser_backend_summary(files: list[FileAuditV2]) -> dict[str, Any]:
    """分别汇总真实 parser backend 和 worker lane，禁止混成一个维度。"""
    backend_counts: dict[str, int] = {}
    lane_counts: dict[str, int] = {}
    by_extension: dict[str, dict[str, dict[str, int] | int]] = {}
    for item in files:
        backend_counts[item.parser_backend] = (
            backend_counts.get(item.parser_backend, 0) + 1
        )
        lane_counts[item.worker_lane] = lane_counts.get(item.worker_lane, 0) + 1
        extension = by_extension.setdefault(
            "." + item.relative_path.lower().rsplit(".", 1)[-1]
            if "." in item.relative_path
            else "(none)",
            {"backends": {}, "lanes": {}, "truncated_count": 0},
        )
        backends = extension["backends"]
        lanes = extension["lanes"]
        assert isinstance(backends, dict)
        assert isinstance(lanes, dict)
        backends[item.parser_backend] = backends.get(item.parser_backend, 0) + 1
        lanes[item.worker_lane] = lanes.get(item.worker_lane, 0) + 1
        if item.truncated:
            extension["truncated_count"] = int(extension["truncated_count"]) + 1
    return {
        "backend_counts": dict(sorted(backend_counts.items())),
        "worker_lane_counts": dict(sorted(lane_counts.items())),
        "truncated_count": sum(item.truncated for item in files),
        "by_extension": {
            extension: {
                "backends": dict(sorted(data["backends"].items())),
                "lanes": dict(sorted(data["lanes"].items())),
                "truncated_count": data["truncated_count"],
            }
            for extension, data in sorted(by_extension.items())
        },
    }


def build_benchmark_payload(
    *,
    envelope: ContextEnvelope,
    inspection: InspectRunResponseV2 | None,
    start_date: date,
    end_date: date,
    summary_mode: bool,
) -> dict[str, Any]:
    """组合稳定 DTO；正文、cache 内容和 SQL schema 永不进入输出。"""
    if inspection is not None:
        if (
            inspection.status != "ok"
            or envelope.scan_run_id != inspection.scan_run_id
            or envelope.context_run_id != inspection.context_run_id
            or envelope.summary != inspection.summary
        ):
            raise ValueError("build-context and inspect-run DTOs disagree")
        files = inspection.files
        stage_metrics = {
            item.stage: item.model_dump(mode="json")
            for item in inspection.stage_metrics
        }
        extension_metrics = [
            item.model_dump(mode="json")
            for item in inspection.extension_metrics
        ]
        file_audits = [item.model_dump(mode="json") for item in files]
    else:
        files = []
        stage_metrics = {}
        extension_metrics = []
        file_audits = []
    summary = envelope.summary
    reused_count = sum(
        item.parse_cache_status in {"fresh", "snapshot"} for item in files
    )
    reparsed_count = sum(item.parse_cache_status == "miss" for item in files)
    return {
        "parameters": {
            "start_date": start_date.isoformat(),
            "end_date": end_date.isoformat(),
            "summary_mode": summary_mode,
            "engine": "native",
        },
        "status": envelope.status,
        "scan_result": {
            "total_files": summary.source_file_count,
            "success_count": summary.success_count,
            "error_count": summary.error_file_count,
            "timeout_count": summary.timeout_count,
        },
        "metrics": {
            "run_id": envelope.scan_run_id,
            "context_run_id": envelope.context_run_id,
            "discovered_count": summary.source_file_count,
            "reused_count": reused_count,
            "reparsed_count": reparsed_count,
            "total_duration_ms": summary.total_duration_ms,
            "discovery_duration_ms": summary.discovery_duration_ms,
            "cache_duration_ms": stage_metrics.get("cache", {}).get(
                "duration_ms", 0
            ),
            "parse_duration_ms": summary.parse_duration_ms,
            "context_duration_ms": summary.compression_duration_ms,
            "files_per_second": calculate_files_per_second(
                file_count=summary.source_file_count,
                duration_ms=summary.total_duration_ms,
            ),
        },
        "stage_metrics": stage_metrics,
        "extension_metrics": extension_metrics,
        "files": file_audits,
        "parser_backend_summary": build_parser_backend_summary(files),
        "warning_codes": sorted(item.error_code for item in envelope.warnings),
        "error_code": "" if envelope.error is None else envelope.error.error_code,
    }


def render_markdown_report(payload: dict[str, Any]) -> str:
    parameters = payload["parameters"]
    result = payload["scan_result"]
    metrics = payload["metrics"]
    lines = [
        "# Rust Scanner Benchmark Report",
        "",
        "## Parameters",
        "",
        f"- start_date: `{parameters['start_date']}`",
        f"- end_date: `{parameters['end_date']}`",
        f"- summary_mode: `{parameters['summary_mode']}`",
        f"- engine: `{parameters['engine']}`",
        "",
        "## Counts",
        "",
        f"- total_files: `{result['total_files']}`",
        f"- success_count: `{result['success_count']}`",
        f"- error_count: `{result['error_count']}`",
        f"- timeout_count: `{result['timeout_count']}`",
        f"- reused_count: `{metrics['reused_count']}`",
        f"- reparsed_count: `{metrics['reparsed_count']}`",
        f"- files_per_second: `{metrics['files_per_second']}`",
        "",
        "## Stage Durations",
        "",
        "| stage | duration_ms |",
        "|---|---:|",
        f"| total | {metrics['total_duration_ms']} |",
        f"| discovery | {metrics['discovery_duration_ms']} |",
        f"| cache | {metrics['cache_duration_ms']} |",
        f"| parse | {metrics['parse_duration_ms']} |",
        f"| context | {metrics['context_duration_ms']} |",
        "",
        "## Parser Backend And Worker Lane Summary",
        "",
        "```json",
        json.dumps(
            payload["parser_backend_summary"],
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        ),
        "```",
        "",
        "## Extension Metrics",
        "",
        "```json",
        json.dumps(
            payload["extension_metrics"],
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        ),
        "```",
    ]
    return "\n".join(lines) + "\n"


def write_report_files(
    payload: dict[str, Any],
    json_out: Path | None,
    markdown_out: Path | None,
) -> None:
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
    return date.fromisoformat(value)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Benchmark Rust v2 scanner")
    default_end_date = date.today()
    parser.add_argument(
        "--start-date",
        type=_parse_date,
        default=default_end_date - timedelta(days=1),
    )
    parser.add_argument("--end-date", type=_parse_date, default=default_end_date)
    parser.add_argument("--summary-mode", action="store_true")
    parser.add_argument("--scan-db-path", type=Path, default=None)
    parser.add_argument("--json-out", type=Path, default=None)
    parser.add_argument("--markdown-out", type=Path, default=None)
    return parser


def run_benchmark(args: argparse.Namespace) -> dict[str, Any]:
    scanner = NativeScanner(
        config,
        index_db_path=args.scan_db_path,
    )
    result = scanner.build_context(
        ScanRequest(
            report_mode="weekly" if args.summary_mode else "daily",
            start_date=args.start_date,
            end_date=args.end_date,
        )
    )
    return build_benchmark_payload(
        envelope=result.envelope,
        inspection=result.evidence,
        start_date=args.start_date,
        end_date=args.end_date,
        summary_mode=args.summary_mode,
    )


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    payload = run_benchmark(args)
    write_report_files(payload, args.json_out, args.markdown_out)
    print(json.dumps(payload, ensure_ascii=False, indent=2))
    return int(payload["status"] == "error")


if __name__ == "__main__":
    raise SystemExit(main())
