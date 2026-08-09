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
import shutil
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


def _fresh_dir(path: Path) -> Path:
    """Create a fresh isolated directory (never reuse a stale DB/cold dir)."""
    if path.exists():
        shutil.rmtree(path)
    path.mkdir(parents=True)
    return path


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
    # rows 转 list 以便 JSON 序列化与跨样本比较
    decisions = [list(row) for row in decisions]
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


def semantic_key(context: dict) -> tuple:
    """final_context hash + decisions + semantic counts 的完整一致性键
    （spec Part 6：``final_context``/decisions/semantic counts 完全一致）。"""
    return (
        context["context_sha256"],
        json.dumps(context["decisions"], ensure_ascii=False, sort_keys=False),
        json.dumps(context["semantic_counts"], ensure_ascii=False, sort_keys=True),
    )


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
        # ok / partial / error 都接受并记录；由场景逻辑判定 cold 是否 clean。
        envelope.update(parsed)
        return parsed

    result = wall_clock_ms(
        [str(scanner), "build-context"],
        json.dumps(request).encode("utf-8"),
        response_validator=validator,
    )
    response_error = envelope.get("error")
    if isinstance(response_error, dict):
        message = str(response_error.get("message") or "")[:1024]
        for sensitive in (
            response_error.get("file_path"),
            str(work_dir),
            str(db_path),
        ):
            if sensitive:
                message = message.replace(str(sensitive), "<redacted-path>")
        response_error = {
            key: response_error.get(key)
            for key in ("error_code", "retryable", "stage", "backend")
        }
        response_error["message"] = message
    else:
        response_error = None
    return {
        "wall_ms": result.wall_ms,
        "exit_code": result.exit_code,
        "request_id": result.request_id,
        "validated": result.validated,
        "status": envelope.get("status"),
        "response_error": response_error,
        "scan_run_id": envelope.get("scan_run_id"),
        "context_run_id": envelope.get("context_run_id"),
    }


def run_inspect_v2(scanner: Path, db_path: Path, scan_run_id: int) -> dict:
    """Non-raising inspect v2 probe. Returns {ok, payload, error}."""
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
        payload.update(parsed)
        return parsed

    result = wall_clock_ms(
        [str(scanner), "inspect-run", "--response-version", "2"],
        json.dumps(request).encode("utf-8"),
        response_validator=validator,
    )
    if not result.validated:
        return {"ok": False, "payload": payload, "error": "inspect validate failed"}
    if payload.get("status") != "ok":
        error = payload.get("error") or {}
        code = error.get("error_code") or "inspect status=error"
        message = error.get("message") or ""
        detail = f"{code}: {message}" if message else code
        return {"ok": False, "payload": payload, "error": detail}
    if result.exit_code != 0:
        return {"ok": False, "payload": payload, "error": "inspect exit non-zero"}
    return {"ok": True, "payload": payload, "error": None}


def _execution_metrics(inspect_payload: dict) -> dict:
    metrics = inspect_payload.get("execution_metrics")
    if not isinstance(metrics, dict):
        raise RuntimeError("inspect v2 response has no execution_metrics")
    return metrics


def query_deadline_metrics(db_path: Path, scan_run_id: int) -> dict | None:
    """DB 直接读取 stage_deadline_exhausted_count（inspect v2 对 deadline-partial
    失败时的诚实回退；null 表示该 run 无 metrics 行）。"""
    try:
        conn = sqlite3.connect(f"file:{db_path.resolve().as_posix()}?mode=ro", uri=True)
        try:
            row = conn.execute(
                "SELECT stage_deadline_exhausted_count, snapshot_hit,"
                " parse_cache_lookup_count, classification_cache_lookup_count"
                " FROM scan_execution_metrics WHERE scan_run_id=?",
                (scan_run_id,),
            ).fetchone()
            if row is None:
                return None
            return {
                "stage_deadline_exhausted_count": row[0],
                "snapshot_hit": bool(row[1]),
                "parse_cache_lookup_count": row[2],
                "classification_cache_lookup_count": row[3],
            }
        finally:
            conn.close()
    except sqlite3.Error:
        return None


