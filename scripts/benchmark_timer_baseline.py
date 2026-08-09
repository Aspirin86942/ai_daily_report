# scripts/benchmark_timer_baseline.py
"""对未改 scanner binary 记录 7d cold / parse-cache-warm 同口径 wall-clock 基线。

冷：每样本一个全新隔离 DB（parse-cache 空），run 一次；
温：在已含全量 parse-cache 的同一 DB 上，用新 request_id 再 run（parse-cache 全命中）。
输出只含聚合指标 + 匿名 manifest + normalized profile/build + 硬件，不写真实路径/正文。
pass/fail 只读 wall_clock_ms（外部计时），不读 ContextSummary.total_duration_ms。
"""
from __future__ import annotations
import hashlib
import json
import sqlite3
import sys
import tempfile
import uuid
from datetime import date
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT_ROOT))

from benchmark_harness import wall_clock_ms  # noqa: E402
from benchmark_snapshot_warm import (  # noqa: E402
    ANONYMOUS_CORPUS_MANIFEST_PROVENANCE,
    SEVEN_D_END,
    SEVEN_D_START,
    _hardware_evidence,
    _version_evidence,
    anonymous_corpus_hash,
    anonymous_corpus_manifest_complete,
    assert_portable_evidence as _assert_portable_evidence,
    build_identity_complete,
    capture_run_reproducibility,
    checkpoint_db,
    normalized_profile_evidence_complete,
)


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
        "benchmark_wall_ms": result.wall_ms,
        "exit_code": result.exit_code,
        "request_id": result.request_id,
        "validated": result.validated,
        "source_file_count": envelope.get("summary", {}).get("source_file_count"),
        "status": envelope.get("status"),
        "scan_run_id": envelope.get("scan_run_id"),
    }


def _missing_provenance() -> dict:
    return {
        "provenance_source": None,
        "context_profile_hash": None,
        "snapshot_key_sha256": None,
        "normalized_profile_json": None,
        "normalized_profile_hash_algorithm": "sha256(sorted-key-json-utf8)",
        "normalized_profile_sha256": None,
        "build_identity": None,
        "build_cross_checks": {
            "engine_build_matches_attempt": False,
            "office_worker_build_matches_attempt": False,
            "python_worker_build_matches_attempt": False,
        },
    }


def _attach_reproducibility(run: dict, db_path: Path) -> None:
    run["evidence_error_code"] = None
    run["corpus_manifest"] = {
        "anonymous_manifest_sha256": None,
        "source_count": None,
    }
    run["provenance"] = _missing_provenance()
    if run.get("scan_run_id") is None:
        return
    try:
        checkpoint_db(db_path)
        conn = sqlite3.connect(
            f"file:{db_path.resolve().as_posix()}?mode=ro",
            uri=True,
        )
        try:
            run["corpus_manifest"] = anonymous_corpus_hash(conn)
            run["provenance"] = capture_run_reproducibility(
                conn,
                run["scan_run_id"],
            )
        finally:
            conn.close()
    except (RuntimeError, sqlite3.Error, TypeError, ValueError):
        run["evidence_error_code"] = "REPRODUCIBILITY_EVIDENCE_UNAVAILABLE"


def _reproducibility_gates(samples: list[dict]) -> dict:
    corpus_keys = {
        json.dumps(sample.get("corpus_manifest"), sort_keys=True, separators=(",", ":"))
        for sample in samples
    }
    profile_keys = {
        (
            (sample.get("provenance") or {}).get("normalized_profile_json"),
            (sample.get("provenance") or {}).get("normalized_profile_sha256"),
        )
        for sample in samples
    }
    build_keys = {
        json.dumps(
            (sample.get("provenance") or {}).get("build_identity"),
            sort_keys=True,
            separators=(",", ":"),
        )
        for sample in samples
    }
    gates = {
        "sample_count_nonzero": bool(samples),
        "cold_corpus_manifests_complete": bool(samples)
        and all(
            anonymous_corpus_manifest_complete(sample.get("corpus_manifest"))
            for sample in samples
        ),
        "cold_corpus_manifests_identical": bool(samples) and len(corpus_keys) == 1,
        "sample_normalized_profiles_complete": bool(samples)
        and all(
            normalized_profile_evidence_complete(sample.get("provenance") or {})
            for sample in samples
        ),
        "sample_normalized_profiles_identical": bool(samples)
        and len(profile_keys) == 1,
        "sample_build_identities_complete": bool(samples)
        and all(
            build_identity_complete(
                (sample.get("provenance") or {}).get("build_identity")
            )
            for sample in samples
        ),
        "sample_build_identities_identical": bool(samples) and len(build_keys) == 1,
        "sample_build_cross_checks_passed": bool(samples)
        and all(
            (sample.get("provenance") or {}).get("build_cross_checks")
            and all(
                (sample.get("provenance") or {})["build_cross_checks"].values()
            )
            for sample in samples
        ),
        "passes": False,
    }
    gates["passes"] = all(value for key, value in gates.items() if key != "passes")
    return gates


