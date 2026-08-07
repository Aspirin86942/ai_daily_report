"""汇总 PDF 两侧原始 JSON，并按冻结门槛给出迁移判定。"""

from __future__ import annotations

import json
import math
from pathlib import Path
from statistics import fmean, median
from typing import Any


RESULTS = Path(__file__).resolve().parent / "results"
PYTHON_RESULT = RESULTS / "python.json"
RUST_RESULT = RESULTS / "rust.json"
OUT = RESULTS / "summary.json"


def percentile(values: list[float], quantile: float) -> float:
    """用 inclusive 线性插值计算百分位。"""
    if not values:
        raise ValueError("percentile requires at least one value")
    ordered = sorted(values)
    position = (len(ordered) - 1) * quantile
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    weight = position - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def load_result(path: Path) -> dict[str, Any]:
    result = json.loads(path.read_text(encoding="utf-8"))
    if result.get("schema_version") != "pdf_parser_gate_v1":
        raise ValueError(f"unsupported result schema in {path}")
    rows = result.get("rows")
    if not isinstance(rows, list) or not rows:
        raise ValueError(f"result has no rows: {path}")
    return result


def engine_summary(result: dict[str, Any]) -> dict[str, float | int | str]:
    rows = result["rows"]
    durations = [float(row["duration_ms"]) for row in rows]
    total_duration_ms = sum(durations)
    return {
        "engine": str(result["engine"]),
        "case_count": len(rows),
        "success_count": sum(int(row.get("exit_code", 0)) == 0 for row in rows),
        "mean_gt_ratio": round(fmean(float(row["gt_ratio"]) for row in rows), 6),
        "mean_printable_ratio": round(
            fmean(float(row["printable_ratio"]) for row in rows),
            6,
        ),
        "p50_duration_ms": round(median(durations), 3),
        "p90_duration_ms": round(percentile(durations, 0.9), 3),
        "total_duration_ms": round(total_duration_ms, 3),
        "throughput_files_per_second": round(
            len(rows) * 1000 / total_duration_ms,
            3,
        ),
    }


def summarize() -> dict[str, Any]:
    python_result = load_result(PYTHON_RESULT)
    rust_result = load_result(RUST_RESULT)
    python_files = [row["file"] for row in python_result["rows"]]
    rust_files = [row["file"] for row in rust_result["rows"]]
    if python_files != rust_files:
        raise ValueError("python and rust results cover different corpus files")

    python_summary = engine_summary(python_result)
    rust_summary = engine_summary(rust_result)
    quality_gt_floor = float(python_summary["mean_gt_ratio"]) * 0.95
    quality_printable_floor = 0.98
    speed_p50_ceiling_ms = float(python_summary["p50_duration_ms"]) * 0.5
    quality_pass = (
        float(rust_summary["mean_gt_ratio"]) >= quality_gt_floor
        and float(rust_summary["mean_printable_ratio"])
        >= quality_printable_floor
    )
    speed_pass = (
        float(rust_summary["p50_duration_ms"]) <= speed_p50_ceiling_ms
    )

    summary = {
        "schema_version": "pdf_parser_gate_summary_v1",
        "python": python_summary,
        "rust": rust_summary,
        "thresholds": {
            "rust_mean_gt_ratio_min": round(quality_gt_floor, 6),
            "rust_mean_printable_ratio_min": quality_printable_floor,
            "rust_p50_duration_ms_max": round(speed_p50_ceiling_ms, 3),
        },
        "comparison": {
            "rust_gt_ratio_vs_python": round(
                float(rust_summary["mean_gt_ratio"])
                / float(python_summary["mean_gt_ratio"]),
                6,
            ),
            "rust_p50_vs_python": round(
                float(rust_summary["p50_duration_ms"])
                / float(python_summary["p50_duration_ms"]),
                6,
            ),
        },
        "gate": {
            "quality_pass": quality_pass,
            "speed_pass": speed_pass,
            "migration_candidate": quality_pass and speed_pass,
            "decision": (
                "design_rust_migration"
                if quality_pass and speed_pass
                else "keep_pdfplumber"
            ),
        },
    }
    OUT.write_text(
        json.dumps(summary, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    return summary


if __name__ == "__main__":
    print(json.dumps(summarize(), ensure_ascii=False, indent=2))
