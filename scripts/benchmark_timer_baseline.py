# scripts/benchmark_timer_baseline.py
"""对未改 scanner binary 记录 7d cold / parse-cache-warm 同口径 wall-clock 基线。

冷：每样本一个全新隔离 DB（parse-cache 空），run 一次；
温：在已含全量 parse-cache 的同一 DB 上，用新 request_id 再 run（parse-cache 全命中）。
输出只含聚合指标 + child SHA + source count + 硬件，不写真实路径/正文。
pass/fail 只读 wall_clock_ms（外部计时），不读 ContextSummary.total_duration_ms。
"""
from __future__ import annotations
import hashlib
import json
import sys
import tempfile
import uuid
from datetime import date
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT_ROOT))

from benchmark_harness import wall_clock_ms  # noqa: E402


def sha256_file(path: Path) -> str:
    d = hashlib.sha256()
    d.update(path.read_bytes())
    return d.hexdigest()


def median_ms(values: list[float]) -> float:
    return sorted(values)[len(values) // 2]


def run_once(
    *,
    scanner: Path,
    work_dir: Path,
    office_worker: Path,
    python_executable: Path,
    db_path: Path,
    request_id: str,
    start: date,
    end: date,
) -> dict:
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": request_id,
        "work_dir": str(work_dir),
        "start_date": start.isoformat(),
        "end_date": end.isoformat(),
        "report_mode": "weekly",
        "compression_profile": None,
        "scan_db_path": str(db_path),
        "scanner_profile": {"schema_version": "scanner_profile_v1"},
        "adapters": {
            "office_worker_path": str(office_worker),
            "python_executable": str(python_executable),
            "python_module_root": str(PROJECT_ROOT),
            "python_document_worker_module": "src.workers.document_parser_worker",
        },
    }
    envelope: dict = {}

    def validator(raw: bytes) -> dict:
        parsed = json.loads(raw)
        if not isinstance(parsed, dict):
            raise ValueError("scanner response is not a JSON object")
        if parsed.get("contract") != "ai_daily_context":
            raise ValueError("scanner response contract mismatch")
        if parsed.get("request_id") != request_id:
            raise ValueError("scanner response request_id mismatch")
        if parsed.get("status") not in ("ok", "partial"):
            raise ValueError("scanner response status is not ok/partial")
        envelope.update(parsed)
        return parsed

    result = wall_clock_ms(
        [str(scanner), "build-context"],
        json.dumps(request).encode(),
        response_validator=validator,
    )
    return {
        "wall_ms": result.wall_ms,
        "exit_code": result.exit_code,
        "request_id": result.request_id,
        "validated": result.validated,
        "source_file_count": envelope.get("summary", {}).get("source_file_count"),
        "status": envelope.get("status"),
    }


def main() -> int:
    scanner = PROJECT_ROOT / "rust" / "target" / "release" / (
        "ai-daily-scanner" if sys.platform != "win32" else "ai-daily-scanner.exe"
    )
    office_worker = PROJECT_ROOT / "rust" / "target" / "release" / (
        "ai-daily-office-parser" if sys.platform != "win32" else "ai-daily-office-parser.exe"
    )
    assert scanner.is_file(), scanner
    assert office_worker.is_file(), office_worker
    work_dir = (PROJECT_ROOT / "tests" / "fixtures" / "worker_documents").resolve()
    assert work_dir.is_dir(), work_dir
    python_executable = Path(sys.executable).resolve()
    start, end = date(2026, 8, 1), date(2026, 8, 8)

    cold: list[dict] = []
    warm: list[dict] = []
    with tempfile.TemporaryDirectory() as td:
        base = Path(td)
        # 冷：每个样本使用全新隔离 DB（parse-cache 空）
        # 注意 scan_db_path 必须以 scan_index_v2.sqlite3 结尾且父目录存在
        cold_db_dirs = []
        for i in range(3):
            cold_dir = base / f"cold_{i}"
            cold_dir.mkdir()
            cold_db_dirs.append(cold_dir)
            cold.append(
                run_once(
                    scanner=scanner,
                    work_dir=work_dir,
                    office_worker=office_worker,
                    python_executable=python_executable,
                    db_path=cold_dir / "scan_index_v2.sqlite3",
                    request_id=str(uuid.uuid4()),
                    start=start,
                    end=end,
                )
            )
        # 温：cold_0 的 DB 已含全量 parse-cache，用新 request_id 复用
        for i in range(3):
            warm.append(
                run_once(
                    scanner=scanner,
                    work_dir=work_dir,
                    office_worker=office_worker,
                    python_executable=python_executable,
                    db_path=cold_db_dirs[0] / "scan_index_v2.sqlite3",
                    request_id=str(uuid.uuid4()),
                    start=start,
                    end=end,
                )
            )

    cold_ms = [r["wall_ms"] for r in cold]
    warm_ms = [r["wall_ms"] for r in warm]
    all_samples = [r["wall_ms"] for r in cold + warm]

    source_counts = [r["source_file_count"] for r in cold + warm if r["source_file_count"] is not None]
    source_count = max(source_counts, default=None)
    source_count_consistent = len(set(source_counts)) == 1 if source_counts else False

    bad = [r for r in cold + warm if r["exit_code"] != 0 or not r["validated"]]
    cold_median = median_ms(cold_ms)
    warm_median = median_ms(warm_ms)
    stop_gate_bad_sample = bool(bad)
    stop_gate_warm_slow = warm_median > 400.0

    out = {
        "scanner_sha256": sha256_file(scanner),
        "corpus": str(work_dir.relative_to(PROJECT_ROOT)).replace("\\", "/"),
        "source_count": source_count,
        "source_count_consistent": source_count_consistent,
        "cold_median_ms": cold_median,
        "cold_max_ms": max(cold_ms),
        "warm_median_ms": warm_median,
        "warm_max_ms": max(warm_ms),
        "all_samples": all_samples,
        "samples_clean": not stop_gate_bad_sample,
        "warm_median_ms_over_400": stop_gate_warm_slow,
    }
    (PROJECT_ROOT / ".artifacts").mkdir(exist_ok=True)
    (PROJECT_ROOT / ".artifacts" / "timer-baseline.json").write_text(
        json.dumps(out, indent=2), encoding="utf-8"
    )

    detail = {
        "cold": cold,
        "warm": warm,
    }
    print("=== timer-baseline (aggregate) ===")
    print(json.dumps(out, indent=2))
    print("=== per-sample detail (report only) ===")
    print(json.dumps(detail, indent=2))

    if stop_gate_bad_sample or stop_gate_warm_slow:
        print("=== STOP-GATE TRIGGERED ===")
        if stop_gate_bad_sample:
            print(
                f"sample(s) with exit_code!=0 or validated==False: "
                f"{[(r['request_id'], r['exit_code'], r['validated']) for r in bad]}"
            )
        if stop_gate_warm_slow:
            print(f"warm median {warm_median:.1f}ms > 400ms")
        print("项目是否冻结由 controller 决定；此处如实记录实测值。")
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