def _anonymous_corpus_label(work_dir: Path) -> str:
    try:
        work_dir.relative_to(PROJECT_ROOT / "tests" / "fixtures")
    except ValueError:
        return "external-corpus"
    return "repository-fixture"


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(
        description="Record the same-yardstick wall-clock cold/parse-cache-warm baseline."
    )
    parser.add_argument(
        "--work-dir",
        type=Path,
        default=PROJECT_ROOT / "tests" / "fixtures" / "worker_documents",
        help="corpus 目录（默认 3-file fixture；真实 7d 语料传 D:/01- 工作）",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=PROJECT_ROOT / ".artifacts" / "timer-baseline.json",
        help="evidence JSON 输出路径",
    )
    parser.add_argument(
        "--start",
        type=date.fromisoformat,
        default=SEVEN_D_START,
        help=f"start date (default {SEVEN_D_START.isoformat()})",
    )
    parser.add_argument(
        "--end",
        type=date.fromisoformat,
        default=SEVEN_D_END,
        help=f"end date (default {SEVEN_D_END.isoformat()})",
    )
    args = parser.parse_args(argv)

    scanner = PROJECT_ROOT / "rust" / "target" / "release" / (
        "ai-daily-scanner" if sys.platform != "win32" else "ai-daily-scanner.exe"
    )
    office_worker = PROJECT_ROOT / "rust" / "target" / "release" / (
        "ai-daily-office-parser" if sys.platform != "win32" else "ai-daily-office-parser.exe"
    )
    assert scanner.is_file(), scanner
    assert office_worker.is_file(), office_worker
    work_dir = args.work_dir.resolve()
    assert work_dir.is_dir(), work_dir
    python_executable = Path(sys.executable).resolve()
    start, end = args.start, args.end

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
            cold_db = cold_dir / "scan_index_v2.sqlite3"
            sample = run_once(
                scanner=scanner,
                work_dir=work_dir,
                office_worker=office_worker,
                python_executable=python_executable,
                db_path=cold_db,
                request_id=str(uuid.uuid4()),
                start=start,
                end=end,
            )
            _attach_reproducibility(sample, cold_db)
            cold.append(sample)
        # 温：cold_0 的 DB 已含全量 parse-cache，用新 request_id 复用
        for i in range(3):
            warm_db = cold_db_dirs[0] / "scan_index_v2.sqlite3"
            sample = run_once(
                scanner=scanner,
                work_dir=work_dir,
                office_worker=office_worker,
                python_executable=python_executable,
                db_path=warm_db,
                request_id=str(uuid.uuid4()),
                start=start,
                end=end,
            )
            _attach_reproducibility(sample, warm_db)
            warm.append(sample)

    cold_ms = [r["benchmark_wall_ms"] for r in cold]
    warm_ms = [r["benchmark_wall_ms"] for r in warm]
    all_samples = [r["benchmark_wall_ms"] for r in cold + warm]

    source_counts = [
        run["source_file_count"]
        for run in cold + warm
        if run["source_file_count"] is not None
    ]
    source_count = max(source_counts, default=None)
    source_count_consistent = len(set(source_counts)) == 1 if source_counts else False

    bad = [r for r in cold + warm if r["exit_code"] != 0 or not r["validated"]]
    cold_median = median_ms(cold_ms)
    warm_median = median_ms(warm_ms)
    cold_reproducibility = _reproducibility_gates(cold)
    all_reproducibility = _reproducibility_gates(cold + warm)
    stop_gate_bad_sample = bool(bad)
    stop_gate_warm_slow = warm_median > 400.0
    first_provenance = cold[0]["provenance"] if cold else _missing_provenance()
    first_manifest = cold[0]["corpus_manifest"] if cold else {
        "anonymous_manifest_sha256": None,
        "source_count": None,
    }
    version = _version_evidence(scanner)
    version_engine_build_matches_run = version.get("engine_build") == (
        first_provenance.get("build_identity") or {}
    ).get("engine_build")
    reproducibility_passes = (
        cold_reproducibility["passes"]
        and all_reproducibility["passes"]
        and source_count_consistent
        and version_engine_build_matches_run
    )
    stop_gate_reproducibility = not reproducibility_passes

    out = {
        "schema": "timer_baseline_v2",
        "timer_contract": {
            "pass_fail_metric": "benchmark_wall_ms",
            "legacy_347ms_engine_summary_is_comparable": False,
        },
        "hardware": _hardware_evidence(),
        "window": {
            "start": start.isoformat(),
            "end": end.isoformat(),
        },
        "report_mode": "weekly",
        "os_page_cache_declared_not_cleared": True,
        "build": {
            "scanner_sha256": sha256_file(scanner),
            "office_worker_sha256": sha256_file(office_worker),
            "engine_version": version.get("engine_version"),
            "engine_build": version.get("engine_build"),
            "target_triple": version.get("target_triple"),
            "run_identity": first_provenance.get("build_identity"),
        },
        "corpus": _anonymous_corpus_label(work_dir),
        "corpus_manifest": first_manifest,
        "corpus_manifest_provenance": ANONYMOUS_CORPUS_MANIFEST_PROVENANCE,
        "cold_corpus_manifests": [sample["corpus_manifest"] for sample in cold],
        "source_count": source_count,
        "source_count_consistent": source_count_consistent,
        "normalized_profile_json": first_provenance.get("normalized_profile_json"),
        "normalized_profile_hash_algorithm": first_provenance.get(
            "normalized_profile_hash_algorithm"
        ),
        "normalized_profile_sha256": first_provenance.get(
            "normalized_profile_sha256"
        ),
        "profile_and_build_provenance": first_provenance.get(
            "provenance_source"
        ),
        "cold_median_ms": cold_median,
        "cold_max_ms": max(cold_ms),
        "cold_benchmark_wall_ms": cold_ms,
        "warm_median_ms": warm_median,
        "warm_max_ms": max(warm_ms),
        "warm_benchmark_wall_ms": warm_ms,
        "all_benchmark_wall_ms": all_samples,
        "samples_clean": not stop_gate_bad_sample,
        "warm_median_ms_over_400": stop_gate_warm_slow,
        "reproducibility_gates": {
            "cold": cold_reproducibility,
            "all_samples": all_reproducibility,
            "envelope_source_count_consistent": source_count_consistent,
            "version_engine_build_matches_run": version_engine_build_matches_run,
            "passes": reproducibility_passes,
        },
    }
    try:
        _assert_portable_evidence(
            out,
            forbidden_paths=(work_dir, args.out, PROJECT_ROOT),
        )
    except ValueError as error:
        print(f"portable evidence gate failed: {error}")
        return 1
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(
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

    if stop_gate_bad_sample or stop_gate_warm_slow or stop_gate_reproducibility:
        print("=== STOP-GATE TRIGGERED ===")
        if stop_gate_bad_sample:
            print(
                f"sample(s) with exit_code!=0 or validated==False: "
                f"{[(r['request_id'], r['exit_code'], r['validated']) for r in bad]}"
            )
        if stop_gate_warm_slow:
            print(f"warm median {warm_median:.1f}ms > 400ms")
        if stop_gate_reproducibility:
            print("sample corpus/profile/build provenance is inconsistent")
        print("项目是否冻结由 controller 决定；此处如实记录实测值。")
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
