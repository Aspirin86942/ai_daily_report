"""Inspect v2 / Version v2 观测接口测试（spec Part 5.3）。

- full_v2 snapshot-hit run → `inspect-run --response-version 2` 返回
  artifact_id 非空、reuse_kind=context_snapshot、reused_from 非空；
- migrated v1 run → v2 inspect fail closed with INSPECT_V2_PROVENANCE_UNAVAILABLE；
- `version --response-version 2` 返回严格 VersionResponseV2。
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
import subprocess
from datetime import date
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.models.scanner_contract import UpgradeDatabaseRequestV1
from src.services.context_scheduler import (
    ContextScheduleRequest,
    ContextScheduler,
)
from src.services.rust_context_client import RustContextClient

PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCANNER_BIN = PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"
OFFICE_BIN = (
    PROJECT_ROOT
    / "rust"
    / "target"
    / "release"
    / "ai-daily-office-parser.exe"
)

if not (SCANNER_BIN.is_file() and OFFICE_BIN.is_file()):
    pytest.skip(
        "Rust scanner release binaries are not built",
        allow_module_level=True,
    )


def _runtime_config(
    root: Path,
    work_dir: Path,
    scan_db_path: Path,
) -> SimpleNamespace:
    raw_profile = {
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".txt"],
        "ignored_patterns": ["~$*", "*.tmp"],
        "max_workers": 2,
    }
    return SimpleNamespace(
        scanner_engine="rust_v2",
        rust_scanner_bin=str(SCANNER_BIN),
        rust_office_parser_bin=str(OFFICE_BIN),
        rust_index_db_path=str(scan_db_path),
        rust_process_timeout_seconds=60.0,
        work_dir=work_dir,
        scanner_contract_profile=lambda: raw_profile.copy(),
        llm_provider="deepseek",
        llm_config={"model_id": "synthetic-no-network"},
        deepseek_api_key="synthetic-doctor-placeholder",
        openai_api_key="",
        reports_dir=root / "shared" / "reports",
        db_dir=root / "shared" / "db",
    )


def _schedule_request() -> ContextScheduleRequest:
    return ContextScheduleRequest(
        report_mode="daily",
        source="scan",
        start_date=date(2000, 1, 1),
        end_date=date(2099, 12, 31),
    )


def test_inspect_v2_reports_snapshot_reuse(tmp_path: Path) -> None:
    work_dir = tmp_path / "快照 合成目录"
    work_dir.mkdir()
    (work_dir / "证据.txt").write_text("snapshot inspect evidence", encoding="utf-8")
    scan_db_path = tmp_path / "state" / "scan_index_v2.sqlite3"
    scan_db_path.parent.mkdir()
    cfg = _runtime_config(tmp_path, work_dir, scan_db_path)
    client = RustContextClient(
        config=cfg,
        project_root=PROJECT_ROOT,
        scanner_binary=SCANNER_BIN,
        scan_db_path=scan_db_path,
        office_worker_path=OFFICE_BIN,
        timeout_seconds=60,
    )

    cold = client.build_context(_schedule_request())
    warm = client.build_context(_schedule_request())
    assert cold.status == warm.status == "ok"
    assert cold.scan_run_id is not None
    assert warm.scan_run_id is not None
    assert cold.scan_run_id != warm.scan_run_id

    # v2 全量 provenance：snapshot-hit run 必须暴露 artifact + source 溯源。
    audit = client.inspect_run_v2(warm.scan_run_id)
    assert audit.status == "ok"
    assert audit.run_status == "success"
    assert audit.artifact_id is not None
    assert audit.reused_from_context_run_id is not None
    assert audit.reuse_kind == "context_snapshot"
    assert audit.execution_metrics.snapshot_hit is True
    # snapshot 跳过 classification/parse lookup，两个 all_hit 为 null。
    assert audit.execution_metrics.parse_cache_lookup_count == 0
    assert audit.execution_metrics.parse_cache_all_hit is None
    assert audit.execution_metrics.classification_cache_all_hit is None
    assert all(
        item.parse_cache_status == "snapshot" for item in audit.files
    )


def test_inspect_v2_cold_run_reports_real_metrics(tmp_path: Path) -> None:
    """冷 run 的 v2 execution_metrics 必须来自 finalize 持久化的真实测量，
    不得用 0 冒充已知值（spec Part 5.3「未知值不得用 0 代替」）。"""
    work_dir = tmp_path / "冷 指标 目录"
    work_dir.mkdir()
    (work_dir / "证据.txt").write_text("cold real metrics evidence", encoding="utf-8")
    scan_db_path = tmp_path / "state" / "scan_index_v2.sqlite3"
    scan_db_path.parent.mkdir()
    cfg = _runtime_config(tmp_path, work_dir, scan_db_path)
    client = RustContextClient(
        config=cfg,
        project_root=PROJECT_ROOT,
        scanner_binary=SCANNER_BIN,
        scan_db_path=scan_db_path,
        office_worker_path=OFFICE_BIN,
        timeout_seconds=60,
    )

    cold = client.build_context(_schedule_request())
    assert cold.scan_run_id is not None
    audit = client.inspect_run_v2(cold.scan_run_id)
    assert audit.status == "ok"
    m = audit.execution_metrics
    # 真实测量：live worker handshake 与 terminal 逻辑行数。
    assert m.worker_handshake_ms > 0
    assert m.terminal_rows_written > 0
    assert m.discovery_observed_file_count == 1
    assert m.snapshot_hit is False
    assert m.parse_cache_lookup_count == 1
    assert m.parse_cache_all_hit is False
    # 冷 run 本轮真实执行 parse：不把 snapshot 身份安到 pdf_classification。
    assert len(audit.files) == 1
    assert audit.files[0].parse_cache_status == "miss"
    assert audit.files[0].parse_transport == "rust_in_process"
    assert audit.files[0].parse_attempt_count == 1
    assert audit.files[0].pdf_classification is None


def test_inspect_v1_projects_snapshot_as_fresh_with_warning(tmp_path: Path) -> None:
    work_dir = tmp_path / "投影 合成目录"
    work_dir.mkdir()
    (work_dir / "证据.txt").write_text("lossy projection evidence", encoding="utf-8")
    scan_db_path = tmp_path / "state" / "scan_index_v2.sqlite3"
    scan_db_path.parent.mkdir()
    cfg = _runtime_config(tmp_path, work_dir, scan_db_path)
    client = RustContextClient(
        config=cfg,
        project_root=PROJECT_ROOT,
        scanner_binary=SCANNER_BIN,
        scan_db_path=scan_db_path,
        office_worker_path=OFFICE_BIN,
        timeout_seconds=60,
    )

    client.build_context(_schedule_request())
    warm = client.build_context(_schedule_request())
    assert warm.scan_run_id is not None

    # v1 投影：snapshot 行投影为 fresh，并附 output-only 的 projection warning。
    v1 = client.inspect_run(warm.scan_run_id)
    assert [(item.cache_status, item.cache_miss_reason) for item in v1.files] == [
        ("fresh", "")
    ]
    codes = {item.error_code for item in v1.warnings}
    assert "SNAPSHOT_REUSE_PROJECTED_AS_FRESH" in codes
    # output-only：不写回 full diagnostics / Envelope metadata。
    v2 = client.inspect_run_v2(warm.scan_run_id)
    assert not any(
        item.error_code == "SNAPSHOT_REUSE_PROJECTED_AS_FRESH"
        for item in v2.warnings
    )


def test_inspect_v2_migrated_v1_run_fails_closed(tmp_path: Path) -> None:
    db = tmp_path / "scan_index_v2.sqlite3"
    _v1_db_with_terminal_run(db)

    upgrade = UpgradeDatabaseRequestV1(
        contract="ai_daily_scanner_upgrade",
        protocol_version=1,
        request_id="123e4567-e89b-42d3-a456-426614174000",
        scan_db_path=str(db),
        apply=True,
    )
    out = subprocess.run(
        [str(SCANNER_BIN), "upgrade-db"],
        input=upgrade.model_dump_json().encode(),
        capture_output=True,
        check=False,
    )
    assert out.returncode == 0, out.stderr.decode(errors="replace")
    payload = json.loads(out.stdout.decode("utf-8"))
    assert payload["status"] == "ok"
    assert payload["schema_migrated"] is True

    # 迁移后的 migrated_v1 run 对 v2 inspect 固定 fail closed，绝不伪造 0/null。
    client = RustContextClient(
        config=_runtime_config(tmp_path, tmp_path, db),
        project_root=PROJECT_ROOT,
        scanner_binary=SCANNER_BIN,
        scan_db_path=db,
        office_worker_path=OFFICE_BIN,
        timeout_seconds=60,
    )
    audit = client.inspect_run_v2(1)
    assert audit.status == "error"
    assert audit.error is not None
    assert audit.error.error_code == "INSPECT_V2_PROVENANCE_UNAVAILABLE"
    assert audit.error.stage == "inspect"
    assert audit.error.retryable is False
    assert audit.error.file_path is None
    assert audit.error.backend is None
    assert audit.artifact_id is None
    assert audit.reused_from_context_run_id is None
    assert audit.reuse_kind == "none"
    assert audit.files == []
    assert audit.decisions == []
    assert audit.execution_metrics.is_error_sentinel()

    # 默认 v1 inspect 继续使用迁移前语义（spec Part 5.3）。
    v1 = client.inspect_run(1)
    assert v1.status == "ok"
    assert v1.run_status == "error"


def test_version_v2_echoes_cache_retention_constants(tmp_path: Path) -> None:
    client = RustContextClient(
        config=_runtime_config(
            tmp_path,
            tmp_path / "work",
            tmp_path / "state" / "scan_index_v2.sqlite3",
        ),
        project_root=PROJECT_ROOT,
        scanner_binary=SCANNER_BIN,
        scan_db_path=tmp_path / "state" / "scan_index_v2.sqlite3",
        office_worker_path=OFFICE_BIN,
        timeout_seconds=60,
    )
    version = client.version_v2()
    assert version.response_version == 2
    assert version.source_guard_policy == "source_guard_v2"
    assert version.max_source_files_per_run == 1_000_000
    assert version.inspect_response_versions == [1, 2]
    policy = version.cache_retention_policy
    assert policy.policy_version == "cache_retention_v1"
    assert policy.parse_cache_max_bytes == 1073741824
    assert policy.classification_cache_max_bytes == 134217728
    assert policy.context_artifacts_max_bytes == 536870912
    assert policy.terminal_audit_max_bytes == 2147483648
    assert policy.terminal_run_max_count == 500
    assert policy.terminal_run_max_age_days == 90
    assert policy.opportunistic_gc_budget_ms == 10


# ---------------------------------------------------------------------------
# migrated_v1 fixture（复用真实 V1_DDL，一条 terminal error run）
# ---------------------------------------------------------------------------

_MIGRATED_REQUEST_ID = "323e4567-e89b-42d3-a456-426614174002"

_VALID_ERROR_ENVELOPE = {
    "contract": "ai_daily_context",
    "protocol_version": 1,
    "request_id": _MIGRATED_REQUEST_ID,
    "engine_version": "test",
    "engine_build": "test-build",
    "status": "error",
    "file_context": "",
    "summary": {
        "source_file_count": 0,
        "success_count": 0,
        "timeout_count": 0,
        "included_file_count": 0,
        "omitted_file_count": 0,
        "error_file_count": 0,
        "input_chars": 0,
        "output_chars": 0,
        "total_duration_ms": 1,
        "discovery_duration_ms": 0,
        "parse_duration_ms": 0,
        "compression_duration_ms": 0,
    },
    "scan_run_id": 1,
    "context_run_id": None,
    "warnings": [],
    "error": {
        "error_code": "PARSER_FAILED",
        "message": "scanner could not start",
        "retryable": False,
        "stage": "parse",
        "file_path": None,
        "backend": None,
    },
}


def _v1_ddl() -> str:
    schema_rs = (
        PROJECT_ROOT / "rust" / "scanner_core" / "src" / "store" / "schema.rs"
    ).read_text(encoding="utf-8")
    marker = 'pub const V1_DDL: &str = r#"'
    start = schema_rs.index(marker) + len(marker)
    end = schema_rs.index('"#;', start)
    return schema_rs[start:end]


def _v1_db_with_terminal_run(path: Path) -> None:
    conn = sqlite3.connect(path)
    try:
        conn.execute("PRAGMA user_version = 1;")
        conn.executescript(_v1_ddl())
        # `canonical_request_json='{}'` must hash to the stored `request_hash`
        # so the migrated run passes the v1 inspect identity check
        # (`domain_hash(b"request-v1\0", canonical_request_json)`).
        request_hash = hashlib.sha256(b"request-v1\x00{}").hexdigest()
        conn.execute(
            """
            INSERT INTO scan_runs(
                request_id, canonical_request_json, request_hash_algorithm, request_hash,
                owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                finished_at_ms, final_envelope_json
             ) VALUES (?, '{}', 'sha256-request-v1', ?, 'owner', 'error', 1, 1, 1, 1, ?)
            """,
            (
                _MIGRATED_REQUEST_ID,
                request_hash,
                json.dumps(_VALID_ERROR_ENVELOPE),
            ),
        )
        conn.execute(
            """
            INSERT INTO run_diagnostics(
                scan_run_id, sequence, severity, error_code, message, retryable,
                stage, file_path, backend
             ) VALUES (1, 0, 'error', 'PARSER_FAILED', 'scanner could not start',
                        0, 'parse', NULL, NULL)
            """
        )
        conn.commit()
    finally:
        conn.close()
