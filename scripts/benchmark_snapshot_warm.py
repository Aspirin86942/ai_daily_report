# scripts/benchmark_snapshot_warm.py
"""Snapshot warm / cache-only warm benchmark（spec Part 6 + Part 9.2）。

- 7d snapshot warm：固定窗口 2026-08-02..2026-08-08（真实语料 `D:\\01- 工作`），
  一个成功 cold run 后在同一隔离 DB 用 3 个全新 request_id 连续跑 3 次；
  每个样本运行前 DB 查询证明 request_id 不存在、每次创建新 scan_run_id
  （共同定义 harness-derived `idempotent_replay=false`）、`snapshot_hit=true`、
  context hash 与 cold 完全一致；判定 median ≤330ms / max ≤400ms。
- 30d/90d：cache-only warm 3 样本各从同一只读 seed 克隆新 DB（`snapshot_hit=false`
  + 两类 lookup count>0/all_hit=true）；snapshot warm 在另一隔离 DB cold 后用
  3 个新 request_id 连续跑；判定 snapshot warm median 比 cache-only warm 改善 ≥20%，
  final_context/decisions/semantic counts 完全一致。
- 真实 30d/90d cold（手工 acceptance，Part 9.2）：3 个独立 cold DB，记录
  stage_deadline_exhausted_count 与是否超过内部 deadline。

pass/fail 只读 harness `benchmark_wall_ms`（wall_clock_ms），永不读取
ContextSummary.total_duration_ms。证据只提交聚合指标 + 匿名 corpus hash +
硬件/build，禁止真实文件路径/文件名/正文（顶层 work_dir 除外，见 corpus 字段）。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import sqlite3
import sys
import time
import uuid
from datetime import date
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from benchmark_harness import wall_clock_ms  # noqa: E402
from benchmark_seed_preparer import (  # noqa: E402
    SeedPreparerError,
    copy_sqlite_db,
    prepare_cache_only_seed,
    sha256_file,
    verify_cache_only_seed_marker,
)

REAL_WORK_DIR = Path("D:/01- 工作").resolve()
SEVEN_D_START = date(2026, 8, 2)
SEVEN_D_END = date(2026, 8, 8)
SEVEN_D_REPORT_MODE = "weekly"
SEVEN_D_PROFILE = {"schema_version": "scanner_profile_v1"}

# spec Part 8.1/9.2：monthly 真实目录显式 summary_pdf_max_pages=5
def _monthly_profile(
    *,
    deadline_ms: int,
    candidates: int,
    classification_pages: int,
    extractions: int,
) -> dict:
    return {
        "schema_version": "scanner_profile_v2",
        "summary_pdf_max_pages": 5,
        "max_candidate_files": candidates,
        "max_total_pdf_classification_pages": classification_pages,
        "max_pdf_text_extractions": extractions,
        "total_deadline_ms": deadline_ms,
    }


SCENARIOS: dict[str, dict] = {
    "30d": {
        "start": date(2026, 7, 10),
        "end": date(2026, 8, 8),
        "report_mode": "monthly",
        "profile": _monthly_profile(
            deadline_ms=25000, candidates=384, classification_pages=370, extractions=16
        ),
        "cold_deadline_ms": 25000,
    },
    "90d": {
        "start": date(2026, 5, 11),
        "end": date(2026, 8, 8),
        "report_mode": "monthly",
        "profile": _monthly_profile(
            deadline_ms=45000, candidates=600, classification_pages=800, extractions=32
        ),
        "cold_deadline_ms": 45000,
    },
}


def median_ms(values: list[float]) -> float:
    if not values:
        return 0.0
    return sorted(values)[len(values) // 2]


def checkpoint_db(db_path: Path) -> None:
    """Checkpoint/truncate WAL so the main file reflects the logical state."""
    conn = sqlite3.connect(db_path, timeout=30)
    try:
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    finally:
        conn.close()


def request_id_exists(db_path: Path, request_id: str) -> bool:
    conn = sqlite3.connect(f"file:{db_path.resolve().as_posix()}?mode=ro", uri=True)
    try:
        row = conn.execute(
            "SELECT 1 FROM scan_runs WHERE request_id=? LIMIT 1", (request_id,)
        ).fetchone()
        return row is not None
    finally:
        conn.close()


def anonymous_corpus_hash(conn: sqlite3.Connection) -> dict:
    """匿名 discovery/source-version manifest hash + source count（spec Part 6）。"""
    rows = conn.execute(
        "SELECT file_identity, source_version, size_bytes"
        " FROM file_inventory ORDER BY file_identity"
    ).fetchall()
    digest = hashlib.sha256()
    for identity, source_version, size_bytes in rows:
        digest.update(hashlib.sha256(identity.encode("utf-8")).hexdigest().encode())
        digest.update(b"\x00")
        digest.update(source_version.encode("utf-8"))
        digest.update(b"\x00")
        digest.update(str(size_bytes).encode("ascii"))
        digest.update(b"\x1e")
    return {
        "anonymous_manifest_sha256": digest.hexdigest(),
        "source_count": len(rows),
    }


def capture_context_state(conn: sqlite3.Connection, scan_run_id: int) -> dict:
    """final_context hash + decisions + semantic counts（用于一致性命中判定）。"""
    row = conn.execute(
        "SELECT context_run_id, context_sha256, source_file_count, success_count,"
        " timeout_count, included_file_count, omitted_file_count, error_file_count,"
        " input_chars, output_chars"
        " FROM context_runs WHERE scan_run_id=?",
        (scan_run_id,),
    ).fetchone()
    if row is None:
        raise RuntimeError(f"context_runs row missing for scan_run_id={scan_run_id}")
    (
        context_run_id,
        context_sha256,
        source_file_count,
        success_count,
        timeout_count,
        included_file_count,
        omitted_file_count,
        error_file_count,
        input_chars,
        output_chars,
    ) = row
    decisions = conn.execute(
        "SELECT file_identity, action, reason, priority, input_chars, output_chars,"
        " truncated, error_code FROM context_decisions WHERE context_run_id=?"
        " ORDER BY file_identity",
        (context_run_id,),
    ).fetchall()
    return {
        "context_sha256": context_sha256,
        "semantic_counts": {
            "source_file_count": source_file_count,
            "success_count": success_count,
            "timeout_count": timeout_count,
            "included_file_count": included_file_count,
            "omitted_file_count": omitted_file_count,
            "error_file_count": error_file_count,
            "input_chars": input_chars,
            "output_chars": output_chars,
        },
        "decisions": decisions,
    }


# ---------------------------------------------------------------------------
# scanner process helpers
# ---------------------------------------------------------------------------


def _build_binary_paths() -> tuple[Path, Path]:
    suffix = ".exe" if sys.platform == "win32" else ""
    scanner = PROJECT_ROOT / "rust" / "target" / "release" / f"ai-daily-scanner{suffix}"
    office = (
        PROJECT_ROOT
        / "rust"
        / "target"
        / "release"
        / f"ai-daily-office-parser{suffix}"
    )
    assert scanner.is_file(), scanner
    assert office.is_file(), office
    return scanner, office


def _adapter_payload(office_worker: Path) -> dict:
    return {
        "office_worker_path": str(office_worker),
        "python_executable": str(Path(sys.executable).resolve()),
        "python_module_root": str(PROJECT_ROOT),
        "python_document_worker_module": "src.workers.document_parser_worker",
    }


def run_build_context(
    *,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    db_path: Path,
    request_id: str,
    start: date,
    end: date,
    report_mode: str,
    profile: dict,
) -> dict:
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": request_id,
        "work_dir": str(work_dir),
        "start_date": start.isoformat(),
        "end_date": end.isoformat(),
        "report_mode": report_mode,
        "compression_profile": None,
        "scan_db_path": str(db_path),
        "scanner_profile": profile,
        "adapters": _adapter_payload(office_worker),
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
        json.dumps(request).encode("utf-8"),
        response_validator=validator,
    )
    return {
        "wall_ms": result.wall_ms,
        "exit_code": result.exit_code,
        "request_id": result.request_id,
        "validated": result.validated,
        "status": envelope.get("status"),
        "scan_run_id": envelope.get("scan_run_id"),
        "context_run_id": envelope.get("context_run_id"),
    }


def run_inspect_v2(scanner: Path, db_path: Path, scan_run_id: int) -> dict:
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": str(uuid.uuid4()),
        "scan_db_path": str(db_path),
        "scan_run_id": scan_run_id,
        "include_content": False,
    }
    payload: dict = {}

    def validator(raw: bytes) -> dict:
        parsed = json.loads(raw)
        if not isinstance(parsed, dict):
            raise ValueError("inspect response is not a JSON object")
        if parsed.get("contract") != "ai_daily_context":
            raise ValueError("inspect response contract mismatch")
        if parsed.get("response_version") != 2:
            raise ValueError("inspect response is not v2")
        payload.update(parsed)
        return parsed

    result = wall_clock_ms(
        [str(scanner), "inspect-run", "--response-version", "2"],
        json.dumps(request).encode("utf-8"),
        response_validator=validator,
    )
    if result.exit_code != 0 or not result.validated or payload.get("status") != "ok":
        raise RuntimeError(
            f"inspect-run v2 failed exit={result.exit_code} "
            f"validated={result.validated} status={payload.get('status')}"
        )
    return payload


def _execution_metrics(inspect_payload: dict) -> dict:
    metrics = inspect_payload.get("execution_metrics")
    if not isinstance(metrics, dict):
        raise RuntimeError("inspect v2 response has no execution_metrics")
    return metrics


def _version_evidence(scanner: Path) -> dict:
    payload: dict = {}

    def validator(raw: bytes) -> dict:
        parsed = json.loads(raw)
        if not isinstance(parsed, dict):
            raise ValueError("version response is not a JSON object")
        payload.update(parsed)
        return parsed

    wall_clock_ms([str(scanner), "version"], b"", response_validator=validator)
    return payload


def _hardware_evidence() -> dict:
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "python": platform.python_version(),
    }


# ---------------------------------------------------------------------------
# 7d snapshot warm
# ---------------------------------------------------------------------------


def run_7d_snapshot_warm(
    *,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    out_root: Path,
    report_mode: str = SEVEN_D_REPORT_MODE,
    profile: dict | None = None,
) -> dict:
    profile = profile or SEVEN_D_PROFILE
    cold_dir = out_root / "7d_cold"
    cold_dir.mkdir(parents=True)
    cold_db = cold_dir / "scan_index_v2.sqlite3"

    cold_req_id = str(uuid.uuid4())
    cold = run_build_context(
        scanner=scanner,
        office_worker=office_worker,
        work_dir=work_dir,
        db_path=cold_db,
        request_id=cold_req_id,
        start=SEVEN_D_START,
        end=SEVEN_D_END,
        report_mode=report_mode,
        profile=profile,
    )
    checkpoint_db(cold_db)
    if cold["scan_run_id"] is None or cold["status"] != "ok":
        raise RuntimeError(f"7d cold run failed: {cold}")

    conn = sqlite3.connect(cold_db)
    try:
        corpus = anonymous_corpus_hash(conn)
        cold_context = capture_context_state(conn, cold["scan_run_id"])
    finally:
        conn.close()

    seen_run_ids = {cold["scan_run_id"]}
    samples: list[dict] = []
    for index in range(3):
        request_id = str(uuid.uuid4())
        idempotent_probe = {
            "request_id_absent_before_run": not request_id_exists(cold_db, request_id),
            "request_id": request_id,
        }
        run = run_build_context(
            scanner=scanner,
            office_worker=office_worker,
            work_dir=work_dir,
            db_path=cold_db,
            request_id=request_id,
            start=SEVEN_D_START,
            end=SEVEN_D_END,
            report_mode=report_mode,
            profile=profile,
        )
        checkpoint_db(cold_db)
        inspect_payload = run_inspect_v2(scanner, cold_db, run["scan_run_id"])
        metrics = _execution_metrics(inspect_payload)
        conn = sqlite3.connect(cold_db)
        try:
            context = capture_context_state(conn, run["scan_run_id"])
        finally:
            conn.close()
        sample = {
            "index": index,
            "wall_ms": run["wall_ms"],
            "scan_run_id": run["scan_run_id"],
            "idempotent_replay_false": (
                idempotent_probe["request_id_absent_before_run"]
                and run["scan_run_id"] not in seen_run_ids
            ),
            "snapshot_hit": bool(metrics.get("snapshot_hit")),
            "context_hash_identical": context["context_sha256"]
            == cold_context["context_sha256"],
        }
        samples.append(sample)
        seen_run_ids.add(run["scan_run_id"])

    warm_ms = [sample["wall_ms"] for sample in samples]
    all_hit = all(sample["snapshot_hit"] for sample in samples)
    all_idempotent = all(sample["idempotent_replay_false"] for sample in samples)
    all_context = all(sample["context_hash_identical"] for sample in samples)
    gates = {
        "median_le_330ms": median_ms(warm_ms) <= 330.0,
        "max_le_400ms": max(warm_ms) <= 400.0,
        "all_snapshot_hit": all_hit,
        "all_idempotent_replay_false": all_idempotent,
        "all_context_identical_to_cold": all_context,
    }
    gates["passes"] = all(gates.values())

    return {
        "window": {"start": SEVEN_D_START.isoformat(), "end": SEVEN_D_END.isoformat()},
        "report_mode": report_mode,
        "corpus": corpus,
        "cold_wall_ms": cold["wall_ms"],
        "snapshot_warm_wall_ms": warm_ms,
        "snapshot_warm_median_ms": median_ms(warm_ms),
        "snapshot_warm_max_ms": max(warm_ms),
        "samples": samples,
        "gates": gates,
    }


# ---------------------------------------------------------------------------
# 30d / 90d cache-only warm vs snapshot warm
# ---------------------------------------------------------------------------


def run_cache_only_vs_snapshot(
    *,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    label: str,
    out_root: Path,
) -> dict:
    scenario = SCENARIOS[label]
    start = scenario["start"]
    end = scenario["end"]
    report_mode = scenario["report_mode"]
    profile = scenario["profile"]
    deadline_ms = scenario["cold_deadline_ms"]

    # 3 independent cold DBs（手工 acceptance，Part 9.2）
    colds: list[dict] = []
    for index in range(3):
        cold_dir = out_root / f"{label}_cold_{index}"
        cold_dir.mkdir(parents=True)
        cold_db = cold_dir / "scan_index_v2.sqlite3"
        run = run_build_context(
            scanner=scanner,
            office_worker=office_worker,
            work_dir=work_dir,
            db_path=cold_db,
            request_id=str(uuid.uuid4()),
            start=start,
            end=end,
            report_mode=report_mode,
            profile=profile,
        )
        checkpoint_db(cold_db)
        if run["scan_run_id"] is None or run["status"] != "ok":
            raise RuntimeError(f"{label} cold run failed: {run}")
        inspect_payload = run_inspect_v2(scanner, cold_db, run["scan_run_id"])
        metrics = _execution_metrics(inspect_payload)
        conn = sqlite3.connect(cold_db)
        try:
            context = capture_context_state(conn, run["scan_run_id"])
        finally:
            conn.close()
        colds.append(
            {
                "db": cold_db,
                "run": run,
                "context": context,
                "metrics": metrics,
            }
        )

    cold_evidence = [
        {
            "wall_ms": cold["run"]["wall_ms"],
            "status": cold["run"]["status"],
            "stage_deadline_exhausted_count": cold["metrics"][
                "stage_deadline_exhausted_count"
            ],
            "deadline_ms": deadline_ms,
            "deadline_exceeded": cold["run"]["wall_ms"] > deadline_ms,
        }
        for cold in colds
    ]
    corpus_conn = sqlite3.connect(colds[0]["db"])
    try:
        corpus = anonymous_corpus_hash(corpus_conn)
    finally:
        corpus_conn.close()

    # cache-only seed from cold[0]（isolated clone + marker）
    seed_out = out_root / f"{label}_seed"
    seed_marker = out_root / f"{label}_seed.marker.json"
    seed = prepare_cache_only_seed(
        src=colds[0]["db"], out_dir=seed_out, marker=seed_marker
    )

    # cache-only warm：3 samples，各从只读 seed 克隆新 DB
    cache_samples: list[dict] = []
    for index in range(3):
        clone_dir = out_root / f"{label}_cache_warm_{index}"
        clone_dir.mkdir(parents=True)
        clone_db = clone_dir / "scan_index_v2.sqlite3"
        verify_cache_only_seed_marker(seed_marker, seed_out)
        copy_sqlite_db(seed_out / "scan_index_v2.sqlite3", clone_db)
        request_id = str(uuid.uuid4())
        idempotent_probe = {
            "request_id_absent_before_run": not request_id_exists(clone_db, request_id),
            "request_id": request_id,
        }
        run = run_build_context(
            scanner=scanner,
            office_worker=office_worker,
            work_dir=work_dir,
            db_path=clone_db,
            request_id=request_id,
            start=start,
            end=end,
            report_mode=report_mode,
            profile=profile,
        )
        checkpoint_db(clone_db)
        inspect_payload = run_inspect_v2(scanner, clone_db, run["scan_run_id"])
        metrics = _execution_metrics(inspect_payload)
        conn = sqlite3.connect(clone_db)
        try:
            context = capture_context_state(conn, run["scan_run_id"])
        finally:
            conn.close()
        cache_samples.append(
            {
                "index": index,
                "wall_ms": run["wall_ms"],
                "scan_run_id": run["scan_run_id"],
                "idempotent_replay_false": idempotent_probe[
                    "request_id_absent_before_run"
                ],
                "snapshot_hit": bool(metrics.get("snapshot_hit")),
                "parse_cache_lookup_count": metrics["parse_cache_lookup_count"],
                "classification_cache_lookup_count": metrics[
                    "classification_cache_lookup_count"
                ],
                "parse_cache_all_hit": metrics["parse_cache_all_hit"],
                "classification_cache_all_hit": metrics[
                    "classification_cache_all_hit"
                ],
                "context_sha256": context["context_sha256"],
            }
        )

    # snapshot warm：cold[1] 的 DB 上，3 个新 request_id 连续运行
    snap_cold = colds[1]
    snap_samples: list[dict] = []
    seen_run_ids = {snap_cold["run"]["scan_run_id"]}
    for index in range(3):
        request_id = str(uuid.uuid4())
        idempotent_probe = {
            "request_id_absent_before_run": not request_id_exists(
                snap_cold["db"], request_id
            ),
            "request_id": request_id,
        }
        run = run_build_context(
            scanner=scanner,
            office_worker=office_worker,
            work_dir=work_dir,
            db_path=snap_cold["db"],
            request_id=request_id,
            start=start,
            end=end,
            report_mode=report_mode,
            profile=profile,
        )
        checkpoint_db(snap_cold["db"])
        inspect_payload = run_inspect_v2(scanner, snap_cold["db"], run["scan_run_id"])
        metrics = _execution_metrics(inspect_payload)
        conn = sqlite3.connect(snap_cold["db"])
        try:
            context = capture_context_state(conn, run["scan_run_id"])
        finally:
            conn.close()
        snap_samples.append(
            {
                "index": index,
                "wall_ms": run["wall_ms"],
                "scan_run_id": run["scan_run_id"],
                "idempotent_replay_false": (
                    idempotent_probe["request_id_absent_before_run"]
                    and run["scan_run_id"] not in seen_run_ids
                ),
                "snapshot_hit": bool(metrics.get("snapshot_hit")),
                "context_sha256": context["context_sha256"],
                "context_identical_to_cold": context["context_sha256"]
                == snap_cold["context"]["context_sha256"],
            }
        )
        seen_run_ids.add(run["scan_run_id"])

    cache_ms = [sample["wall_ms"] for sample in cache_samples]
    snap_ms = [sample["wall_ms"] for sample in snap_samples]
    cache_median = median_ms(cache_ms)
    snap_median = median_ms(snap_ms)
    improvement = 0.0
    if cache_median > 0:
        improvement = 1.0 - snap_median / cache_median

    # semantic identity across cache-warm + snapshot-warm + their colds
    all_contexts = (
        [sample["context_sha256"] for sample in cache_samples]
        + [sample["context_sha256"] for sample in snap_samples]
        + [cold["context"]["context_sha256"] for cold in colds]
    )
    semantic_identical = len(set(all_contexts)) == 1

    cache_warm_ok = all(
        sample["snapshot_hit"] is False
        and sample["parse_cache_lookup_count"] > 0
        and sample["classification_cache_lookup_count"] > 0
        and sample["parse_cache_all_hit"] is True
        and sample["classification_cache_all_hit"] is True
        for sample in cache_samples
    )
    snap_warm_ok = all(
        sample["snapshot_hit"] is True and sample["idempotent_replay_false"]
        for sample in snap_samples
    )

    gates = {
        "snapshot_warm_median_improvement_ge_20pct": improvement >= 0.20,
        "cache_warm_all_snapshot_miss_and_cache_all_hit": cache_warm_ok,
        "snapshot_warm_all_snapshot_hit": snap_warm_ok,
        "semantic_identical": semantic_identical,
    }
    gates["passes"] = all(gates.values())

    return {
        "window": {"start": start.isoformat(), "end": end.isoformat()},
        "report_mode": report_mode,
        "profile": profile,
        "corpus": corpus,
        "cold_deadline_ms": deadline_ms,
        "cold": cold_evidence,
        "seed": {
            "source_sha256": seed["source_sha256"],
            "seed_sha256": seed["seed_sha256"],
            "nonce_prefix": seed["nonce"][:8],
            "cold_cache_state": seed["cold_cache_state"],
        },
        "cache_only_warm_wall_ms": cache_ms,
        "cache_only_warm_median_ms": cache_median,
        "cache_only_warm_samples": cache_samples,
        "snapshot_warm_wall_ms": snap_ms,
        "snapshot_warm_median_ms": snap_median,
        "snapshot_warm_max_ms": max(snap_ms),
        "snapshot_warm_samples": snap_samples,
        "improvement": improvement,
        "gates": gates,
    }


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Snapshot warm / cache-only warm benchmark (spec Part 6 / 9.2)."
    )
    parser.add_argument(
        "--work-dir",
        type=Path,
        default=REAL_WORK_DIR,
        help="work_dir（默认真实语料 D:/01- 工作）",
    )
    parser.add_argument(
        "--out-root",
        type=Path,
        default=PROJECT_ROOT / ".artifacts" / "snapshot-warm",
        help="evidence + temp DB 根目录",
    )
    parser.add_argument(
        "--only",
        choices=["7d", "30d", "90d", "7d,30d,90d"],
        default="7d,30d,90d",
        help="运行哪些场景",
    )
    parser.add_argument(
        "--report-mode-7d",
        choices=["daily", "weekly", "monthly"],
        default=SEVEN_D_REPORT_MODE,
        help="7d 场景 report_mode（默认 weekly，与 Plan 1 baseline 一致）",
    )
    args = parser.parse_args(argv)

    work_dir = args.work_dir.resolve()
    if not work_dir.is_dir():
        parser.error(f"work_dir {work_dir} is not a directory")
    out_root = args.out_root.resolve()
    out_root.mkdir(parents=True, exist_ok=True)

    scanner, office_worker = _build_binary_paths()
    results: dict = {
        "schema": "snapshot_warm_benchmark_v1",
        "generated_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "hardware": _hardware_evidence(),
        "build": {
            "scanner_sha256": sha256_file(scanner),
            "office_worker_sha256": sha256_file(office_worker),
        },
        "corpus": {"work_dir": str(work_dir)},
        "scenarios": {},
    }
    try:
        version_payload = _version_evidence(scanner)
        if isinstance(version_payload, dict):
            results["build"].update(
                {
                    "engine_version": version_payload.get("engine_version"),
                    "engine_build": version_payload.get("engine_build"),
                    "target_triple": version_payload.get("target_triple"),
                }
            )
    except Exception:
        results["build"]["engine_version"] = "unavailable"

    selected = [part for part in args.only.split(",") if part]
    for scenario in selected:
        try:
            if scenario == "7d":
                result = run_7d_snapshot_warm(
                    scanner=scanner,
                    office_worker=office_worker,
                    work_dir=work_dir,
                    out_root=out_root,
                    report_mode=args.report_mode_7d,
                )
            else:
                result = run_cache_only_vs_snapshot(
                    scanner=scanner,
                    office_worker=office_worker,
                    work_dir=work_dir,
                    label=scenario,
                    out_root=out_root,
                )
        except SeedPreparerError as error:
            print(f"seed preparer failed closed: {error}")
            return 1
        except Exception as error:  # noqa: BLE001 - benchmark evidence must be collected
            results["scenarios"][scenario] = {"error": str(error)}
            print(f"scenario {scenario} failed: {error}")
            continue
        results["scenarios"][scenario] = result

    evidence_path = out_root / "evidence.json"
    evidence_path.write_text(
        json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    print("=== snapshot-warm benchmark (aggregate) ===")
    compact = json.loads(json.dumps(results, default=str))
    print(json.dumps(compact, ensure_ascii=False, indent=2))

    failed = [
        scenario
        for scenario, result in results["scenarios"].items()
        if "error" in result or not result.get("gates", {}).get("passes", False)
    ]
    if failed:
        print("=== STOP-GATE TRIGGERED ===")
        for scenario in failed:
            result = results["scenarios"][scenario]
            if "error" in result:
                print(f"{scenario}: error={result['error']}")
            else:
                print(f"{scenario}: gates={json.dumps(result['gates'])}")
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
