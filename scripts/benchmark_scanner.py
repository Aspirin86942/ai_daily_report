"""运行 native scanner，并从本次结果输出完整性能证据。"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from datetime import date, timedelta
from pathlib import Path
from statistics import median
from typing import Any, Sequence

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from src.models.scanner_contract import (  # noqa: E402
    ContextEnvelope,
    FileAuditV2,
    ScannerEvidence,
)
from src.services.native_scanner import NativeScanner, ScanRequest  # noqa: E402


@dataclass(frozen=True, slots=True)
class BenchmarkRuntimeConfig:
    """显式 benchmark 配置；绝不加载本机 settings.yaml。"""

    work_dir: Path
    index_db_path: Path
    office_worker_path: Path

    def scanner_settings(self) -> dict[str, object]:
        return {"legacy_office_enabled": True}


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
    evidence: ScannerEvidence | None,
    start_date: date,
    end_date: date,
    summary_mode: bool,
) -> dict[str, Any]:
    """组合稳定 DTO；正文、cache 内容和 SQL schema 永不进入输出。"""
    if evidence is not None:
        if (
            envelope.scan_run_id != evidence.scan_run_id
            or envelope.context_run_id != evidence.context_run_id
            or envelope.summary != evidence.summary
        ):
            raise ValueError("context envelope and scanner evidence disagree")
        files = evidence.files
        stage_metrics = {
            item.stage: item.model_dump(mode="json")
            for item in evidence.stage_metrics
        }
        extension_metrics = [
            item.model_dump(mode="json")
            for item in evidence.extension_metrics
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
            "peak_worker_rss_bytes": (
                None
                if evidence is None
                else evidence.execution_metrics.peak_worker_rss_bytes
            ),
            "native_call_count": 1,
            "scanner_process_start_count": 0,
            "scanner_transport_serialized_bytes": 0,
        },
        "stage_metrics": stage_metrics,
        "extension_metrics": extension_metrics,
        "files": file_audits,
        "parser_backend_summary": build_parser_backend_summary(files),
        "warning_codes": sorted(item.error_code for item in envelope.warnings),
        "error_code": "" if envelope.error is None else envelope.error.error_code,
    }


def render_markdown_report(payload: dict[str, Any]) -> str:
    if "cold" in payload:
        return _render_suite_markdown(payload)
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


def _render_suite_markdown(payload: dict[str, Any]) -> str:
    gate = payload["gate"]
    lines = [
        "# Native Scanner Benchmark Report",
        "",
        f"- iterations: `{payload['parameters']['iterations']}`",
        f"- passed: `{gate['passed']}`",
        f"- scanner_process_start_count: `{gate['scanner_process_start_count']}`",
        f"- scanner_transport_serialized_bytes: `{gate['scanner_transport_serialized_bytes']}`",
        "",
        "| mode | median_ms | p95_ms | median_files_per_second | peak_worker_rss_bytes |",
        "|---|---:|---:|---:|---:|",
    ]
    for mode in ("cold", "warm"):
        item = payload[mode]
        lines.append(
            f"| {mode} | {item['median_ms']} | {item['p95_ms']} | "
            f"{item['median_files_per_second']} | {item['peak_worker_rss_bytes']} |"
        )
    lines.extend(
        [
            "",
            "## Baseline gate",
            "",
            "```json",
            json.dumps(gate["baseline"], ensure_ascii=False, indent=2, sort_keys=True),
            "```",
            "",
        ]
    )
    return "\n".join(lines)


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
    parser = argparse.ArgumentParser(description="Benchmark native scanner")
    default_end_date = date.today()
    parser.add_argument(
        "--start-date",
        type=_parse_date,
        default=default_end_date - timedelta(days=1),
    )
    parser.add_argument("--end-date", type=_parse_date, default=default_end_date)
    parser.add_argument("--summary-mode", action="store_true")
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument(
        "--office-worker-path",
        type=Path,
        default=PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-office-parser.exe",
    )
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--baseline-cold-ms", type=float, default=None)
    parser.add_argument("--baseline-warm-ms", type=float, default=None)
    parser.add_argument("--json-out", type=Path, default=None)
    parser.add_argument("--markdown-out", type=Path, default=None)
    return parser


def run_benchmark(
    args: argparse.Namespace,
    *,
    scanner: NativeScanner | None = None,
    scan_db_path: Path | None = None,
) -> dict[str, Any]:
    selected_db = scan_db_path or args.state_dir / "scan_index_v3.sqlite3"
    runtime_config = BenchmarkRuntimeConfig(
        work_dir=args.work_dir.resolve(strict=True),
        index_db_path=selected_db.resolve(),
        office_worker_path=args.office_worker_path.resolve(strict=True),
    )
    scanner = NativeScanner(
        runtime_config,
        index_db_path=selected_db,
    ) if scanner is None else scanner
    result = scanner.build_context(
        ScanRequest(
            report_mode="weekly" if args.summary_mode else "daily",
            start_date=args.start_date,
            end_date=args.end_date,
        )
    )
    return build_benchmark_payload(
        envelope=result.envelope,
        evidence=result.evidence,
        start_date=args.start_date,
        end_date=args.end_date,
        summary_mode=args.summary_mode,
    )


def run_benchmark_suite(args: argparse.Namespace) -> dict[str, Any]:
    """运行成对 cold/warm 样本；每对使用独立的新鲜 v3 数据库。"""
    if args.iterations < 1:
        raise ValueError("iterations must be positive")
    state_dir = args.state_dir.resolve()
    state_dir.mkdir(parents=True, exist_ok=True)
    cold_runs: list[dict[str, Any]] = []
    warm_runs: list[dict[str, Any]] = []
    for index in range(args.iterations):
        pair_dir = state_dir / f"pair_{index + 1}"
        database = pair_dir / "scan_index_v3.sqlite3"
        if database.exists():
            raise ValueError(f"benchmark database already exists: {database}")
        pair_dir.mkdir(parents=True, exist_ok=False)
        runtime_config = BenchmarkRuntimeConfig(
            work_dir=args.work_dir.resolve(strict=True),
            index_db_path=database,
            office_worker_path=args.office_worker_path.resolve(strict=True),
        )
        scanner = NativeScanner(runtime_config, index_db_path=database)
        cold_runs.append(run_benchmark(args, scanner=scanner, scan_db_path=database))
        warm_runs.append(run_benchmark(args, scanner=scanner, scan_db_path=database))

    cold = _summarize_samples(cold_runs)
    warm = _summarize_samples(warm_runs)
    gate = _performance_gate(args, cold_runs, warm_runs, cold, warm)
    return {
        "parameters": {
            "work_dir": str(args.work_dir.resolve()),
            "state_dir": str(state_dir),
            "iterations": args.iterations,
            "start_date": args.start_date.isoformat(),
            "end_date": args.end_date.isoformat(),
            "summary_mode": args.summary_mode,
            "engine": "native",
        },
        "cold": cold,
        "warm": warm,
        "gate": gate,
        "runs": {"cold": cold_runs, "warm": warm_runs},
    }


def _summarize_samples(samples: list[dict[str, Any]]) -> dict[str, Any]:
    durations = [sample["metrics"]["total_duration_ms"] for sample in samples]
    throughputs = [sample["metrics"]["files_per_second"] for sample in samples]
    rss_values = [
        sample["metrics"]["peak_worker_rss_bytes"]
        for sample in samples
        if sample["metrics"]["peak_worker_rss_bytes"] is not None
    ]
    return {
        "median_ms": round(float(median(durations)), 3),
        "p95_ms": float(sorted(durations)[max(0, (95 * len(durations) + 99) // 100 - 1)]),
        "median_files_per_second": round(float(median(throughputs)), 3),
        "peak_worker_rss_bytes": max(rss_values, default=None),
    }


def _performance_gate(
    args: argparse.Namespace,
    cold_runs: list[dict[str, Any]],
    warm_runs: list[dict[str, Any]],
    cold: dict[str, Any],
    warm: dict[str, Any],
) -> dict[str, Any]:
    baseline: dict[str, Any] = {"evaluated": False}
    baseline_passed = True
    if args.baseline_cold_ms is not None or args.baseline_warm_ms is not None:
        if args.baseline_cold_ms is None or args.baseline_warm_ms is None:
            raise ValueError("both baseline values are required")
        cold_limit = round(args.baseline_cold_ms * 1.05, 3)
        warm_limit = round(args.baseline_warm_ms * 1.05, 3)
        baseline_passed = (
            cold["median_ms"] <= cold_limit
            and warm["median_ms"] <= warm_limit
        )
        baseline = {
            "evaluated": True,
            "cold_reference_ms": args.baseline_cold_ms,
            "warm_reference_ms": args.baseline_warm_ms,
            "cold_limit_ms": cold_limit,
            "warm_limit_ms": warm_limit,
            "passed": baseline_passed,
        }
    complete = all(
        run["status"] == "ok"
        and run["scan_result"]["error_count"] == 0
        and run["scan_result"]["timeout_count"] == 0
        for run in cold_runs + warm_runs
    )
    warm_reuse = all(
        run["metrics"]["reparsed_count"] == 0
        and run["metrics"]["reused_count"] == run["scan_result"]["total_files"]
        for run in warm_runs
    )
    return {
        "passed": complete and warm_reuse and baseline_passed,
        "complete": complete,
        "warm_full_reuse": warm_reuse,
        "scanner_process_start_count": 0,
        "scanner_transport_serialized_bytes": 0,
        "baseline": baseline,
    }


def main(argv: Sequence[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    payload = run_benchmark_suite(args)
    write_report_files(payload, args.json_out, args.markdown_out)
    print(json.dumps(payload, ensure_ascii=False, indent=2))
    return int(not payload["gate"]["passed"])


if __name__ == "__main__":
    raise SystemExit(main())
