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
- text_pdf_coverage == 1.0；no-text pdfplumber/action anomaly == 0
- source_guard_unavailable_count == 0；session capability present；session_fallback_count == 0
- guard metrics 完整非负；normalized profile/build/corpus 跨样本完全一致且 corpus 未漂移
- validated == true

pass/fail 只读 harness `benchmark_wall_ms`（wall_clock_ms），永不读取
ContextSummary.total_duration_ms。证据只提交聚合值 + 匿名 corpus hash + 硬件/build；
禁止真实路径/文件名/正文。
"""
from __future__ import annotations

import argparse
import json
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
    ANONYMOUS_CORPUS_MANIFEST_PROVENANCE,
    REAL_WORK_DIR,
    SEVEN_D_END,
    SEVEN_D_PROFILE,
    SEVEN_D_REPORT_MODE,
    SEVEN_D_START,
    _build_binary_paths,
    _fresh_dir,
    _hardware_evidence,
    _version_evidence,
    anonymous_corpus_hash,
    anonymous_corpus_manifest_complete,
    assert_portable_evidence as _assert_portable_evidence,
    build_identity_complete as _build_identity_complete,
    capture_context_state,
    capture_run_reproducibility,
    checkpoint_db,
    median_ms,
    normalized_profile_evidence_complete,
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


def _source_guard_evidence(metrics: dict, policy: str | None) -> dict:
    required = (
        "discovery_observed_file_count",
        "source_guard_content_hash_file_count",
        "source_guard_unavailable_count",
        "source_guard_bytes_read",
    )
    metrics_complete = all(
        key in metrics
        and isinstance(metrics[key], int)
        and not isinstance(metrics[key], bool)
        and metrics[key] >= 0
        for key in required
    )
    observed = int(metrics.get("discovery_observed_file_count", 0) or 0)
    content_hash = int(metrics.get("source_guard_content_hash_file_count", 0) or 0)
    unavailable = int(metrics.get("source_guard_unavailable_count", 0) or 0)
    metadata = observed - content_hash - unavailable
    if metadata < 0:
        metrics_complete = False
        metadata = 0
    metadata_kind = (
        "windows_file_id_change_time_v1"
        if sys.platform == "win32"
        else "unix_inode_ctime_v1"
    )
    return {
        "policy": policy,
        "metrics_complete": metrics_complete,
        "discovery_observed_file_count": observed,
        "kind_counts": {
            metadata_kind: metadata,
            "content_sha256_v1": content_hash,
            "unavailable": unavailable,
        },
        "content_hash_bytes_read": int(metrics.get("source_guard_bytes_read", 0) or 0),
    }


def _failed_sample_evidence(
    *,
    session_capability_present: bool,
    source_guard_policy: str | None,
    collection_error_code: str,
) -> dict:
    """Return the same top-level evidence shape without exception/path text."""
    return {
        "validated": False,
        "collection_error_code": collection_error_code,
        "stage_deadline_exhausted_count": None,
        "runtime_not_parsed_count": None,
        "unknown_count": None,
        "error_count": None,
        "timeout_count": None,
        "text_pdf_coverage": None,
        "no_text_pdfplumber_invocations": None,
        "no_text_action_label_anomaly_count": None,
        "source_guard_unavailable_count": None,
        "session_capability_present": bool(session_capability_present),
        "session_fallback_count": None,
        "context_profile_hash": None,
        "normalized_profile_json": None,
        "normalized_profile_hash_algorithm": "sha256(sorted-key-json-utf8)",
        "normalized_profile_sha256": None,
        "snapshot_key_sha256": None,
        "build_identity": None,
        "build_cross_checks": {
            "engine_build_matches_attempt": False,
            "office_worker_build_matches_attempt": False,
            "python_worker_build_matches_attempt": False,
        },
        "source_guard": _source_guard_evidence({}, source_guard_policy),
        "summary": None,
        "metrics": None,
        "quota_actual": None,
        "cache_state": None,
    }


def _portable_response_error(value: object) -> dict | None:
    if not isinstance(value, dict):
        return None
    return {
        "error_code": value.get("error_code") or "SCANNER_ERROR",
        "retryable": bool(value.get("retryable")),
        "stage": value.get("stage"),
        "backend": value.get("backend"),
    }


def _cold_reproducibility_gates(
    cold_samples: list[dict],
    corpus_manifests: list[dict],
    *,
    frozen_manifest_sha256: str,
    expected_pdf_max_pages: int = 5,
) -> dict:
    """Cross-sample anti-drift gates for one three-cold acceptance scenario."""
    anti_cheat = [sample.get("anti_cheat") or {} for sample in cold_samples]
    corpus_keys = {
        (
            manifest.get("anonymous_manifest_sha256"),
            manifest.get("source_count"),
        )
        for manifest in corpus_manifests
    }
    profile_keys = {
        (
            sample.get("normalized_profile_json"),
            sample.get("normalized_profile_sha256"),
        )
        for sample in anti_cheat
    }
    build_keys = {
        json.dumps(sample.get("build_identity"), sort_keys=True, separators=(",", ":"))
        for sample in anti_cheat
    }
    profiles_complete = all(
        normalized_profile_evidence_complete(sample)
        for sample in anti_cheat
    )
    expected_profile = False
    if profiles_complete:
        try:
            expected_profile = all(
                json.loads(sample["normalized_profile_json"])
                .get("parse", {})
                .get("pdf", {})
                .get("max_pages")
                == expected_pdf_max_pages
                for sample in anti_cheat
            )
        except (AttributeError, json.JSONDecodeError):
            expected_profile = False

    gates = {
        "exactly_three_cold_samples": len(cold_samples) == 3,
        "exactly_three_corpus_manifests": len(corpus_manifests) == 3,
        "cold_corpus_manifests_complete": len(corpus_manifests) == 3
        and all(
            anonymous_corpus_manifest_complete(manifest)
            for manifest in corpus_manifests
        ),
        "cold_corpus_manifests_identical": len(corpus_manifests) == 3
        and len(corpus_keys) == 1,
        "corpus_matches_frozen_manifest": len(corpus_manifests) == 3
        and all(
            manifest.get("anonymous_manifest_sha256") == frozen_manifest_sha256
            for manifest in corpus_manifests
        ),
        "cold_normalized_profiles_complete": len(anti_cheat) == 3
        and profiles_complete,
        "cold_normalized_profiles_identical": len(anti_cheat) == 3
        and len(profile_keys) == 1,
        "normalized_pdf_max_pages_matches_scenario": expected_profile,
        "cold_build_identities_complete": len(anti_cheat) == 3
        and all(_build_identity_complete(sample.get("build_identity")) for sample in anti_cheat),
        "cold_build_identities_identical": len(anti_cheat) == 3
        and len(build_keys) == 1,
        "cold_build_cross_checks_passed": len(anti_cheat) == 3
        and all(
            sample.get("build_cross_checks")
            and all(sample["build_cross_checks"].values())
            for sample in anti_cheat
        ),
        "passes": False,
    }
    gates["passes"] = all(value for key, value in gates.items() if key != "passes")
    return gates


def _identity_evidence_consistent(evidence_rows: list[dict]) -> bool:
    if not evidence_rows:
        return False
    profile_keys = {
        (
            evidence.get("normalized_profile_json"),
            evidence.get("normalized_profile_sha256"),
        )
        for evidence in evidence_rows
    }
    build_keys = {
        json.dumps(
            evidence.get("build_identity"),
            sort_keys=True,
            separators=(",", ":"),
        )
        for evidence in evidence_rows
    }
    return (
        len(profile_keys) == 1
        and len(build_keys) == 1
        and all(
            normalized_profile_evidence_complete(evidence)
            and _build_identity_complete(evidence.get("build_identity"))
            and evidence.get("build_cross_checks")
            and all(evidence["build_cross_checks"].values())
            for evidence in evidence_rows
        )
    )


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
    source_guard_policy: str | None,
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
        (
            error_count,
            timeout_count,
            included_count,
            source_count,
            success_count,
            omitted_count,
        ) = summary

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
        run_provenance = capture_run_reproducibility(conn, scan_run_id)
    finally:
        conn.close()

    source_guard = _source_guard_evidence(metrics, source_guard_policy)
    evidence = {
        "validated": bool(validated),
        "collection_error_code": None,
        "stage_deadline_exhausted_count": int(
            metrics.get("stage_deadline_exhausted_count", 0) or 0
        ),
        "runtime_not_parsed_count": int(runtime_not_parsed_count),
        "unknown_count": int(unknown_count),
        "error_count": int(error_count),
        "timeout_count": int(timeout_count),
        "text_pdf_coverage": text_pdf_coverage,
        "no_text_pdfplumber_invocations": int(no_text_pdfplumber_invocations),
        "no_text_action_label_anomaly_count": int(no_text_action_label_anomaly_count),
        "source_guard_unavailable_count": int(
            metrics.get("source_guard_unavailable_count", 0) or 0
        ),
        "session_capability_present": bool(session_capability_present),
        "session_fallback_count": int(metrics.get("session_fallback_count", 0) or 0),
        "context_profile_hash": context_profile_hash,
        "normalized_profile_json": run_provenance["normalized_profile_json"],
        "normalized_profile_hash_algorithm": run_provenance[
            "normalized_profile_hash_algorithm"
        ],
        "normalized_profile_sha256": run_provenance[
            "normalized_profile_sha256"
        ],
        "snapshot_key_sha256": run_provenance["snapshot_key_sha256"],
        "build_identity": run_provenance["build_identity"],
        "build_cross_checks": run_provenance["build_cross_checks"],
        "source_guard": source_guard,
        "summary": {
            "source_file_count": int(source_count),
            "success_count": int(success_count),
            "included_file_count": int(included_count),
            "omitted_file_count": int(omitted_count),
            "error_file_count": int(error_count),
            "timeout_count": int(timeout_count),
        },
        "metrics": {
            "discovery_observed_file_count": int(
                metrics.get("discovery_observed_file_count", 0) or 0
            ),
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
            # spec Part 6: 持久化可重放的 wall-clock 拆分（handshake/discovery/
            # snapshot lookup/audit write/precommit/rebuild）。7d FAIL 的 handshake
            # 归属必须能从证据直接读出。
            "worker_handshake_ms": int(metrics.get("worker_handshake_ms", 0) or 0),
            "discovery_ms": int(metrics.get("discovery_ms", 0) or 0),
            "snapshot_lookup_ms": int(metrics.get("snapshot_lookup_ms", 0) or 0),
            "current_run_audit_write_ms": int(
                metrics.get("current_run_audit_write_ms", 0) or 0
            ),
            "terminal_precommit_ms": int(metrics.get("terminal_precommit_ms", 0) or 0),
            "envelope_rebuild_ms": int(metrics.get("envelope_rebuild_ms", 0) or 0),
            "terminal_rows_written": int(metrics.get("terminal_rows_written", 0) or 0),
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
    assert ac["stage_deadline_exhausted_count"] == 0, "STAGE_DEADLINE_EXHAUSTED"
    assert ac["runtime_not_parsed_count"] == 0, "RUNTIME_NOT_PARSED"
    assert ac["unknown_count"] == 0, "UNKNOWN_RESULT"
    assert ac["error_count"] == 0, "ERROR_RESULT"
    assert ac["timeout_count"] == 0, "TIMEOUT_RESULT"
    assert ac["text_pdf_coverage"] == 1.0, "TEXT_PDF_COVERAGE"
    assert ac["no_text_pdfplumber_invocations"] == 0, "NO_TEXT_PDFPLUMBER"
    assert ac["no_text_action_label_anomaly_count"] == 0, (
        "NO_TEXT_ACTION_LABEL_ANOMALY"
    )
    source_guard = ac["source_guard"]
    assert source_guard["policy"] == "source_guard_v2", "SOURCE_GUARD_POLICY"
    assert source_guard["metrics_complete"] is True, "SOURCE_GUARD_METRICS_INCOMPLETE"
    kind_counts = source_guard["kind_counts"]
    metadata_kind = (
        "windows_file_id_change_time_v1"
        if sys.platform == "win32"
        else "unix_inode_ctime_v1"
    )
    assert set(kind_counts) == {
        metadata_kind,
        "content_sha256_v1",
        "unavailable",
    }, "SOURCE_GUARD_KIND_SET_INVALID"
    assert all(
        isinstance(value, int) and not isinstance(value, bool) and value >= 0
        for value in kind_counts.values()
    ), "SOURCE_GUARD_KIND_COUNTS_INVALID"
    assert source_guard["content_hash_bytes_read"] >= 0, "SOURCE_GUARD_BYTES_INVALID"
    assert sum(kind_counts.values()) == source_guard["discovery_observed_file_count"], (
        "SOURCE_GUARD_COUNT_MISMATCH"
    )
    assert ac["source_guard_unavailable_count"] == 0, "SOURCE_GUARD_UNAVAILABLE"
    assert kind_counts["unavailable"] == 0, "SOURCE_GUARD_UNAVAILABLE_KIND"
    assert ac["session_capability_present"] is True, "SESSION_CAPABILITY_ABSENT"
    assert ac["session_fallback_count"] == 0, "SESSION_FALLBACK"
    assert normalized_profile_evidence_complete(ac), "PROFILE_EVIDENCE_INVALID"
    assert _build_identity_complete(ac["build_identity"]), "BUILD_IDENTITY_INCOMPLETE"
    assert all(ac["build_cross_checks"].values()), "BUILD_IDENTITY_MISMATCH"
    assert ac["validated"] is True, "SAMPLE_NOT_VALIDATED"


def _collect_sample_evidence(
    db_path: Path,
    scan_run_id: int | None,
    metrics: dict,
    *,
    session_capability_present: bool,
    source_guard_policy: str | None,
    validated: bool,
) -> dict:
    if scan_run_id is None:
        return _failed_sample_evidence(
            session_capability_present=session_capability_present,
            source_guard_policy=source_guard_policy,
            collection_error_code="EVIDENCE_COLLECTION_FAILED",
        )
    try:
        return _derive_sample_evidence(
            db_path,
            scan_run_id,
            metrics,
            session_capability_present=session_capability_present,
            source_guard_policy=source_guard_policy,
            validated=validated,
        )
    except (RuntimeError, sqlite3.Error, KeyError, TypeError, ValueError):
        return _failed_sample_evidence(
            session_capability_present=session_capability_present,
            source_guard_policy=source_guard_policy,
            collection_error_code="EVIDENCE_COLLECTION_FAILED",
        )


def _anti_cheat_verdict(evidence: dict) -> tuple[bool, str | None]:
    try:
        _assert_sample({"anti_cheat": evidence})
    except AssertionError as error:
        return False, str(error)
    return True, None


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
    source_guard_policy: str | None,
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
        corpus = {
            "anonymous_manifest_sha256": None,
            "source_count": None,
        }
        try:
            corpus_conn = sqlite3.connect(
                f"file:{cold_db.resolve().as_posix()}?mode=ro",
                uri=True,
            )
            try:
                corpus = anonymous_corpus_hash(corpus_conn)
            finally:
                corpus_conn.close()
        except sqlite3.Error:
            pass
        inspect_result = None
        metrics: dict = {}
        context = None
        if run["scan_run_id"] is not None:
            inspect_result = run_inspect_v2(scanner, cold_db, run["scan_run_id"])
            if (
                inspect_result
                and inspect_result["ok"]
                and run["status"] in {"ok", "partial"}
            ):
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
                "corpus_manifest": corpus,
            }
        )

    # 每个 cold 样本的 wall/证据/反作弊。
    cold_samples: list[dict] = []
    for index, cold in enumerate(colds):
        run = cold["run"]
        inspect_ok = bool(cold["inspect_result"] and cold["inspect_result"]["ok"])
        evidence = _collect_sample_evidence(
            cold["db"],
            run["scan_run_id"],
            cold["metrics"],
            session_capability_present=session_capability[
                "session_capability_present"
            ],
            source_guard_policy=source_guard_policy,
            validated=run["validated"] and inspect_ok,
        )
        sample = {
            "index": index,
            "wall_ms": run["wall_ms"],
            "status": run["status"],
            "response_error": _portable_response_error(run.get("response_error")),
            "scan_run_id": run["scan_run_id"],
            "inspect_ok": inspect_ok,
            "inspect_error_code": None if inspect_ok else "INSPECT_FAILED",
            "anti_cheat": evidence,
            "deadline_ms": deadline_ms,
        }
        passed, failure_code = _anti_cheat_verdict(evidence)
        sample["anti_cheat_passed"] = passed
        if failure_code is not None:
            sample["anti_cheat_failure_code"] = failure_code
        cold["anti_cheat"] = evidence
        cold_samples.append(sample)

    cold_ms = [s["wall_ms"] for s in cold_samples]
    cold_median = median_ms(cold_ms)
    cold_max = max(cold_ms)

    corpus_manifests = [cold["corpus_manifest"] for cold in colds]
    reproducibility_gates = _cold_reproducibility_gates(
        cold_samples,
        corpus_manifests,
        frozen_manifest_sha256=PREVIOUS_MANIFEST_HASHES[label],
    )
    gates = {
        **{key: value for key, value in reproducibility_gates.items() if key != "passes"},
        "all_samples_anti_cheat_passed": all(s["anti_cheat_passed"] for s in cold_samples),
        "all_samples_status_ok": all(s["status"] == "ok" for s in cold_samples),
        "cold_median_le_target": cold_median <= target["median_ms_le"],
        "cold_max_le_target": cold_max <= target["max_ms_le"],
        "passes": False,
    }
    gates["passes"] = all(value for key, value in gates.items() if key != "passes")

    first_evidence = cold_samples[0]["anti_cheat"] if cold_samples else {}
    corpus = corpus_manifests[0] if corpus_manifests else {
        "anonymous_manifest_sha256": None,
        "source_count": None,
    }

    result: dict = {
        "window": {"start": start.isoformat(), "end": end.isoformat()},
        "report_mode": report_mode,
        "profile": profile,
        "normalized_profile_json": first_evidence.get("normalized_profile_json"),
        "normalized_profile_hash_algorithm": first_evidence.get(
            "normalized_profile_hash_algorithm"
        ),
        "normalized_profile_sha256": first_evidence.get(
            "normalized_profile_sha256"
        ),
        "build_identity": first_evidence.get("build_identity"),
        "corpus": corpus,
        "corpus_manifest_provenance": ANONYMOUS_CORPUS_MANIFEST_PROVENANCE,
        "cold_corpus_manifests": corpus_manifests,
        "corpus_changed_since_previous": not gates[
            "corpus_matches_frozen_manifest"
        ],
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
            session_capability=session_capability,
            source_guard_policy=source_guard_policy,
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
    session_capability: dict,
    source_guard_policy: str | None,
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
            corpus_manifest = anonymous_corpus_hash(conn)
        finally:
            conn.close()
        metrics = (
            inspect_result["payload"].get("execution_metrics") or {}
            if inspect_result["ok"]
            else {}
        )
        anti_cheat = _collect_sample_evidence(
            clone_db,
            run["scan_run_id"],
            metrics,
            session_capability_present=session_capability[
                "session_capability_present"
            ],
            source_guard_policy=source_guard_policy,
            validated=run["validated"] and inspect_result["ok"],
        )
        sample = {
            "index": index,
            "wall_ms": run["wall_ms"],
            "status": run["status"],
            "response_error": _portable_response_error(run.get("response_error")),
            "scan_run_id": run["scan_run_id"],
            "idempotent_replay_false": (
                idempotent_probe["request_id_absent_before_run"]
                and run["scan_run_id"] is not None
            ),
            "snapshot_hit": bool(metrics.get("snapshot_hit")),
            "parse_cache_lookup_count": metrics.get("parse_cache_lookup_count"),
            "classification_cache_lookup_count": metrics.get(
                "classification_cache_lookup_count"
            ),
            "parse_cache_all_hit": metrics.get("parse_cache_all_hit"),
            "classification_cache_all_hit": metrics.get(
                "classification_cache_all_hit"
            ),
            "inspect_ok": inspect_result["ok"],
            "inspect_error_code": None
            if inspect_result["ok"]
            else "INSPECT_FAILED",
            "corpus_manifest": corpus_manifest,
            "anti_cheat": anti_cheat,
            "context_sha256": context["context_sha256"],
            "timing_ms": _timing_ms(metrics),
            "semantic": context,
        }
        passed, failure_code = _anti_cheat_verdict(anti_cheat)
        sample["anti_cheat_passed"] = passed
        if failure_code is not None:
            sample["anti_cheat_failure_code"] = failure_code
        cache_samples.append(sample)

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
            corpus_manifest = anonymous_corpus_hash(conn)
        finally:
            conn.close()
        metrics = (
            inspect_result["payload"].get("execution_metrics") or {}
            if inspect_result["ok"]
            else {}
        )
        anti_cheat = _collect_sample_evidence(
            snap_cold["db"],
            run["scan_run_id"],
            metrics,
            session_capability_present=session_capability[
                "session_capability_present"
            ],
            source_guard_policy=source_guard_policy,
            validated=run["validated"] and inspect_result["ok"],
        )
        sample = {
            "index": index,
            "wall_ms": run["wall_ms"],
            "status": run["status"],
            "response_error": _portable_response_error(run.get("response_error")),
            "scan_run_id": run["scan_run_id"],
            "idempotent_replay_false": (
                idempotent_probe["request_id_absent_before_run"]
                and run["scan_run_id"] not in seen_run_ids
            ),
            "snapshot_hit": bool(metrics.get("snapshot_hit")),
            "inspect_ok": inspect_result["ok"],
            "inspect_error_code": None
            if inspect_result["ok"]
            else "INSPECT_FAILED",
            "corpus_manifest": corpus_manifest,
            "anti_cheat": anti_cheat,
            "context_sha256": context["context_sha256"],
            "context_identical_to_cold": context["context_sha256"]
            == snap_cold["context"]["context_sha256"],
            "timing_ms": _timing_ms(metrics),
            "semantic": context,
        }
        passed, failure_code = _anti_cheat_verdict(anti_cheat)
        sample["anti_cheat_passed"] = passed
        if failure_code is not None:
            sample["anti_cheat_failure_code"] = failure_code
        snap_samples.append(sample)
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
        s["status"] == "ok"
        and s["inspect_ok"] is True
        and s["anti_cheat_passed"] is True
        and s["idempotent_replay_false"] is True
        and s["snapshot_hit"] is False
        and s["parse_cache_lookup_count"] > 0
        and s["classification_cache_lookup_count"] > 0
        and s["parse_cache_all_hit"] is True
        and s["classification_cache_all_hit"] is True
        for s in cache_samples
    )
    snap_warm_ok = all(
        s["status"] == "ok"
        and s["inspect_ok"] is True
        and s["anti_cheat_passed"] is True
        and s["snapshot_hit"] is True
        and s["idempotent_replay_false"]
        for s in snap_samples
    )

    all_evidence = (
        [cold["anti_cheat"] for cold in colds]
        + [sample["anti_cheat"] for sample in cache_samples]
        + [sample["anti_cheat"] for sample in snap_samples]
    )
    all_manifests = (
        [cold["corpus_manifest"] for cold in colds]
        + [sample["corpus_manifest"] for sample in cache_samples]
        + [sample["corpus_manifest"] for sample in snap_samples]
    )
    corpus_keys = {
        (
            manifest.get("anonymous_manifest_sha256"),
            manifest.get("source_count"),
        )
        for manifest in all_manifests
    }
    warm_anti_cheat_ok = all(
        sample["anti_cheat_passed"] for sample in cache_samples + snap_samples
    )
    provenance_ok = len(all_evidence) == 9 and _identity_evidence_consistent(
        all_evidence
    )
    corpus_ok = len(all_manifests) == 9 and len(corpus_keys) == 1 and all(
        manifest.get("anonymous_manifest_sha256") == PREVIOUS_MANIFEST_HASHES[label]
        for manifest in all_manifests
    )

    gates = {
        "snapshot_warm_median_improvement_ge_20pct": improvement >= MIN_WARM_IMPROVEMENT,
        "cache_warm_all_snapshot_miss_and_cache_all_hit": cache_warm_ok,
        "snapshot_warm_all_snapshot_hit": snap_warm_ok,
        "semantic_identical": semantic_identical,
        "all_warm_samples_anti_cheat_passed": warm_anti_cheat_ok,
        "all_cold_and_warm_profiles_builds_identical": provenance_ok,
        "all_cold_and_warm_corpus_manifests_frozen": corpus_ok,
        "passes": False,
    }
    gates["passes"] = all(value for key, value in gates.items() if key != "passes")

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


def _timing_ms(metrics: dict) -> dict:
    """spec Part 6 持久化可重放的 wall-clock 拆分（来自 inspect execution_metrics）。"""
    return {
        "worker_handshake_ms": int(metrics.get("worker_handshake_ms", 0) or 0),
        "discovery_ms": int(metrics.get("discovery_ms", 0) or 0),
        "snapshot_lookup_ms": int(metrics.get("snapshot_lookup_ms", 0) or 0),
        "current_run_audit_write_ms": int(
            metrics.get("current_run_audit_write_ms", 0) or 0
        ),
        "terminal_precommit_ms": int(metrics.get("terminal_precommit_ms", 0) or 0),
        "envelope_rebuild_ms": int(metrics.get("envelope_rebuild_ms", 0) or 0),
        "terminal_rows_written": int(metrics.get("terminal_rows_written", 0) or 0),
        "deadline_precommit_elapsed_ms": int(
            metrics.get("deadline_precommit_elapsed_ms", 0) or 0
        ),
    }


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
    session_capability: dict | None = None,
    source_guard_policy: str | None = None,
) -> dict:
    profile = profile or SEVEN_D_PROFILE
    session_capability = session_capability or _probe_session_capability(scanner)
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
    cold_metrics: dict = {}
    corpus = {
        "anonymous_manifest_sha256": None,
        "source_count": None,
    }
    cold_evidence = _failed_sample_evidence(
        session_capability_present=session_capability[
            "session_capability_present"
        ],
        source_guard_policy=source_guard_policy,
        collection_error_code="EVIDENCE_COLLECTION_FAILED",
    )
    if cold["scan_run_id"] is not None:
        cold_inspect = run_inspect_v2(scanner, cold_db, cold["scan_run_id"])
        if cold_inspect["ok"]:
            cold_metrics = cold_inspect["payload"].get("execution_metrics") or {}
            conn = sqlite3.connect(cold_db)
            try:
                corpus = anonymous_corpus_hash(conn)
                cold_context = capture_context_state(conn, cold["scan_run_id"])
            finally:
                conn.close()
            cold_evidence = _collect_sample_evidence(
                cold_db,
                cold["scan_run_id"],
                cold_metrics,
                session_capability_present=session_capability[
                    "session_capability_present"
                ],
                source_guard_policy=source_guard_policy,
                validated=cold["validated"],
            )
    cold_anti_cheat_passed, cold_anti_cheat_failure_code = _anti_cheat_verdict(
        cold_evidence
    )
    corpus_matches_frozen = (
        corpus.get("anonymous_manifest_sha256") == PREVIOUS_MANIFEST_HASHES["7d"]
    )
    cold_clean = (
        cold["status"] == "ok"
        and cold_context is not None
        and bool(cold_inspect and cold_inspect["ok"])
        and cold_anti_cheat_passed
        and corpus_matches_frozen
    )

    if not cold_clean:
        return {
            "window": {"start": SEVEN_D_START.isoformat(), "end": SEVEN_D_END.isoformat()},
            "report_mode": report_mode,
            "corpus": corpus,
            "corpus_manifest_provenance": ANONYMOUS_CORPUS_MANIFEST_PROVENANCE,
            "corpus_changed_since_previous": not corpus_matches_frozen,
            "normalized_profile_json": cold_evidence.get(
                "normalized_profile_json"
            ),
            "normalized_profile_hash_algorithm": cold_evidence.get(
                "normalized_profile_hash_algorithm"
            ),
            "normalized_profile_sha256": cold_evidence.get(
                "normalized_profile_sha256"
            ),
            "build_identity": cold_evidence.get("build_identity"),
            "cold_wall_ms": cold["wall_ms"],
            "cold_status": cold["status"],
            "cold_response_error": _portable_response_error(
                cold.get("response_error")
            ),
            "cold_inspect_ok": bool(cold_inspect and cold_inspect["ok"]),
            "cold_inspect_error_code": None
            if cold_inspect and cold_inspect["ok"]
            else "INSPECT_FAILED",
            "cold_anti_cheat": cold_evidence,
            "cold_anti_cheat_failure_code": cold_anti_cheat_failure_code,
            "samples": [],
            "snapshot_warm_wall_ms": [],
            "snapshot_warm_median_ms": 0.0,
            "snapshot_warm_max_ms": 0.0,
            "gates": {
                "cold_runs_clean": False,
                "corpus_matches_frozen_manifest": corpus_matches_frozen,
                "cold_anti_cheat_passed": cold_anti_cheat_passed,
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
            sample_corpus = anonymous_corpus_hash(conn)
        finally:
            conn.close()
        metrics = (
            inspect_result["payload"].get("execution_metrics") or {}
            if inspect_result["ok"]
            else {}
        )
        anti_cheat = _collect_sample_evidence(
            cold_db,
            run["scan_run_id"],
            metrics,
            session_capability_present=session_capability[
                "session_capability_present"
            ],
            source_guard_policy=source_guard_policy,
            validated=run["validated"] and inspect_result["ok"],
        )
        sample = {
            "index": index,
            "wall_ms": run["wall_ms"],
            "status": run["status"],
            "response_error": _portable_response_error(run.get("response_error")),
            "scan_run_id": run["scan_run_id"],
            "idempotent_replay_false": (
                idempotent_probe["request_id_absent_before_run"]
                and run["scan_run_id"] not in seen_run_ids
            ),
            "snapshot_hit": bool(metrics.get("snapshot_hit")),
            "inspect_ok": inspect_result["ok"],
            "inspect_error_code": None
            if inspect_result["ok"]
            else "INSPECT_FAILED",
            "corpus_manifest": sample_corpus,
            "anti_cheat": anti_cheat,
            "context_hash_identical": context["context_sha256"]
            == cold_context["context_sha256"],
            # spec Part 6: snapshot-warm wall 拆分（handshake/discovery/lookup/
            # finalization）必须证据可查，7d FAIL 归属才能验证。
            "timing_ms": _timing_ms(metrics),
        }
        passed, failure_code = _anti_cheat_verdict(anti_cheat)
        sample["anti_cheat_passed"] = passed
        if failure_code is not None:
            sample["anti_cheat_failure_code"] = failure_code
        samples.append(sample)
        seen_run_ids.add(run["scan_run_id"])

    warm_ms = [s["wall_ms"] for s in samples]
    all_evidence = [cold_evidence] + [sample["anti_cheat"] for sample in samples]
    all_profiles_builds_identical = (
        len(all_evidence) == 4 and _identity_evidence_consistent(all_evidence)
    )
    all_corpus_frozen = all(
        sample["corpus_manifest"] == corpus
        and sample["corpus_manifest"].get("anonymous_manifest_sha256")
        == PREVIOUS_MANIFEST_HASHES["7d"]
        for sample in samples
    )
    gates = {
        "cold_runs_clean": True,
        "corpus_matches_frozen_manifest": corpus_matches_frozen,
        "cold_anti_cheat_passed": cold_anti_cheat_passed,
        "all_warm_samples_anti_cheat_passed": all(
            sample["anti_cheat_passed"] for sample in samples
        ),
        "all_warm_samples_status_ok": all(
            sample["status"] == "ok" and sample["inspect_ok"] is True
            for sample in samples
        ),
        "all_cold_and_warm_profiles_builds_identical": all_profiles_builds_identical,
        "all_cold_and_warm_corpus_manifests_frozen": all_corpus_frozen,
        "median_le_370ms": median_ms(warm_ms) <= SEVEN_D_TARGETS["median_ms_le"],
        "max_le_420ms": max(warm_ms) <= SEVEN_D_TARGETS["max_ms_le"],
        "all_snapshot_hit": all(s["snapshot_hit"] for s in samples),
        "all_idempotent_replay_false": all(s["idempotent_replay_false"] for s in samples),
        "all_context_identical_to_cold": all(s["context_hash_identical"] for s in samples),
        "passes": False,
    }
    gates["passes"] = all(value for key, value in gates.items() if key != "passes")

    return {
        "window": {"start": SEVEN_D_START.isoformat(), "end": SEVEN_D_END.isoformat()},
        "report_mode": report_mode,
        "corpus": corpus,
        "corpus_manifest_provenance": ANONYMOUS_CORPUS_MANIFEST_PROVENANCE,
        "corpus_changed_since_previous": not corpus_matches_frozen,
        "normalized_profile_json": cold_evidence.get("normalized_profile_json"),
        "normalized_profile_hash_algorithm": cold_evidence.get(
            "normalized_profile_hash_algorithm"
        ),
        "normalized_profile_sha256": cold_evidence.get(
            "normalized_profile_sha256"
        ),
        "build_identity": cold_evidence.get("build_identity"),
        "cold_wall_ms": cold["wall_ms"],
        "cold_status": cold["status"],
        "cold_response_error": _portable_response_error(cold.get("response_error")),
        "cold_inspect_ok": bool(cold_inspect and cold_inspect["ok"]),
        "cold_inspect_error_code": None
        if cold_inspect and cold_inspect["ok"]
        else "INSPECT_FAILED",
        "cold_anti_cheat": cold_evidence,
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
    classifier_probe = _probe_classifier_build(scanner)

    results: dict = {
        "schema": "acceptance_real_dir_v2",
        "generated_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "hardware": _hardware_evidence(),
        "build": {
            "scanner_sha256": sha256_file(scanner),
            "office_worker_sha256": sha256_file(office_worker),
            "session_capability": session_capability,
        },
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
            results["build"]["classifier"] = classifier_probe
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
                    session_capability=session_capability,
                    source_guard_policy=classifier_probe.get(
                        "source_guard_policy"
                    ),
                )
            else:
                result = run_cold_acceptance_and_warm(
                    scanner=scanner,
                    office_worker=office_worker,
                    work_dir=work_dir,
                    label=scenario,
                    out_root=out_root,
                    session_capability=session_capability,
                    source_guard_policy=classifier_probe.get("source_guard_policy"),
                )
        except SeedPreparerError:
            results["scenarios"][scenario] = {
                "error": {
                    "error_code": "SEED_PREPARER_FAILED_CLOSED",
                    "exception_type": "SeedPreparerError",
                }
            }
            print(f"scenario {scenario} seed preparer failed closed")
            continue
        except Exception as error:  # noqa: BLE001 - evidence must be collected
            results["scenarios"][scenario] = {
                "error": {
                    "error_code": "SCENARIO_FAILED",
                    "exception_type": type(error).__name__,
                }
            }
            print(f"scenario {scenario} failed")
            continue
        results["scenarios"][scenario] = result

    try:
        _assert_portable_evidence(
            results,
            forbidden_paths=(work_dir, out_root, args.evidence, PROJECT_ROOT),
        )
    except ValueError as error:
        print(f"portable evidence gate failed: {error}")
        return 1

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
                    print(
                        f"{scenario}: warm_gates="
                        f"{json.dumps(result['warm_comparison']['gates'])}"
                    )
            else:
                print(f"{scenario}: gates={json.dumps(result.get('gates', {}))}")
        return 2
    print("=== ALL ACCEPTANCE GATES PASS ===")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
