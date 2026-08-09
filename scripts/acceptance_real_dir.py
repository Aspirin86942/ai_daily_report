# scripts/acceptance_real_dir.py
"""真实目录手工 acceptance（spec Part 9.2 / Plan 4 Task 3）。脚本即测试，含反作弊断言。

固定同一台机器、release build、`D:\\01- 工作`、report_mode=monthly、
RawScannerProfileV2 `summary_pdf_max_pages=5`：

- 30d (2026-07-10..2026-08-08, quota 384/370/16/25000): cold target median<=20s / max<=25s
- 90d (2026-05-11..2026-08-08, quota 600/800/32/45000): cold target median<=40s / max<=50s
- 7d snapshot warm re-run: target median<=370ms / max<=420ms（Part 6 用户重定）
- 30d/90d snapshot warm vs cache-only warm: 改善>=20% 且 semantic 完全一致

"cold" 唯一含义：每样本一个新建隔离 DB（parse/classification/artifact/run 全空），
通过子进程重启 scanner/Python worker；不尝试清 Windows OS page cache，但在证据中声明。

每样本反作弊条件（全部满足才通过，见 _assert_sample）：
- stage_deadline_exhausted_count == 0；无 runtime NotParsed；无 unknown；无 Error；无 Timeout
- text_pdf_coverage == 1.0；no-text pdfplumber_invocations == 0
- source_guard_unavailable_count == 0；session capability present；session_fallback_count == 0
- validated == true

pass/fail 只读 harness `benchmark_wall_ms`（wall_clock_ms），永不读取
ContextSummary.total_duration_ms。证据只提交聚合值 + 匿名 corpus hash + 硬件/build；
禁止真实路径/文件名/正文（顶层 work_dir 除外）。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
import sqlite3
import subprocess
import sys
import time
import uuid
from datetime import date
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))

# 复用 Plan 3 种子/snapshot 基础设施（spec Part 6）。这些函数不做 pass/fail 判定，
# 只提供场景/profile/进程/harness 帮助。
from benchmark_snapshot_warm import (  # noqa: E402
    REAL_WORK_DIR,
    SEVEN_D_END,
    SEVEN_D_PROFILE,
    SEVEN_D_REPORT_MODE,
    SEVEN_D_START,
    _adapter_payload,
    _build_binary_paths,
    _fresh_dir,
    _hardware_evidence,
    _version_evidence,
    anonymous_corpus_hash,
    capture_context_state,
    checkpoint_db,
    median_ms,
    request_id_exists,
    run_build_context,
    run_inspect_v2,
    semantic_key,
)
from benchmark_harness import wall_clock_ms  # noqa: E402
from benchmark_seed_preparer import (  # noqa: E402
    SeedPreparerError,
    copy_sqlite_db,
    prepare_cache_only_seed,
    sha256_file,
    verify_cache_only_seed_marker,
)

# ---------------------------------------------------------------------------
# 场景与门槛（spec Part 8.1 / 9.2 / 6）
# ---------------------------------------------------------------------------

# 30d/90d cold 门槛：median/max 以 wall-clock ms 计。
COLD_TARGETS = {
    "30d": {"median_ms_le": 20_000.0, "max_ms_le": 25_000.0},
    "90d": {"median_ms_le": 40_000.0, "max_ms_le": 50_000.0},
}
SEVEN_D_TARGETS = {"median_ms_le": 370.0, "max_ms_le": 420.0}
MIN_WARM_IMPROVEMENT = 0.20

# 上次真实目录 evidence（.artifacts/snapshot-warm/evidence.json）中的匿名 manifest hash。
PREVIOUS_MANIFEST_HASHES = {
    "30d": "99f1d4e36e2567b7429a4019a9677ec5c2fb9053dbbce23138ea020052bc70a3",
    "90d": "c4fb4d457f1d2ddc8622e6ee1edeabc817c718948b144f475fb97d0ebf180037",
    "7d": "bfbcc7759e0cce2bf182023e73a0b12fa94be147a703b0648dfa053fe3f07e88",
}


def _monthly_profile(
    *,
    deadline_ms: int,
    candidates: int,
    classification_pages: int,
    extractions: int,
) -> dict:
    # spec Part 8.1/9.2：monthly 真实目录显式 summary_pdf_max_pages=5
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
        "deadline_ms": 25000,
        "quota_nominal": {
            "max_candidate_files": 384,
            "max_total_pdf_classification_pages": 370,
            "max_pdf_text_extractions": 16,
            "total_deadline_ms": 25000,
        },
    },
    "90d": {
        "start": date(2026, 5, 11),
        "end": date(2026, 8, 8),
        "report_mode": "monthly",
        "profile": _monthly_profile(
            deadline_ms=45000, candidates=600, classification_pages=800, extractions=32
        ),
        "deadline_ms": 45000,
        "quota_nominal": {
            "max_candidate_files": 600,
            "max_total_pdf_classification_pages": 800,
            "max_pdf_text_extractions": 32,
            "total_deadline_ms": 45000,
        },
    },
}

# spec Part 8.1：session 默认（profile 未显式给出时）。
SESSION_PARAMS_DEFAULT = {
    "session_concurrency": "min(max_workers,4)",
    "max_requests_per_session": 128,
    "session_idle_ttl_ms": 30_000,
    "session_rss_limit_bytes": 536_870_912,
}


# ---------------------------------------------------------------------------
# DB 派生反作弊证据（inspect v2 对冷 run 的逐文件 pdf_classification 为 null，
# 因此分类事实从隔离 DB 的 classification_cache / context_decisions 派生）
# ---------------------------------------------------------------------------


def _derive_sample_evidence(
    db_path: Path,
    scan_run_id: int,
    metrics: dict,
    *,
    session_capability_present: bool,
    validated: bool,
) -> dict:
    """从隔离 DB + inspect 派生反作弊断言所需的全部数值。不写入真实路径。"""
    conn = sqlite3.connect(f"file:{db_path.resolve().as_posix()}?mode=ro", uri=True)
    try:
        summary = conn.execute(
            "SELECT error_file_count, timeout_count, included_file_count,"
            " source_file_count, success_count, omitted_file_count"
            " FROM context_runs WHERE scan_run_id=?",
            (scan_run_id,),
        ).fetchone()
        if summary is None:
            raise RuntimeError(f"context_runs row missing for scan_run_id={scan_run_id}")
        error_count, timeout_count, included_count, source_count, success_count, omitted_count = summary

        runtime_not_parsed_count = conn.execute(
            "SELECT count(*) FROM context_decisions cd"
            " JOIN context_runs cr USING (context_run_id)"
            " WHERE cr.scan_run_id=? AND cd.reason='runtime_deadline_exhausted'",
            (scan_run_id,),
        ).fetchone()[0]

        # unknown：PDF 文件在本轮 parse 中出现 error/timeout（classifier unknown/
        # crash/timeout 或 PDF parse 失败都表现为 PDF 行 error/timeout）。
        unknown_count = conn.execute(
            "SELECT count(*) FROM scan_file_results sfr"
            " JOIN file_inventory fi USING (file_identity)"
            " WHERE sfr.scan_run_id=? AND fi.file_type='.pdf'"
            " AND sfr.parse_status IN ('error','timeout')",
            (scan_run_id,),
        ).fetchone()[0]

        # text_pdf_coverage：admitted text PDF（= extraction_slot_count）中成功提取
        # 且进入正文（action keep/compress）的占比。冷 run 全为 parse-cache miss。
        extraction_slot_count = int(metrics.get("extraction_slot_count", 0) or 0)
        text_extracted_success = conn.execute(
            "SELECT count(DISTINCT cd.file_identity) FROM context_decisions cd"
            " JOIN context_runs cr USING (context_run_id)"
            " JOIN classification_cache cc USING (file_identity)"
            " WHERE cr.scan_run_id=? AND cc.status='text_in_parse_window'"
            " AND cd.action IN ('keep','compress')",
            (scan_run_id,),
        ).fetchone()[0]
        if extraction_slot_count > 0:
            text_pdf_coverage = text_extracted_success / extraction_slot_count
        else:
            text_pdf_coverage = 1.0

        # no-text pdfplumber：no-text PDF 实际走 pdf_text_v1（真正调用 pdfplumber）
        # 的次数。正确行为应全为 pdf_metadata_v1 元数据草稿。注意：decision 的
        # action 标签可能错误地把元数据草稿标为 keep（decision.rs 按 size/content
        # 重算 action，未保留 pdf_no_text_in_parse_window reason），但这不等于
        # 调用了 pdfplumber；pdfplumber 只由 pdf_text_v1 backend 触发。
        no_text_pdfplumber_invocations = conn.execute(
            "SELECT count(DISTINCT sfr.file_identity) FROM scan_file_results sfr"
            " JOIN file_inventory fi USING (file_identity)"
            " JOIN classification_cache cc USING (file_identity)"
            " WHERE sfr.scan_run_id=? AND cc.status='no_text_in_parse_window'"
            " AND sfr.parser_backend='pdf_text_v1'",
            (scan_run_id,),
        ).fetchone()[0]
        # 记录 decision action 标签异常（no-text 元数据草稿被标为 keep/compress），
        # 这是正确性观察项，不触发 pdfplumber。
        no_text_action_label_anomaly_count = conn.execute(
            "SELECT count(DISTINCT cd.file_identity) FROM context_decisions cd"
            " JOIN context_runs cr USING (context_run_id)"
            " JOIN classification_cache cc USING (file_identity)"
            " WHERE cr.scan_run_id=? AND cc.status='no_text_in_parse_window'"
            " AND cd.action IN ('keep','compress')",
            (scan_run_id,),
        ).fetchone()[0]

        # 各阶段耗时（scan_stage_metrics 只含 discovery/cache/parse/context）。
        stage_durations_ms = {}
        for stage, item_count, duration_ms in conn.execute(
            "SELECT stage, item_count, duration_ms FROM scan_stage_metrics"
            " WHERE scan_run_id=? ORDER BY stage",
            (scan_run_id,),
        ).fetchall():
            stage_durations_ms[stage] = {"item_count": item_count, "duration_ms": duration_ms}

        # cache state：parse/classification cache + inventory 行数（隔离 DB 本轮生成）。
        parse_cache_rows = conn.execute("SELECT count(*) FROM parse_cache").fetchone()[0]
        classification_cache_rows = conn.execute(
            "SELECT count(*) FROM classification_cache"
        ).fetchone()[0]
        inventory_rows = conn.execute("SELECT count(*) FROM file_inventory").fetchone()[0]
        context_profile_hash = conn.execute(
            "SELECT context_profile_hash FROM context_runs WHERE scan_run_id=?",
            (scan_run_id,),
        ).fetchone()[0]
    finally:
        conn.close()

    evidence = {
        "validated": bool(validated),
        "stage_deadline_exhausted_count": int(metrics.get("stage_deadline_exhausted_count", 0) or 0),
        "runtime_not_parsed_count": int(runtime_not_parsed_count),
        "unknown_count": int(unknown_count),
        "error_count": int(error_count),
        "timeout_count": int(timeout_count),
        "text_pdf_coverage": text_pdf_coverage,
        "no_text_pdfplumber_invocations": int(no_text_pdfplumber_invocations),
        "no_text_action_label_anomaly_count": int(no_text_action_label_anomaly_count),
        "source_guard_unavailable_count": int(metrics.get("source_guard_unavailable_count", 0) or 0),
        "session_capability_present": bool(session_capability_present),
        "session_fallback_count": int(metrics.get("session_fallback_count", 0) or 0),
        "context_profile_hash": context_profile_hash,
        "summary": {
            "source_file_count": int(source_count),
            "success_count": int(success_count),
            "included_file_count": int(included_count),
            "omitted_file_count": int(omitted_count),
            "error_file_count": int(error_count),
            "timeout_count": int(timeout_count),
        },
        "metrics": {
            "candidate_file_count": int(metrics.get("candidate_file_count", 0) or 0),
            "classification_slot_count": int(metrics.get("classification_slot_count", 0) or 0),
            "admitted_file_count": int(metrics.get("admitted_file_count", 0) or 0),
            "extraction_slot_count": extraction_slot_count,
            "nominal_charged_pages_total": int(metrics.get("nominal_charged_pages_total", 0) or 0),
            "confirmed_run_inspected_pages_total": int(
                metrics.get("confirmed_run_inspected_pages_total", 0) or 0
            ),
            "unobserved_classification_attempt_count": int(
                metrics.get("unobserved_classification_attempt_count", 0) or 0
            ),
            "pdfplumber_invocations": int(metrics.get("pdfplumber_invocations", 0) or 0),
            "classify_attempt_count": int(metrics.get("classify_attempt_count", 0) or 0),
            "parse_attempt_count": int(metrics.get("parse_attempt_count", 0) or 0),
            "parse_cache_lookup_count": int(metrics.get("parse_cache_lookup_count", 0) or 0),
            "classification_cache_lookup_count": int(
                metrics.get("classification_cache_lookup_count", 0) or 0
            ),
            "parse_cache_all_hit": metrics.get("parse_cache_all_hit"),
            "classification_cache_all_hit": metrics.get("classification_cache_all_hit"),
            "session_restart_count": int(metrics.get("session_restart_count", 0) or 0),
            "source_guard_content_hash_file_count": int(
                metrics.get("source_guard_content_hash_file_count", 0) or 0
            ),
            "source_guard_bytes_read": int(metrics.get("source_guard_bytes_read", 0) or 0),
            "reserved_chars": int(metrics.get("reserved_chars", 0) or 0),
            "rendered_chars": int(metrics.get("rendered_chars", 0) or 0),
            "peak_worker_rss_bytes": metrics.get("peak_worker_rss_bytes"),
            "stage_durations_ms": stage_durations_ms,
            "deadline_precommit_elapsed_ms": int(
                metrics.get("deadline_precommit_elapsed_ms", 0) or 0
            ),
        },
        "quota_actual": {
            "candidate_file_count": int(metrics.get("candidate_file_count", 0) or 0),
            "nominal_charged_pages_total": int(
                metrics.get("nominal_charged_pages_total", 0) or 0
            ),
            "extraction_slot_count": extraction_slot_count,
        },
        "cache_state": {
            "parse_cache_rows": int(parse_cache_rows),
            "classification_cache_rows": int(classification_cache_rows),
            "file_inventory_rows": int(inventory_rows),
        },
    }
    return evidence


def _assert_sample(sample: dict) -> None:
    """反作弊断言（brief Task 3）。任一失败抛 AssertionError，该样本失败。"""
    ac = sample["anti_cheat"]
    assert ac["stage_deadline_exhausted_count"] == 0, (
        f"stage_deadline_exhausted_count={ac['stage_deadline_exhausted_count']}"
    )
    assert ac["runtime_not_parsed_count"] == 0, (
        f"runtime_not_parsed_count={ac['runtime_not_parsed_count']}"
    )
    assert ac["unknown_count"] == 0, f"unknown_count={ac['unknown_count']}"
    assert ac["error_count"] == 0, f"error_count={ac['error_count']}"
    assert ac["timeout_count"] == 0, f"timeout_count={ac['timeout_count']}"
    assert ac["text_pdf_coverage"] == 1.0, f"text_pdf_coverage={ac['text_pdf_coverage']}"
    assert ac["no_text_pdfplumber_invocations"] == 0, (
        f"no_text_pdfplumber_invocations={ac['no_text_pdfplumber_invocations']}"
    )
    assert ac["source_guard_unavailable_count"] == 0, (
        f"source_guard_unavailable_count={ac['source_guard_unavailable_count']}"
    )
    assert ac["session_capability_present"] is True, "session_capability_present is False"
    assert ac["session_fallback_count"] == 0, f"session_fallback_count={ac['session_fallback_count']}"
    assert ac["validated"] is True, "validated is False"


# ---------------------------------------------------------------------------
# 会话能力探测（build 级事实）
# ---------------------------------------------------------------------------


def _version_evidence_v2(scanner: Path) -> dict:
    """scanner version 的 v2 投影（spec Part 5.3：新能力只由 v2 发布）。"""
    payload: dict = {}

    def validator(raw: bytes) -> dict:
        parsed = json.loads(raw)
        if not isinstance(parsed, dict):
            raise ValueError("version response is not a JSON object")
        payload.update(parsed)
        return parsed

    wall_clock_ms(
        [str(scanner), "version", "--response-version", "2"],
        b"",
        response_validator=validator,
    )
    return payload


def _probe_session_capability(scanner: Path) -> dict:
    """session capability present：scanner 宣告（v2）+ Python worker 支持 session-version。"""
    advertised = False
    try:
        version = _version_evidence_v2(scanner)
        versions = version.get("session_contract_versions") or []
        advertised = "ai_daily_python_session_v1" in versions
    except Exception:  # noqa: BLE001 - evidence must be collected
        advertised = False

    worker_supports = False
    try:
        result = subprocess.run(
            [
                str(Path(sys.executable).resolve()),
                "-m",
                "src.workers.document_parser_worker",
                "session-version",
            ],
            cwd=str(PROJECT_ROOT),
            capture_output=True,
            timeout=60,
        )
        if result.returncode == 0:
            payload = json.loads(result.stdout.decode("utf-8"))
            worker_supports = (
                payload.get("contract") == "ai_daily_python_session"
                and payload.get("session_contract_version") == "ai_daily_python_session_v1"
            )
    except Exception:  # noqa: BLE001
        worker_supports = False

    return {
        "scanner_advertises_session_contract": advertised,
        "python_worker_supports_session_version": worker_supports,
        "session_capability_present": advertised and worker_supports,
        "session_params_default": SESSION_PARAMS_DEFAULT,
    }


def _probe_classifier_build(scanner: Path) -> dict:
    try:
        version = _version_evidence_v2(scanner)
        return {
            "classifier_contract_versions": version.get("classifier_contract_versions") or [],
            "source_guard_policy": version.get("source_guard_policy"),
            "max_source_files_per_run": version.get("max_source_files_per_run"),
        }
    except Exception:  # noqa: BLE001
        return {}


def _probe_worker_builds(db_path: Path, scan_run_id: int) -> dict:
    """从 scan_run_attempts 读取 worker/engine fingerprints（不含路径）。"""
    conn = sqlite3.connect(f"file:{db_path.resolve().as_posix()}?mode=ro", uri=True)
    try:
        row = conn.execute(
            "SELECT engine_fingerprint, office_worker_build, python_worker_build"
            " FROM scan_run_attempts WHERE scan_run_id=? ORDER BY attempt_number LIMIT 1",
            (scan_run_id,),
        ).fetchone()
    finally:
        conn.close()
    if row is None:
        return {}
    return {
        "engine_fingerprint": row[0],
        "office_worker_build": row[1],
        "python_worker_build": row[2],
    }


# ---------------------------------------------------------------------------
# 30d / 90d 手工 acceptance（3 独立 cold DB + warm 对比）
# ---------------------------------------------------------------------------


def run_cold_acceptance_and_warm(
    *,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    label: str,
    out_root: Path,
    session_capability: dict,
) -> dict:
    scenario = SCENARIOS[label]
    start = scenario["start"]
    end = scenario["end"]
    report_mode = scenario["report_mode"]
    profile = scenario["profile"]
    deadline_ms = scenario["deadline_ms"]
    target = COLD_TARGETS[label]

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
        metrics: dict = {}
        context = None
        if run["scan_run_id"] is not None:
            inspect_result = run_inspect_v2(scanner, cold_db, run["scan_run_id"])
            if inspect_result and inspect_result["ok"]:
                metrics = inspect_result["payload"].get("execution_metrics") or {}
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

    # 每个 cold 样本的 wall/证据/反作弊。
    cold_samples: list[dict] = []
    for index, cold in enumerate(colds):
        run = cold["run"]
        inspect_ok = bool(cold["inspect_result"] and cold["inspect_result"]["ok"])
        evidence = _derive_sample_evidence(
            cold["db"],
            run["scan_run_id"],
            cold["metrics"],
            session_capability_present=session_capability["session_capability_present"],
            validated=run["validated"] and inspect_ok,
        )
        sample = {
            "index": index,
            "wall_ms": run["wall_ms"],
            "status": run["status"],
            "scan_run_id": run["scan_run_id"],
            "inspect_ok": inspect_ok,
            "inspect_error": None
            if cold["inspect_result"] is None
            else cold["inspect_result"]["error"],
            "anti_cheat": evidence,
            "deadline_ms": deadline_ms,
        }
        try:
            _assert_sample(sample)
            sample["anti_cheat_passed"] = True
        except AssertionError as error:
            sample["anti_cheat_passed"] = False
            sample["anti_cheat_failure"] = str(error)
        cold_samples.append(sample)

    cold_ms = [s["wall_ms"] for s in cold_samples]
    cold_median = median_ms(cold_ms)
    cold_max = max(cold_ms)

    corpus_conn = sqlite3.connect(colds[0]["db"])
    try:
        corpus = anonymous_corpus_hash(corpus_conn)
    finally:
        corpus_conn.close()

    gates = {
        "all_samples_anti_cheat_passed": all(s["anti_cheat_passed"] for s in cold_samples),
        "all_samples_status_ok": all(s["status"] == "ok" for s in cold_samples),
        "cold_median_le_target": cold_median <= target["median_ms_le"],
        "cold_max_le_target": cold_max <= target["max_ms_le"],
        "passes": False,
    }
    gates["passes"] = all(
        [gates["all_samples_anti_cheat_passed"], gates["all_samples_status_ok"], gates["cold_median_le_target"], gates["cold_max_le_target"]]
    )

    result: dict = {
        "window": {"start": start.isoformat(), "end": end.isoformat()},
        "report_mode": report_mode,
        "profile": profile,
        "profile_hash_30d_sample": cold_samples[0]["anti_cheat"]["context_profile_hash"]
        if cold_samples
        else None,
        "corpus": corpus,
        "corpus_changed_since_previous": corpus["anonymous_manifest_sha256"]
        != PREVIOUS_MANIFEST_HASHES[label],
        "cold_deadline_ms": deadline_ms,
        "cold_target": target,
        "cold_wall_ms": cold_ms,
        "cold_median_ms": cold_median,
        "cold_max_ms": cold_max,
        "cold_samples": cold_samples,
        "cold_gates": gates,
    }

    # warm 对比：只有 3 个 cold 全部 clean 才做（Part 6/9.2）。
    warm = None
    if gates["passes"]:
        warm = run_warm_comparison(
            scanner=scanner,
            office_worker=office_worker,
            work_dir=work_dir,
            label=label,
            out_root=out_root,
            colds=colds,
        )
    result["warm_comparison"] = warm

    return result


def run_warm_comparison(
    *,
    scanner: Path,
    office_worker: Path,
    work_dir: Path,
    label: str,
    out_root: Path,
    colds: list[dict],
) -> dict:
    """snapshot warm vs cache-only warm（Part 6：seed clone + marker，无 bypass）。"""
    scenario = SCENARIOS[label]
    start = scenario["start"]
    end = scenario["end"]
    report_mode = scenario["report_mode"]
    profile = scenario["profile"]

    seed_out = out_root / f"{label}_seed"
    if seed_out.exists():
        shutil.rmtree(seed_out)
    seed_marker = out_root / f"{label}_seed.marker.json"
    seed = prepare_cache_only_seed(src=colds[0]["db"], out_dir=seed_out, marker=seed_marker)

    # cache-only warm：3 samples，各从只读 seed 克隆新 DB。
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
            inspect_result["payload"].get("execution_metrics") or {}
            if inspect_result["ok"]
            else {}
        )
        cache_samples.append(
            {
                "index": index,
                "wall_ms": run["wall_ms"],
                "scan_run_id": run["scan_run_id"],
                "idempotent_replay_false": idempotent_probe["request_id_absent_before_run"],
                "snapshot_hit": bool(metrics.get("snapshot_hit")),
                "parse_cache_lookup_count": metrics.get("parse_cache_lookup_count"),
                "classification_cache_lookup_count": metrics.get(
                    "classification_cache_lookup_count"
                ),
                "parse_cache_all_hit": metrics.get("parse_cache_all_hit"),
                "classification_cache_all_hit": metrics.get("classification_cache_all_hit"),
                "inspect_error": inspect_result["error"],
                "context_sha256": context["context_sha256"],
                "semantic": context,
            }
        )

    # snapshot warm：cold[1] 的 DB 上，3 个新 request_id 连续运行。
    snap_cold = colds[1]
    snap_samples: list[dict] = []
    seen_run_ids = {snap_cold["run"]["scan_run_id"]}
    for index in range(3):
        request_id = str(uuid.uuid4())
        idempotent_probe = {
            "request_id_absent_before_run": not request_id_exists(snap_cold["db"], request_id),
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
            inspect_result["payload"].get("execution_metrics") or {}
            if inspect_result["ok"]
            else {}
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

    cache_ms = [s["wall_ms"] for s in cache_samples]
    snap_ms = [s["wall_ms"] for s in snap_samples]
    cache_median = median_ms(cache_ms)
    snap_median = median_ms(snap_ms)
    improvement = 0.0
    if cache_median > 0:
        improvement = 1.0 - snap_median / cache_median

    all_contexts = (
        [s["semantic"] for s in cache_samples]
        + [s["semantic"] for s in snap_samples]
        + [cold["context"] for cold in colds]
    )
    semantic_identical = len({semantic_key(context) for context in all_contexts}) == 1

    cache_warm_ok = all(
        s["snapshot_hit"] is False
        and s["parse_cache_lookup_count"] > 0
        and s["classification_cache_lookup_count"] > 0
        and s["parse_cache_all_hit"] is True
        and s["classification_cache_all_hit"] is True
        for s in cache_samples
    )
    snap_warm_ok = all(
        s["snapshot_hit"] is True and s["idempotent_replay_false"] for s in snap_samples
    )

    gates = {
        "snapshot_warm_median_improvement_ge_20pct": improvement >= MIN_WARM_IMPROVEMENT,
        "cache_warm_all_snapshot_miss_and_cache_all_hit": cache_warm_ok,
        "snapshot_warm_all_snapshot_hit": snap_warm_ok,
        "semantic_identical": semantic_identical,
        "passes": all(
            [
                improvement >= MIN_WARM_IMPROVEMENT,
                cache_warm_ok,
                snap_warm_ok,
                semantic_identical,
            ]
        ),
    }

    return {
        "warm_comparison": "completed",
        "seed": {
            "source_sha256": seed["source_sha256"],
            "seed_sha256": seed["seed_sha256"],
            "nonce_prefix": seed["nonce"][:8],
            "cold_cache_state": seed["cold_cache_state"],
        },
        "cache_only_warm_wall_ms": cache_ms,
        "cache_only_warm_median_ms": cache_median,
        "cache_only_warm_samples": _strip_semantic_from_samples(cache_samples),
        "snapshot_warm_wall_ms": snap_ms,
        "snapshot_warm_median_ms": snap_median,
        "snapshot_warm_max_ms": max(snap_ms),
        "snapshot_warm_samples": _strip_semantic_from_samples(snap_samples),
        "improvement": improvement,
        "gates": gates,
    }


def _strip_semantic_from_samples(samples: list[dict]) -> list[dict]:
    """从证据中剥离 `semantic`（含 decisions 的 file_identity 哈希集合，避免体积与
    不必要的内部细节）。scope：证据只留聚合值。"""
    return [{k: v for k, v in sample.items() if k != "semantic"} for sample in samples]


# ---------------------------------------------------------------------------
# 7d snapshot warm 复测（Part 6：3 个新 request_id，median<=370ms/max<=420ms）
# ---------------------------------------------------------------------------


def run_7d_snapshot_warm_recheck(
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

    cold = run_build_context(
        scanner=scanner,
        office_worker=office_worker,
        work_dir=work_dir,
        db_path=cold_db,
        request_id=str(uuid.uuid4()),
        start=SEVEN_D_START,
        end=SEVEN_D_END,
        report_mode=report_mode,
        profile=profile,
    )
    checkpoint_db(cold_db)
    cold_inspect = None
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
    cold_clean = cold["status"] == "ok" and cold_context is not None

    if not cold_clean:
        return {
            "window": {"start": SEVEN_D_START.isoformat(), "end": SEVEN_D_END.isoformat()},
            "report_mode": report_mode,
            "corpus": corpus,
            "corpus_changed_since_previous": corpus["anonymous_manifest_sha256"]
            != PREVIOUS_MANIFEST_HASHES["7d"],
            "cold_wall_ms": cold["wall_ms"],
            "cold_status": cold["status"],
            "cold_inspect_ok": bool(cold_inspect and cold_inspect["ok"]),
            "cold_inspect_error": None if cold_inspect is None else cold_inspect["error"],
            "samples": [],
            "snapshot_warm_wall_ms": [],
            "snapshot_warm_median_ms": 0.0,
            "snapshot_warm_max_ms": 0.0,
            "gates": {
                "cold_runs_clean": False,
                "median_le_370ms": False,
                "max_le_420ms": False,
                "all_snapshot_hit": False,
                "all_idempotent_replay_false": False,
                "all_context_identical_to_cold": False,
                "passes": False,
            },
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
        metrics = (
            inspect_result["payload"].get("execution_metrics") or {}
            if inspect_result["ok"]
            else {}
        )
        samples.append(
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
                "context_hash_identical": context["context_sha256"]
                == cold_context["context_sha256"],
            }
        )
        seen_run_ids.add(run["scan_run_id"])

    warm_ms = [s["wall_ms"] for s in samples]
    gates = {
        "cold_runs_clean": True,
        "median_le_370ms": median_ms(warm_ms) <= SEVEN_D_TARGETS["median_ms_le"],
        "max_le_420ms": max(warm_ms) <= SEVEN_D_TARGETS["max_ms_le"],
        "all_snapshot_hit": all(s["snapshot_hit"] for s in samples),
        "all_idempotent_replay_false": all(s["idempotent_replay_false"] for s in samples),
        "all_context_identical_to_cold": all(s["context_hash_identical"] for s in samples),
        "passes": False,
    }
    gates["passes"] = all(
        [
            gates["cold_runs_clean"],
            gates["median_le_370ms"],
            gates["max_le_420ms"],
            gates["all_snapshot_hit"],
            gates["all_idempotent_replay_false"],
            gates["all_context_identical_to_cold"],
        ]
    )

    return {
        "window": {"start": SEVEN_D_START.isoformat(), "end": SEVEN_D_END.isoformat()},
        "report_mode": report_mode,
        "corpus": corpus,
        "corpus_changed_since_previous": corpus["anonymous_manifest_sha256"]
        != PREVIOUS_MANIFEST_HASHES["7d"],
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
# CLI 入口
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="真实目录手工 acceptance（spec Part 9.2 / Plan 4 Task 3）。"
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
        default=PROJECT_ROOT / ".artifacts" / "acceptance-real-dir",
        help="证据 + 临时 DB 根目录",
    )
    parser.add_argument(
        "--evidence",
        type=Path,
        default=PROJECT_ROOT / ".artifacts" / "acceptance-real-dir.json",
        help="聚合证据 JSON 输出路径",
    )
    parser.add_argument(
        "--only",
        choices=["7d", "30d", "90d", "30d,90d", "7d,30d,90d"],
        default="7d,30d,90d",
        help="运行哪些场景",
    )
    args = parser.parse_args(argv)

    work_dir = args.work_dir.resolve()
    if not work_dir.is_dir():
        parser.error(f"work_dir {work_dir} is not a directory")
    out_root = args.out_root.resolve()
    out_root.mkdir(parents=True, exist_ok=True)

    scanner, office_worker = _build_binary_paths()
    session_capability = _probe_session_capability(scanner)

    results: dict = {
        "schema": "acceptance_real_dir_v1",
        "generated_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "hardware": _hardware_evidence(),
        "build": {
            "scanner_sha256": sha256_file(scanner),
            "office_worker_sha256": sha256_file(office_worker),
            "session_capability": session_capability,
        },
        "corpus": {"work_dir": str(work_dir)},
        "os_page_cache_declared_not_cleared": True,
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
            results["build"]["classifier"] = _probe_classifier_build(scanner)
    except Exception:  # noqa: BLE001
        results["build"]["engine_version"] = "unavailable"

    selected = [part for part in args.only.split(",") if part]
    for scenario in selected:
        try:
            if scenario == "7d":
                result = run_7d_snapshot_warm_recheck(
                    scanner=scanner,
                    office_worker=office_worker,
                    work_dir=work_dir,
                    out_root=out_root,
                )
            else:
                result = run_cold_acceptance_and_warm(
                    scanner=scanner,
                    office_worker=office_worker,
                    work_dir=work_dir,
                    label=scenario,
                    out_root=out_root,
                    session_capability=session_capability,
                )
        except SeedPreparerError as error:
            print(f"seed preparer failed closed: {error}")
            return 1
        except Exception as error:  # noqa: BLE001 - evidence must be collected
            results["scenarios"][scenario] = {"error": str(error)}
            print(f"scenario {scenario} failed: {error}")
            continue
        results["scenarios"][scenario] = result

    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(
        json.dumps(results, ensure_ascii=False, indent=2, default=str), encoding="utf-8"
    )
    print("=== acceptance-real-dir (aggregate) ===")
    print(json.dumps(results, ensure_ascii=False, indent=2, default=str))

    failed: list[str] = []
    for scenario, result in results["scenarios"].items():
        if "error" in result:
            failed.append(scenario)
            continue
        gates = result.get("gates") or result.get("cold_gates")
        if gates is not None and not gates.get("passes", False):
            failed.append(scenario)
        warm = result.get("warm_comparison")
        if isinstance(warm, dict) and warm.get("gates") and not warm["gates"].get("passes", False):
            failed.append(f"{scenario}:warm")

    if failed:
        print("=== STOP-GATE TRIGGERED ===")
        for scenario in failed:
            result = results["scenarios"].get(scenario.split(":")[0], {})
            if "error" in result:
                print(f"{scenario}: error={result['error']}")
            elif "cold_gates" in result:
                print(f"{scenario}: cold_gates={json.dumps(result['cold_gates'])}")
                if isinstance(result.get("warm_comparison"), dict):
                    print(f"{scenario}: warm_gates={json.dumps(result['warm_comparison']['gates'])}")
            else:
                print(f"{scenario}: gates={json.dumps(result.get('gates', {}))}")
        return 2
    print("=== ALL ACCEPTANCE GATES PASS ===")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
