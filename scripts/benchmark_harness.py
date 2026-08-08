# scripts/benchmark_harness.py
"""Timer-only scanner/worker harness. pass/fail 只读 benchmark_wall_ms，不读 ContextSummary.total_duration_ms。"""
from __future__ import annotations
import json
import subprocess
import time
from dataclasses import dataclass
from typing import Callable

@dataclass(frozen=True)
class BenchmarkResult:
    wall_ms: float
    exit_code: int
    request_id: str | None
    validated: bool

def wall_clock_ms(
    command: list[str],
    stdin_bytes: bytes,
    response_validator: Callable[[bytes], object] | None = None,
) -> BenchmarkResult:
    """wall_ms 从 CreateProcessW 前一刻到 stdout/stderr/exit/schema 校验完成。"""
    started = time.perf_counter()
    proc = subprocess.run(command, input=stdin_bytes, capture_output=True, timeout=3600)
    validated = True
    request_id = None
    if response_validator is not None:
        try:
            parsed = response_validator(proc.stdout)
            if isinstance(parsed, dict):
                request_id = parsed.get("request_id")
        except Exception:
            validated = False
    wall_ms = (time.perf_counter() - started) * 1000.0
    return BenchmarkResult(wall_ms=wall_ms, exit_code=proc.returncode, request_id=request_id, validated=validated)