def query_runtime_not_parsed_count(db_path: Path, scan_run_id: int) -> int | None:
    """DB 直接读取 runtime NotParsed 行数（deadline-partial 审计回退）。"""
    try:
        conn = sqlite3.connect(f"file:{db_path.resolve().as_posix()}?mode=ro", uri=True)
        try:
            row = conn.execute(
                "SELECT count(*) FROM context_decisions cd"
                " JOIN context_runs cr USING (context_run_id)"
                " WHERE cr.scan_run_id=? AND cd.reason='runtime_deadline_exhausted'",
                (scan_run_id,),
            ).fetchone()
            return row[0] if row else None
        finally:
            conn.close()
    except sqlite3.Error:
        return None


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
    cold_dir = _fresh_dir(out_root / "7d_cold")
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
    cold_inspect = None
    cold_clean = False
    cold_context = None
    corpus = {"source_count": None}
    if cold["scan_run_id"] is not None:
        cold_inspect = run_inspect_v2(scanner, cold_db, cold["scan_run_id"])
        conn = sqlite3.connect(cold_db)
        try:
            corpus = anonymous_corpus_hash(conn)
            cold_context = capture_context_state(conn, cold["scan_run_id"])
        finally:
            conn.close()
        # 7d snapshot warm 的 cold 只要求 Success（status=ok）。cold run 自身的
        # inspect v2 若因 RUN_CORRUPT 等缺陷失败，单独记录为 cold_inspect_error，
        # 不阻断 snapshot warm 测量（snapshot warm 的 inspect 走 snapshot rows）。
        cold_clean = cold["status"] == "ok" and cold_context is not None

    if not cold_clean:
        return {
            "window": {"start": SEVEN_D_START.isoformat(), "end": SEVEN_D_END.isoformat()},
            "report_mode": report_mode,
            "corpus": corpus,
            "cold_wall_ms": cold["wall_ms"],
            "cold_status": cold["status"],
            "cold_inspect_ok": bool(cold_inspect and cold_inspect["ok"]),
            "cold_inspect_error": None if cold_inspect is None else cold_inspect["error"],
            "samples": [],
            "gates": {
                "cold_runs_clean": False,
                "median_le_330ms": False,
                "max_le_400ms": False,
                "all_snapshot_hit": False,
                "all_idempotent_replay_false": False,
                "all_context_identical_to_cold": False,
                "passes": False,
            },
            "snapshot_warm_wall_ms": [],
            "snapshot_warm_median_ms": 0.0,
            "snapshot_warm_max_ms": 0.0,
        }

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
        inspect_result = run_inspect_v2(scanner, cold_db, run["scan_run_id"])
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
            "snapshot_hit": bool(
                _execution_metrics(inspect_result["payload"]).get("snapshot_hit")
            )
            if inspect_result["ok"]
            else False,
            "inspect_error": inspect_result["error"],
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
        "cold_runs_clean": True,
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
        "cold_status": cold["status"],
        "cold_inspect_ok": bool(cold_inspect and cold_inspect["ok"]),
        "cold_inspect_error": None if cold_inspect is None else cold_inspect["error"],
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
        cold_dir = _fresh_dir(out_root / f"{label}_cold_{index}")
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
        inspect_result = None
        metrics = {}
        context = None
        if run["scan_run_id"] is not None:
            inspect_result = run_inspect_v2(scanner, cold_db, run["scan_run_id"])
            if inspect_result["ok"]:
                metrics = _execution_metrics(inspect_result["payload"])
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
                "inspect_result": inspect_result,
            }
        )

    cold_evidence = []
    for cold in colds:
        metrics = cold["metrics"]
        inspect_result = cold["inspect_result"]
        run = cold["run"]
        # inspect v2 失败（如 deadline-partial RUN_CORRUPT）时从 DB 诚实回退。
        fallback_metrics = (
            None if inspect_result and inspect_result["ok"] else query_deadline_metrics(cold["db"], run["scan_run_id"])
        )
        deadline_count = (
            metrics.get("stage_deadline_exhausted_count")
            if inspect_result and inspect_result["ok"]
            else (
                None
                if fallback_metrics is None
                else fallback_metrics["stage_deadline_exhausted_count"]
            )
        )
        runtime_not_parsed = (
            None
            if run["scan_run_id"] is None
            else query_runtime_not_parsed_count(cold["db"], run["scan_run_id"])
        )
        cold_evidence.append(
            {
                "wall_ms": run["wall_ms"],
                "status": run["status"],
                "scan_run_id": run["scan_run_id"],
                "inspect_ok": bool(inspect_result and inspect_result["ok"]),
                "inspect_error": None
                if inspect_result is None
                else inspect_result["error"],
                "stage_deadline_exhausted_count": deadline_count,
                "runtime_not_parsed_count": runtime_not_parsed,
                "deadline_ms": deadline_ms,
                "deadline_exceeded": (
                    run["wall_ms"] > deadline_ms or deadline_count not in (None, 0)
                ),
            }
        )

    corpus_conn = sqlite3.connect(colds[0]["db"])
    try:
        corpus = anonymous_corpus_hash(corpus_conn)
    finally:
        corpus_conn.close()

    # 只有 3 个 cold 都 clean（status=ok、inspect ok、无 stage deadline 触发）才可
    # 做 cache-only vs snapshot 的 warm 对比；否则 acceptance 已在 cold 阶段失败。
    cold_runs_clean = all(
        cold["run"]["status"] == "ok"
        and cold["inspect_result"] is not None
        and cold["inspect_result"]["ok"]
        and cold["metrics"].get("stage_deadline_exhausted_count") == 0
        for cold in colds
    )
    if not cold_runs_clean:
        gates = {
            "cold_runs_clean": False,
            "snapshot_warm_median_improvement_ge_20pct": False,
            "cache_warm_all_snapshot_miss_and_cache_all_hit": False,
            "snapshot_warm_all_snapshot_hit": False,
            "semantic_identical": False,
            "passes": False,
        }
        return {
            "window": {"start": start.isoformat(), "end": end.isoformat()},
            "report_mode": report_mode,
            "profile": profile,
            "corpus": corpus,
            "cold_deadline_ms": deadline_ms,
            "cold": cold_evidence,
            "warm_comparison": "skipped_cold_not_clean",
            "cache_only_warm_wall_ms": [],
            "snapshot_warm_wall_ms": [],
            "improvement": 0.0,
            "gates": gates,
        }

    # cache-only seed from cold[0]（isolated clone + marker）
    seed_out = out_root / f"{label}_seed"
    if seed_out.exists():
        shutil.rmtree(seed_out)
    seed_marker = out_root / f"{label}_seed.marker.json"
    seed = prepare_cache_only_seed(
        src=colds[0]["db"], out_dir=seed_out, marker=seed_marker
    )

    # cache-only warm：3 samples，各从只读 seed 克隆新 DB
    cache_samples: list[dict] = []
    for index in range(3):
        clone_dir = _fresh_dir(out_root / f"{label}_cache_warm_{index}")
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
        inspect_result = run_inspect_v2(scanner, clone_db, run["scan_run_id"])
        conn = sqlite3.connect(clone_db)
        try:
            context = capture_context_state(conn, run["scan_run_id"])
        finally:
            conn.close()
        metrics = (
            _execution_metrics(inspect_result["payload"]) if inspect_result["ok"] else {}
        )
        cache_samples.append(
            {
                "index": index,
                "wall_ms": run["wall_ms"],
                "scan_run_id": run["scan_run_id"],
                "idempotent_replay_false": idempotent_probe[
                    "request_id_absent_before_run"
                ],
                "snapshot_hit": bool(metrics.get("snapshot_hit")),
                "parse_cache_lookup_count": metrics.get("parse_cache_lookup_count"),
                "classification_cache_lookup_count": metrics.get(
                    "classification_cache_lookup_count"
                ),
                "parse_cache_all_hit": metrics.get("parse_cache_all_hit"),
                "classification_cache_all_hit": metrics.get(
                    "classification_cache_all_hit"
                ),
                "inspect_error": inspect_result["error"],
                "context_sha256": context["context_sha256"],
                "semantic": context,
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
        inspect_result = run_inspect_v2(scanner, snap_cold["db"], run["scan_run_id"])
        conn = sqlite3.connect(snap_cold["db"])
        try:
            context = capture_context_state(conn, run["scan_run_id"])
        finally:
            conn.close()
        metrics = (
            _execution_metrics(inspect_result["payload"]) if inspect_result["ok"] else {}
        )
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
                "inspect_error": inspect_result["error"],
                "context_sha256": context["context_sha256"],
                "context_identical_to_cold": context["context_sha256"]
                == snap_cold["context"]["context_sha256"],
                "semantic": context,
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

    # semantic identity across cache-warm + snapshot-warm + their colds:
    # brief mandates final_context + decisions + semantic counts 完全一致,
    # so the gate compares the full captured tuple, not just context_sha256.
    all_contexts = (
        [sample["semantic"] for sample in cache_samples]
        + [sample["semantic"] for sample in snap_samples]
        + [cold["context"] for cold in colds]
    )
    semantic_identical = len({semantic_key(context) for context in all_contexts}) == 1

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
        "cold_runs_clean": True,
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
        "warm_comparison": "completed",
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

    # Evidence 只含聚合值；`semantic` 字段含 decisions（含 file_identity 真实路径），
    # 只用于内存中的一致性 gate，写入前必须剥离（scope：禁止真实路径/文件名）。
    def _strip_semantic(payload: dict) -> dict:
        for scenario in payload.get("scenarios", {}).values():
            for key in ("cache_only_warm_samples", "snapshot_warm_samples"):
                for sample in scenario.get(key, []):
                    if isinstance(sample, dict):
                        sample.pop("semantic", None)
        return payload

    evidence_path = out_root / "evidence.json"
    evidence = _strip_semantic(json.loads(json.dumps(results, default=str)))
    evidence_path.write_text(
        json.dumps(evidence, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    print("=== snapshot-warm benchmark (aggregate) ===")
    print(json.dumps(evidence, ensure_ascii=False, indent=2))

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
