# scripts/corpus_gate.py
"""Fixed-corpus nine-state cache consistency gate (spec Part 9.1).

流程（每个组合 = parse × classification 的 empty/randomized-partial/full 之一）：

1. ``--gen-manifest``：构建固定合成语料，冷跑一次 build-context，冻结
   ``scripts/corpus_manifest.json``（discovery rows、classification truth、
   nominal plan、included/omitted/reason 集合、final_context SHA-256、partial
   subset/seed）。
2. 门禁运行：每次先在同一 work_dir 冷跑一次校准（得到当前 scanner 的
   parse/classification cache rows），再对 9 个组合各建**独立新 DB**，只按
   manifest 预种该组合（empty/partial/full），运行前断言 artifact/run 表为空
   （正常 snapshot lookup 必须 miss；不使用 bypass 开关，也不沿用上一样本刚写入
   的 cache）。随后断言：

   - 无 deadline 时九态 semantic output（final_context + decisions + semantic
     counts 的完整 tuple）完全一致，且与 manifest 冻结值一致；
   - ``text_pdf_coverage`` =（成功提取或 parse-cache 命中的 admitted text PDF）/
     admitted text PDF = 100%（分母 0 按 100% 并单列 count=0）；
   - 只有 manifest 指定的 semantic/policy NotParsed，无 runtime NotParsed；
   - safety guard 未触发（``stage_deadline_exhausted_count == 0``）；
   - ``pdfplumber_invocations`` == 获得 extraction slot 的 text PDF parse-cache
     misses；no-text 必须 0 次调用；
   - Part 3 classifier 数值门禁独立全绿（``tests/test_pdf_classifier.py``）。

证据只输出聚合值 + manifest hash，禁止真实路径/文件名/正文。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import shutil
import sqlite3
import subprocess
import sys
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from benchmark_seed_preparer import (  # noqa: E402
    _delete_run_artifact_lease,
    copy_sqlite_db,
)

MANIFEST_SCHEMA = "corpus_gate_manifest_v1"
EVIDENCE_SCHEMA = "corpus_gate_evidence_v1"
CORPUS_START = "2026-08-01"
CORPUS_END = "2026-08-08"
REPORT_MODE = "weekly"
CORPUS_MTIME = datetime(2026, 8, 5, 10, 0, 0, tzinfo=timezone.utc)
DEFAULT_SEED = "corpus-gate-v1-2026-08-09"

# 冻结 profile：max_file_size_mb=1 使 2MiB 文件走 file_size_policy；
# max_candidate_files=4 使按 nominal rank 的第 5 个 candidate 走
# semantic_file_quota_exhausted；large deadline 保证 safety guard 不触发。
# summary_pdf_max_pages=2 显式钉住每周档单文件页数上限（新默认 100 会使
# 3 个 PDF × 100 页预算恰好耗尽 100 页分类预算、只余 1 个分类槽），
# 保证 3 槽分类 + 1 槽抽取的九态矩阵不变。
# pdf_classification_timeout_ms=10000 放宽分类 worker 冷启动预算（默认 2s，
# 本机冷启动 2.6-3.3s 会 PARSER_TIMEOUT），门禁测一致性不测性能。
PROFILE: dict = {
    "schema_version": "scanner_profile_v2",
    "max_file_size_mb": 1,
    "max_candidate_files": 4,
    "max_total_pdf_classification_pages": 100,
    "max_pdf_text_extractions": 100,
    "summary_pdf_max_pages": 2,
    "pdf_classification_timeout_ms": 10000,
    "total_deadline_ms": 60000,
}

# parse × classification = 9 种
COMBOS: list[str] = [
    f"{parse}_{classification}"
    for parse in ("empty", "partial", "full")
    for classification in ("empty", "partial", "full")
]

TEXT_PDF_SOURCE = "tests/fixtures/pdf_classifier/text_plain_01.pdf"
NO_TEXT_BLANK_SOURCE = "tests/fixtures/pdf_classifier/no_text_blank_01.pdf"
NO_TEXT_IMAGE_SOURCE = "tests/fixtures/pdf_classifier/no_text_image_01.pdf"

CORPUS_TEXT_FILES: dict[str, str] = {
    "notes.md": "今日完成扫描调度器组装与状态矩阵重定义。\n",
    "report.txt": "Quarterly financial review: revenue reached $128,400.\n",
    "data/over_budget.md": "x" * (2 * 1024 * 1024),
}

DELETED_RUN_TABLES = ["engine_lease", "scan_runs", "context_artifacts"]
PRE_RUN_EMPTY_TABLES = [
    "scan_runs",
    "scan_run_attempts",
    "run_diagnostics",
    "scan_file_results",
    "scan_file_execution_v2",
    "scan_stage_metrics",
    "scan_extension_metrics",
    "scan_execution_metrics",
    "context_runs",
    "context_decisions",
    "context_artifacts",
    "context_artifact_files",
    "context_artifact_decisions",
    "engine_lease",
]


def _now_ms() -> int:
    return int(time.time() * 1000)


def _now_utc() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def semantic_key(context: dict) -> tuple:
    """final_context hash + decisions + semantic counts 的完整一致性键
    （brief：final_context + decisions + semantic counts 完全一致）。"""
    return (
        context["context_sha256"],
        json.dumps(context["decisions"], ensure_ascii=False, separators=(",", ":")),
        json.dumps(context["semantic_counts"], ensure_ascii=False, sort_keys=True, separators=(",", ":")),
    )


# ---------------------------------------------------------------------------
# corpus
# ---------------------------------------------------------------------------


def ensure_corpus(work_dir: Path) -> None:
    """构建确定性的合成语料（PDF 复制自已提交 fixture + 文本文件）。

    内容与 mtime 确定 => 同一 profile 下 plan/semantic 稳定（final_context 只含
    relative path + content，不含 source_guard/source_version）。每次覆盖重建，
    由校准冷跑捕获当次的 source_guard/source_version 并写入 cache key。
    """
    work_dir = Path(work_dir).resolve()
    work_dir.mkdir(parents=True, exist_ok=True)
    mtime = CORPUS_MTIME.timestamp()
    for rel, content in CORPUS_TEXT_FILES.items():
        path = work_dir / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        os.utime(path, (mtime, mtime))
    copies = {
        "pdf/text_plain_01.pdf": PROJECT_ROOT / TEXT_PDF_SOURCE,
        "pdf/no_text_blank_01.pdf": PROJECT_ROOT / NO_TEXT_BLANK_SOURCE,
        "pdf/no_text_image_01.pdf": PROJECT_ROOT / NO_TEXT_IMAGE_SOURCE,
    }
    for rel, src in copies.items():
        dest = work_dir / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        if not src.is_file():
            raise RuntimeError(f"corpus fixture missing: {src}")
        shutil.copyfile(src, dest)
        os.utime(dest, (mtime, mtime))


# ---------------------------------------------------------------------------
# scanner invocation
# ---------------------------------------------------------------------------


def _invoke(scanner: Path, args: list[str], request: dict) -> dict:
    proc = subprocess.run(
        [str(scanner), *args],
        input=json.dumps(request).encode("utf-8"),
        capture_output=True,
        timeout=180,
    )
    stdout = proc.stdout.decode("utf-8", errors="replace")
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(
            f"scanner {args} did not return JSON (exit={proc.returncode}): {stdout[:400]}"
        ) from exc
    return payload


def _build_context_request(
    work_dir: Path, db_path: Path, request_id: str, profile: dict, office_worker: Path
) -> dict:
    db_path = Path(db_path).resolve()
    db_path.parent.mkdir(parents=True, exist_ok=True)
    return {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": request_id,
        "work_dir": str(Path(work_dir).resolve()),
        "start_date": CORPUS_START,
        "end_date": CORPUS_END,
        "report_mode": REPORT_MODE,
        "compression_profile": None,
        "scan_db_path": str(db_path),
        "scanner_profile": profile,
        "adapters": {
            "office_worker_path": str(office_worker),
            "python_executable": str(Path(sys.executable).resolve()),
            "python_module_root": str(PROJECT_ROOT),
            "python_document_worker_module": "src.workers.document_parser_worker",
        },
    }


def _version_evidence(scanner: Path) -> dict:
    try:
        payload = _invoke(scanner, ["version"], {})
    except Exception:  # noqa: BLE001
        return {"engine_version": "unavailable"}
    return {
        "engine_version": payload.get("engine_version"),
        "engine_build": payload.get("engine_build"),
        "target_triple": payload.get("target_triple"),
    }


# ---------------------------------------------------------------------------
# sqlite helpers
# ---------------------------------------------------------------------------


def checkpoint_db(db_path: Path) -> None:
    conn = sqlite3.connect(str(db_path), timeout=30)
    try:
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    finally:
        conn.close()


def _fresh_dir(path: Path) -> Path:
    path = Path(path)
    if path.exists():
        shutil.rmtree(path)
    path.mkdir(parents=True)
    return path


def _read_discovery(conn: sqlite3.Connection) -> list[dict]:
    rows = conn.execute(
        "SELECT relative_path, size_bytes, file_type FROM file_inventory ORDER BY relative_path"
    ).fetchall()
    return [
        {
            "path": row[0],
            "size_bytes": row[1],
            "file_type": row[2],
            "extension": Path(row[0]).suffix.lower(),
        }
        for row in rows
    ]


def capture_semantic(conn: sqlite3.Connection, scan_run_id: int) -> dict:
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
        "SELECT relative_path, action, reason, priority, input_chars, output_chars,"
        " truncated, error_code"
        " FROM context_decisions WHERE context_run_id=? ORDER BY relative_path",
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
        "decisions": [list(decision) for decision in decisions],
    }


def read_execution_metrics(conn: sqlite3.Connection, scan_run_id: int) -> dict | None:
    row = conn.execute(
        "SELECT discovery_observed_file_count, candidate_file_count, admitted_file_count,"
        " classification_slot_count, nominal_charged_pages_total, extraction_slot_count,"
        " pdfplumber_invocations, snapshot_hit, parse_cache_lookup_count,"
        " classification_cache_lookup_count, parse_cache_all_hit, classification_cache_all_hit,"
        " stage_deadline_exhausted_count, classify_attempt_count, parse_attempt_count"
        " FROM scan_execution_metrics WHERE scan_run_id=?",
        (scan_run_id,),
    ).fetchone()
    if row is None:
        return None
    return {
        "discovery_observed_file_count": row[0],
        "candidate_file_count": row[1],
        "admitted_file_count": row[2],
        "classification_slot_count": row[3],
        "nominal_charged_pages_total": row[4],
        "extraction_slot_count": row[5],
        "pdfplumber_invocations": row[6],
        "snapshot_hit": bool(row[7]),
        "parse_cache_lookup_count": row[8],
        "classification_cache_lookup_count": row[9],
        "parse_cache_all_hit": None if row[10] is None else bool(row[10]),
        "classification_cache_all_hit": None if row[11] is None else bool(row[11]),
        "stage_deadline_exhausted_count": row[12],
        "classify_attempt_count": row[13],
        "parse_attempt_count": row[14],
    }


def _read_classification_truth(conn: sqlite3.Connection) -> dict[str, str]:
    rows = conn.execute(
        "SELECT fi.relative_path, cc.status FROM classification_cache cc"
        " JOIN file_inventory fi ON fi.file_identity = cc.file_identity"
        " ORDER BY fi.relative_path"
    ).fetchall()
    return {row[0]: row[1] for row in rows}


def _read_cache_paths(conn: sqlite3.Connection, table: str) -> list[str]:
    rows = conn.execute(
        f"SELECT DISTINCT fi.relative_path FROM {table} c"
        " JOIN file_inventory fi ON fi.file_identity = c.file_identity"
        " ORDER BY fi.relative_path"
    ).fetchall()
    return [row[0] for row in rows]


def capture_file_evidence(
    conn: sqlite3.Connection, scan_run_id: int, artifact_id: int | None
) -> dict[str, dict]:
    rows = conn.execute(
        "SELECT fr.relative_path, fr.parse_status, fr.parse_cache_status, fr.parser_backend,"
        " af.classifier_status"
        " FROM scan_file_results fr"
        " LEFT JOIN context_artifact_files af"
        "   ON af.file_identity = fr.file_identity AND af.artifact_id = ?2"
        " WHERE fr.scan_run_id = ?1",
        (scan_run_id, artifact_id if artifact_id is not None else -1),
    ).fetchall()
    result: dict[str, dict] = {}
    for rel, parse_status, parse_cache_status, parser_backend, classifier_status in rows:
        result[rel] = {
            "parse_status": parse_status,
            "parse_cache_status": parse_cache_status,
            "parser_backend": parser_backend,
            "classifier_status": classifier_status,
        }
    return result


# ---------------------------------------------------------------------------
# metrics derivation + coverage
# ---------------------------------------------------------------------------


def compute_text_pdf_metrics(semantic: dict, file_evidence: dict) -> dict:
    """text_pdf_coverage + pdfplumber 勾稽 + no-text 0 调用的逐文件推导。

    - admitted text PDF：ContentAdmissionPlan 中 admitted（keep/compress/
      metadata_only）且 classification=text_in_parse_window 的 PDF；
    - covered：parse_cache_status=fresh（cache hit）或 miss+success（提取成功）；
    - extraction_slot_miss_count：admitted text PDF 中 parse_cache_status=miss；
    - no-text violation：admitted no-text PDF 出现 pdf_text_v1 或非
      not_applicable 的 parse cache（禁止提取 no-text）。
    """
    admitted = {
        d[0]: d for d in semantic["decisions"]
        if d[1] in ("keep", "compress", "metadata_only")
    }
    admitted_text: list[str] = []
    admitted_no_text: list[str] = []
    missing_classifier: list[str] = []
    for path in admitted:
        if not path.lower().endswith(".pdf"):
            continue
        status = file_evidence.get(path, {}).get("classifier_status")
        if status == "text_in_parse_window":
            admitted_text.append(path)
        elif status == "no_text_in_parse_window":
            admitted_no_text.append(path)
        else:
            missing_classifier.append(path)

    covered = 0
    extraction_misses = 0
    for path in admitted_text:
        evidence = file_evidence.get(path, {})
        parse_cache_status = evidence.get("parse_cache_status")
        if parse_cache_status == "fresh":
            covered += 1
        elif parse_cache_status == "miss":
            extraction_misses += 1
            if evidence.get("parse_status") == "success":
                covered += 1

    no_text_violations = 0
    for path in admitted_no_text:
        evidence = file_evidence.get(path, {})
        # spec Part 3.2: no-text admitted 固定为 metadata-only draft，禁止进入
        # pdf_text_v1 提取路径；backend 必须是 scanner-owned pdf_metadata_v1。
        if evidence.get("parser_backend") != "pdf_metadata_v1":
            no_text_violations += 1

    total = len(admitted_text)
    coverage = 1.0 if total == 0 else covered / total
    return {
        "admitted_text_pdf_count": total,
        "covered_text_pdf_count": covered,
        "text_pdf_coverage": coverage,
        "admitted_no_text_pdf_count": len(admitted_no_text),
        "no_text_violations": no_text_violations,
        "missing_classifier_pdf_count": len(missing_classifier),
        "extraction_slot_miss_count": extraction_misses,
    }


def _derive_plan(semantic: dict, metrics: dict | None, classification_truth: dict) -> dict:
    admitted = {
        d[0] for d in semantic["decisions"]
        if d[1] in ("keep", "compress", "metadata_only")
    }
    return {
        "source_file_count": semantic["semantic_counts"]["source_file_count"],
        "candidate_file_count": (metrics or {}).get("candidate_file_count", 0),
        "classification_slot_count": (metrics or {}).get("classification_slot_count", 0),
        "nominal_charged_pages_total": (metrics or {}).get("nominal_charged_pages_total", 0),
        "admitted_file_count": (metrics or {}).get("admitted_file_count", 0),
        "extraction_slot_count": (metrics or {}).get("extraction_slot_count", 0),
        "included_file_count": semantic["semantic_counts"]["included_file_count"],
        "omitted_file_count": semantic["semantic_counts"]["omitted_file_count"],
        "error_file_count": semantic["semantic_counts"]["error_file_count"],
        "not_parsed_reasons": sorted(
            {d[2] for d in semantic["decisions"] if d[1] == "omit"}
        ),
        "admitted_text_pdf_paths": sorted(
            path for path in admitted
            if path.lower().endswith(".pdf")
            and classification_truth.get(path) == "text_in_parse_window"
        ),
        "admitted_no_text_pdf_paths": sorted(
            path for path in admitted
            if path.lower().endswith(".pdf")
            and classification_truth.get(path) == "no_text_in_parse_window"
        ),
    }


# ---------------------------------------------------------------------------
# calibration
# ---------------------------------------------------------------------------


def calibrate(
    *,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    db_path: Path,
    profile: dict,
) -> dict:
    db_path = Path(db_path)
    db_path.parent.mkdir(parents=True, exist_ok=True)
    request = _build_context_request(
        work_dir, db_path, str(uuid.uuid4()), profile, office_worker
    )
    envelope = _invoke(scanner, ["build-context"], request)
    checkpoint_db(db_path)
    if envelope.get("status") != "ok":
        raise RuntimeError(
            f"calibration build-context failed status={envelope.get('status')} "
            f"error={envelope.get('error')}"
        )
    conn = sqlite3.connect(str(db_path), timeout=30)
    try:
        files = _read_discovery(conn)
        semantic = capture_semantic(conn, envelope["scan_run_id"])
        metrics = read_execution_metrics(conn, envelope["scan_run_id"])
        classification_truth = _read_classification_truth(conn)
        parse_cache_paths = _read_cache_paths(conn, "parse_cache")
        classification_cache_paths = _read_cache_paths(conn, "classification_cache")
    finally:
        conn.close()
    return {
        "envelope": envelope,
        "db_path": db_path,
        "files": files,
        "semantic": semantic,
        "metrics": metrics,
        "classification_truth": classification_truth,
        "parse_cache_paths": parse_cache_paths,
        "classification_cache_paths": classification_cache_paths,
    }


# ---------------------------------------------------------------------------
# combo preparation
# ---------------------------------------------------------------------------


def _trim_cache(
    conn: sqlite3.Connection, table: str, state: str, keep_paths: list[str]
) -> None:
    if state == "empty":
        conn.execute(f"DELETE FROM {table}")
    elif state == "partial":
        if not keep_paths:
            conn.execute(f"DELETE FROM {table}")
            return
        placeholders = ",".join("?" * len(keep_paths))
        conn.execute(
            f"DELETE FROM {table} WHERE file_identity NOT IN ("
            f" SELECT file_identity FROM file_inventory WHERE relative_path IN ({placeholders}))",
            list(keep_paths),
        )
    elif state == "full":
        return
    else:
        raise ValueError(f"unknown cache state: {state}")


def _assert_pre_run_empty(conn: sqlite3.Connection) -> None:
    """运行前 artifact/run 表必须为空（正常 snapshot lookup 必须 miss）。"""
    for table in PRE_RUN_EMPTY_TABLES:
        count = conn.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
        if count:
            raise AssertionError(f"{table} is not empty before run ({count} rows)")


def prepare_combo_db(
    *,
    combo: str,
    calib_db: Path,
    combo_db: Path,
    manifest: dict,
) -> None:
    """复制校准 DB -> 删除 run/artifact/lease -> 按该组合裁剪两类 cache。"""
    parse_state, classification_state = combo.split("_")
    copy_sqlite_db(calib_db, combo_db)
    conn = sqlite3.connect(str(combo_db), timeout=30)
    try:
        conn.execute("PRAGMA foreign_keys = ON")
        with conn:
            _delete_run_artifact_lease(conn)
            _trim_cache(conn, "parse_cache", parse_state, manifest["partial"]["parse_paths"])
            _trim_cache(
                conn,
                "classification_cache",
                classification_state,
                manifest["partial"]["classification_paths"],
            )
            _assert_pre_run_empty(conn)
    finally:
        conn.close()
    checkpoint_db(combo_db)


# ---------------------------------------------------------------------------
# single combo
# ---------------------------------------------------------------------------


def run_combo(
    *,
    combo: str,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    out_root: Path,
    profile: dict,
    calib_db: Path,
    manifest: dict,
) -> dict:
    combo_dir = _fresh_dir(out_root / f"combo_{combo}")
    combo_db = combo_dir / "scan_index_v2.sqlite3"
    prepare_combo_db(combo=combo, calib_db=calib_db, combo_db=combo_db, manifest=manifest)

    request = _build_context_request(
        work_dir, combo_db, str(uuid.uuid4()), profile, office_worker
    )
    started = time.time()
    envelope = _invoke(scanner, ["build-context"], request)
    wall_ms = (time.time() - started) * 1000.0
    checkpoint_db(combo_db)

    return _collect_combo(combo, envelope, combo_db, wall_ms)


def _collect_combo(combo: str, envelope: dict, db_path: Path, wall_ms: float) -> dict:
    status = envelope.get("status")
    scan_run_id = envelope.get("scan_run_id")
    if scan_run_id is None:
        return {
            "combo": combo,
            "status": status,
            "wall_ms": wall_ms,
            "semantic": None,
            "semantic_counts": {},
            "error_file_count": None,
            "text_pdf_coverage": None,
            "admitted_text_pdf_count": 0,
            "covered_text_pdf_count": 0,
            "admitted_no_text_pdf_count": 0,
            "no_text_violations": 0,
            "missing_classifier_pdf_count": 0,
            "pdfplumber_invocations": 0,
            "extraction_slot_miss_count": 0,
            "stage_deadline_exhausted_count": 0,
            "snapshot_hit": None,
            "parse_cache_lookup_count": 0,
            "classification_cache_lookup_count": 0,
            "parse_cache_all_hit": None,
            "classification_cache_all_hit": None,
            "nominal_charged_pages_total": 0,
            "candidate_file_count": 0,
            "classification_slot_count": 0,
            "admitted_file_count": 0,
            "extraction_slot_count": 0,
            "not_parsed_reasons": [],
        }

    conn = sqlite3.connect(str(db_path), timeout=30)
    try:
        semantic = capture_semantic(conn, scan_run_id)
        metrics = read_execution_metrics(conn, scan_run_id) or {}
        row = conn.execute(
            "SELECT artifact_id FROM context_runs WHERE scan_run_id=?", (scan_run_id,)
        ).fetchone()
        artifact_id = row[0] if row else None
        file_evidence = capture_file_evidence(conn, scan_run_id, artifact_id)
    finally:
        conn.close()

    pdf = compute_text_pdf_metrics(semantic, file_evidence)
    return {
        "combo": combo,
        "status": status,
        "wall_ms": wall_ms,
        "semantic": semantic,
        "semantic_counts": semantic["semantic_counts"],
        "error_file_count": semantic["semantic_counts"]["error_file_count"],
        "text_pdf_coverage": pdf["text_pdf_coverage"],
        "admitted_text_pdf_count": pdf["admitted_text_pdf_count"],
        "covered_text_pdf_count": pdf["covered_text_pdf_count"],
        "admitted_no_text_pdf_count": pdf["admitted_no_text_pdf_count"],
        "no_text_violations": pdf["no_text_violations"],
        "missing_classifier_pdf_count": pdf["missing_classifier_pdf_count"],
        "pdfplumber_invocations": metrics.get("pdfplumber_invocations", 0),
        "extraction_slot_miss_count": pdf["extraction_slot_miss_count"],
        "stage_deadline_exhausted_count": metrics.get("stage_deadline_exhausted_count", 0),
        "snapshot_hit": metrics.get("snapshot_hit", False),
        "parse_cache_lookup_count": metrics.get("parse_cache_lookup_count", 0),
        "classification_cache_lookup_count": metrics.get("classification_cache_lookup_count", 0),
        "parse_cache_all_hit": metrics.get("parse_cache_all_hit"),
        "classification_cache_all_hit": metrics.get("classification_cache_all_hit"),
        "nominal_charged_pages_total": metrics.get("nominal_charged_pages_total", 0),
        "candidate_file_count": metrics.get("candidate_file_count", 0),
        "classification_slot_count": metrics.get("classification_slot_count", 0),
        "admitted_file_count": metrics.get("admitted_file_count", 0),
        "extraction_slot_count": metrics.get("extraction_slot_count", 0),
        "not_parsed_reasons": sorted(
            {d[2] for d in semantic["decisions"] if d[1] == "omit"}
        ),
    }


# ---------------------------------------------------------------------------
# manifest generation / validation
# ---------------------------------------------------------------------------


def _validate_calibration(calib: dict, manifest: dict) -> list[str]:
    errors: list[str] = []
    calib_files = {(f["path"], f["size_bytes"], f["extension"]) for f in calib["files"]}
    manifest_files = {
        (f["path"], f["size_bytes"], f["extension"]) for f in manifest["files"]
    }
    if calib_files != manifest_files:
        errors.append("discovery files differ from manifest (corpus drift?)")

    plan = manifest["plan"]
    derived = _derive_plan(calib["semantic"], calib["metrics"], calib["classification_truth"])
    for key in (
        "source_file_count",
        "candidate_file_count",
        "classification_slot_count",
        "nominal_charged_pages_total",
        "admitted_file_count",
        "extraction_slot_count",
        "included_file_count",
        "omitted_file_count",
        "error_file_count",
        "not_parsed_reasons",
    ):
        if derived.get(key) != plan.get(key):
            errors.append(f"plan {key} differs: {derived.get(key)} != {plan.get(key)}")

    if calib["classification_truth"] != manifest["classification_truth"]:
        errors.append("classification truth differs from manifest")
    if semantic_key(calib["semantic"]) != semantic_key(manifest["semantic"]):
        errors.append("semantic tuple differs from manifest")
    return errors


def _partial_subset(rng: random.Random, eligible: list[str]) -> list[str]:
    if not eligible:
        return []
    size = max(1, int(round(len(eligible) * 0.5)))
    return sorted(rng.sample(eligible, size))


def generate_manifest(
    *,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    out_root: Path,
    manifest_path: Path,
    seed: str = DEFAULT_SEED,
) -> dict:
    ensure_corpus(work_dir)
    out_root = Path(out_root)
    out_root.mkdir(parents=True, exist_ok=True)
    calib_dir = _fresh_dir(out_root / "calibration")
    calib = calibrate(
        scanner=scanner,
        office_worker=office_worker,
        work_dir=work_dir,
        db_path=calib_dir / "scan_index_v2.sqlite3",
        profile=PROFILE,
    )
    if (calib["metrics"] or {}).get("stage_deadline_exhausted_count", 0) != 0:
        raise RuntimeError("calibration triggered the safety deadline; cannot freeze manifest")

    rng = random.Random(seed)
    parse_partial = _partial_subset(rng, calib["parse_cache_paths"])
    classification_partial = _partial_subset(rng, calib["classification_cache_paths"])

    manifest = {
        "schema": MANIFEST_SCHEMA,
        "generated_at_utc": _now_utc(),
        "window": {"start": CORPUS_START, "end": CORPUS_END},
        "report_mode": REPORT_MODE,
        "profile": PROFILE,
        "files": calib["files"],
        "classification_truth": calib["classification_truth"],
        "plan": _derive_plan(calib["semantic"], calib["metrics"], calib["classification_truth"]),
        "semantic": calib["semantic"],
        "partial": {
            "seed": seed,
            "parse_paths": parse_partial,
            "classification_paths": classification_partial,
        },
        "combos": COMBOS,
    }
    manifest_path = Path(manifest_path)
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    return manifest


# ---------------------------------------------------------------------------
# gate runner
# ---------------------------------------------------------------------------


def run_gate(
    *,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    out_root: Path,
    manifest_path: Path,
    combos: list[str] | None = None,
) -> dict:
    manifest_path = Path(manifest_path)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    profile = manifest["profile"]
    manifest_hash = hashlib.sha256(
        json.dumps(manifest, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    ).hexdigest()

    ensure_corpus(work_dir)
    out_root = Path(out_root)
    out_root.mkdir(parents=True, exist_ok=True)

    calib_dir = _fresh_dir(out_root / "calibration")
    calib = calibrate(
        scanner=scanner,
        office_worker=office_worker,
        work_dir=work_dir,
        db_path=calib_dir / "scan_index_v2.sqlite3",
        profile=profile,
    )
    calibration_errors = _validate_calibration(calib, manifest)

    selected = combos if combos is not None else list(manifest["combos"])
    combo_results: list[dict] = []
    for combo in selected:
        result = run_combo(
            combo=combo,
            scanner=scanner,
            office_worker=office_worker,
            work_dir=work_dir,
            out_root=out_root,
            profile=profile,
            calib_db=calib["db_path"],
            manifest=manifest,
        )
        combo_results.append(result)

    gates = _compute_gates(combo_results, calibration_errors, manifest)

    return {
        "schema": "corpus_gate_result_v1",
        "manifest_hash": manifest_hash,
        "build": {
            "scanner_sha256": _sha256_file(scanner),
            "office_worker_sha256": _sha256_file(office_worker),
            **_version_evidence(scanner),
        },
        "corpus": {"source_count": len(manifest["files"])},
        "combo_count": len(combo_results),
        "combos": combo_results,
        "calibration_errors": calibration_errors,
        "gates": gates,
    }


def _compute_gates(combo_results: list[dict], calibration_errors: list[str], manifest: dict) -> dict:
    semantics = [c["semantic"] for c in combo_results if c["semantic"] is not None]
    semantic_identical = len({semantic_key(s) for s in semantics}) == 1
    all_match_manifest = all(
        c["semantic"] is not None and semantic_key(c["semantic"]) == semantic_key(manifest["semantic"])
        for c in combo_results
    )
    gates = {
        "semantic_identical": semantic_identical,
        "all_semantic_match_manifest": all_match_manifest,
        "all_status_ok": all(c["status"] == "ok" for c in combo_results),
        "all_error_file_count_zero": all(c["error_file_count"] == 0 for c in combo_results),
        "all_text_pdf_coverage_100": all(c["text_pdf_coverage"] == 1.0 for c in combo_results),
        "all_no_runtime_not_parsed": all(
            "runtime_deadline_exhausted" not in c["not_parsed_reasons"] for c in combo_results
        ),
        "all_not_parsed_match_manifest": all(
            set(c["not_parsed_reasons"]) == set(manifest["plan"]["not_parsed_reasons"])
            for c in combo_results
        ),
        "all_safety_guard_not_triggered": all(
            c["stage_deadline_exhausted_count"] == 0 for c in combo_results
        ),
        "all_pdfplumber_equals_extraction_misses": all(
            c["pdfplumber_invocations"] == c["extraction_slot_miss_count"] for c in combo_results
        ),
        "all_no_text_zero_invocations": all(c["no_text_violations"] == 0 for c in combo_results),
        "all_missing_classifier_zero": all(
            c["missing_classifier_pdf_count"] == 0 for c in combo_results
        ),
        "all_snapshot_miss": all(c["snapshot_hit"] is False for c in combo_results),
        "calibration_matches_manifest": not calibration_errors,
    }
    gates["passes"] = all(gates.values())
    return gates


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(128 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


# ---------------------------------------------------------------------------
# classifier numeric gate (Part 3, 独立全绿引用)
# ---------------------------------------------------------------------------


def run_classifier_numeric_gate() -> dict:
    """运行既有 tests/test_pdf_classifier.py::test_classifier_numeric_gate。"""
    try:
        proc = subprocess.run(
            [
                sys.executable,
                "-m",
                "pytest",
                "tests/test_pdf_classifier.py::test_classifier_numeric_gate",
                "-q",
            ],
            cwd=str(PROJECT_ROOT),
            capture_output=True,
            timeout=600,
        )
        detail = proc.stdout.decode("utf-8", errors="replace")
        if proc.returncode != 0:
            detail += proc.stderr.decode("utf-8", errors="replace")
        return {
            "ok": proc.returncode == 0,
            "exit_code": proc.returncode,
            "detail": detail[-3000:] if proc.returncode else "pass",
        }
    except Exception as error:  # noqa: BLE001
        return {"ok": False, "exit_code": None, "detail": str(error)}


def build_evidence(result: dict, numeric: dict) -> dict:
    combos = [{k: v for k, v in c.items() if k != "semantic"} for c in result["combos"]]
    gates = dict(result["gates"])
    gates["classifier_numeric_gate"] = numeric["ok"]
    gates["passes"] = bool(gates["passes"]) and bool(numeric["ok"])
    return {
        "schema": EVIDENCE_SCHEMA,
        "generated_at_utc": _now_utc(),
        "manifest_hash": result["manifest_hash"],
        "build": result["build"],
        "corpus": result["corpus"],
        "combo_count": result["combo_count"],
        "combos": combos,
        "calibration_errors": result["calibration_errors"],
        "classifier_numeric_gate": numeric,
        "gates": gates,
    }


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _build_binary_paths() -> tuple[Path, Path]:
    suffix = ".exe" if sys.platform == "win32" else ""
    scanner = PROJECT_ROOT / "rust" / "target" / "release" / f"ai-daily-scanner{suffix}"
    office = PROJECT_ROOT / "rust" / "target" / "release" / f"ai-daily-office-parser{suffix}"
    assert scanner.is_file(), scanner
    assert office.is_file(), office
    return scanner, office


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Fixed-corpus nine-state cache consistency gate (spec Part 9.1)."
    )
    parser.add_argument("--gen-manifest", action="store_true", help="regenerate the frozen manifest")
    parser.add_argument("--work-dir", type=Path, default=PROJECT_ROOT / "scripts" / "corpus")
    parser.add_argument(
        "--out-root", type=Path, default=PROJECT_ROOT / ".artifacts" / "corpus-gate-run"
    )
    parser.add_argument(
        "--manifest", type=Path, default=PROJECT_ROOT / "scripts" / "corpus_manifest.json"
    )
    parser.add_argument(
        "--evidence", type=Path, default=PROJECT_ROOT / ".artifacts" / "corpus-gate.json"
    )
    parser.add_argument("--seed", default=DEFAULT_SEED)
    parser.add_argument("--combos", default=None, help="comma-separated combo subset for debug")
    args = parser.parse_args(argv)

    scanner, office = _build_binary_paths()

    if args.gen_manifest:
        generate_manifest(
            scanner=scanner,
            office_worker=office,
            work_dir=args.work_dir,
            out_root=args.out_root,
            manifest_path=args.manifest,
            seed=args.seed,
        )
        print(f"manifest written: {args.manifest}")
        return 0

    selected = args.combos.split(",") if args.combos else None
    result = run_gate(
        scanner=scanner,
        office_worker=office,
        work_dir=args.work_dir,
        out_root=args.out_root,
        manifest_path=args.manifest,
        combos=selected,
    )
    numeric = run_classifier_numeric_gate()
    evidence = build_evidence(result, numeric)
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(
        json.dumps(evidence, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    print(f"evidence written: {args.evidence}")
    print(f"gates: {json.dumps(evidence['gates'], ensure_ascii=False, indent=2)}")
    print(f"classifier_numeric_gate: {json.dumps(numeric, ensure_ascii=False)[:400]}")

    if not evidence["gates"]["passes"]:
        print("=== STOP-GATE TRIGGERED ===")
        for error in evidence["calibration_errors"]:
            print(f"calibration: {error}")
        for combo in evidence["combos"]:
            if combo["status"] != "ok" or combo["text_pdf_coverage"] != 1.0:
                print(f"combo {combo['combo']}: status={combo['status']} "
                      f"coverage={combo['text_pdf_coverage']}")
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
