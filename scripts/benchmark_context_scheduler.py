"""运行应用级 ContextScheduler，并只输出稳定 summary/run-id 证据。"""

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

from src.services.context_scheduler import (  # noqa: E402
    ContextBuildResult,
    ContextScheduleRequest,
    ContextScheduler,
)


def _parse_date(value: str) -> date:
    return date.fromisoformat(value)


def build_context_scheduler_summary(
    result: ContextBuildResult,
) -> dict[str, Any]:
    """只使用应用结果，不导入 scanner internals 或读取旧 store。"""
    summary = result.summary.model_dump(mode="json")
    input_chars = int(summary["input_chars"])
    output_chars = int(summary["output_chars"])
    summary.update(
        {
            "status": result.status,
            "warning_count": len(result.warnings),
            "warning_codes": sorted(
                warning.error_code for warning in result.warnings
            ),
            "error_code": "" if result.error is None else result.error.error_code,
            "compression_ratio": (
                0.0 if input_chars == 0 else output_chars / input_chars
            ),
        }
    )
    return summary


def build_benchmark_payload(
    *,
    context_result: ContextBuildResult,
    parameters: dict[str, Any],
) -> dict[str, Any]:
    return {
        "parameters": parameters,
        "scan_run": {"run_id": context_result.scan_run_id},
        "context_run": {
            "context_run_id": context_result.context_run_id,
            "status": context_result.status,
        },
        "context_scheduler_summary": build_context_scheduler_summary(
            context_result
        ),
    }


def render_markdown_report(payload: dict[str, Any]) -> str:
    parameters = payload.get("parameters", {})
    scan_run = payload.get("scan_run", {})
    context_run = payload.get("context_run", {})
    context_summary = payload.get("context_scheduler_summary", {})
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
        "## Run IDs",
        "",
        f"- scan_run_id: `{scan_run.get('run_id', '')}`",
        f"- context_run_id: `{context_run.get('context_run_id', '')}`",
        f"- status: `{context_run.get('status', '')}`",
        "",
        "## Context Scheduler Summary",
        "",
        "| key | value |",
        "|---|---:|",
    ]
    for key in sorted(context_summary):
        lines.append(f"| {key} | {_format_markdown_value(context_summary[key])} |")
    return "\n".join(lines) + "\n"


def write_report_files(
    payload: dict[str, Any],
    *,
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


def _format_markdown_value(value: Any) -> str:
    if isinstance(value, (dict, list)):
        return "`" + json.dumps(value, ensure_ascii=False, sort_keys=True) + "`"
    return f"`{value}`"


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Benchmark ai_daily_report ContextScheduler"
    )
    default_end_date = date.today()
    parser.add_argument(
        "--start-date",
        type=_parse_date,
        default=default_end_date - timedelta(days=1),
    )
    parser.add_argument("--end-date", type=_parse_date, default=default_end_date)
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
    result = ContextScheduler().build_context(
        ContextScheduleRequest(
            report_mode=args.report_mode,
            source="scan",
            start_date=args.start_date,
            end_date=args.end_date,
            compression_profile=args.compression_profile,
        )
    )
    parameters = {
        "start_date": args.start_date.isoformat(),
        "end_date": args.end_date.isoformat(),
        "report_mode": args.report_mode,
        "compression_profile": (
            args.compression_profile or f"{args.report_mode}_balanced_v1"
        ),
    }
    return build_benchmark_payload(
        context_result=result,
        parameters=parameters,
    )


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    payload = run_benchmark(args)
    write_report_files(
        payload,
        json_out=args.json_out,
        markdown_out=args.markdown_out,
    )
    print(json.dumps(payload, ensure_ascii=False, indent=2))
    return int(payload["context_run"]["status"] == "error")


if __name__ == "__main__":
    raise SystemExit(main())
