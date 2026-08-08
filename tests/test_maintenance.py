"""maintenance 命令的 dry-run / incremental_vacuum 集成测试（spec Part 4/5.3）。

mode=gc|incremental_vacuum，dry_run 零写；auto_vacuum=none 时 incremental_vacuum
必须明确失败为 MAINTENANCE_MODE_UNAVAILABLE；v1 库固定 SCHEMA_UPGRADE_REQUIRED；
gc 非 dry-run 执行深度 row GC（terminal run retention + orphan artifact + cache eviction）。
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
import subprocess
from pathlib import Path

import pytest

from src.models.scanner_contract import MaintenanceRequestV1

PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCANNER_BIN = PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"

if not SCANNER_BIN.is_file():
    pytest.skip(
        "Rust scanner release binary is not built",
        allow_module_level=True,
    )


def _v2_ddl() -> str:
    """从 Rust 源读取真实的 V2_DDL，避免最小 fixture 与真实 schema 漂移。"""
    schema_rs = (
        PROJECT_ROOT / "rust" / "scanner_core" / "src" / "store" / "schema.rs"
    ).read_text(encoding="utf-8")
    marker = 'pub const V2_DDL: &str = r#"'
    start = schema_rs.index(marker) + len(marker)
    end = schema_rs.index('"#;', start)
    return schema_rs[start:end]


def _fresh_v2_db(path: Path, *, auto_vacuum: str = "incremental") -> None:
    """用真实 V2_DDL 建一个空的 v2 库；auto_vacuum 可选 incremental|none。"""
    conn = sqlite3.connect(path)
    try:
        if auto_vacuum == "incremental":
            conn.execute("PRAGMA auto_vacuum = INCREMENTAL;")
        conn.executescript(_v2_ddl())
        conn.execute("PRAGMA user_version = 2;")
        conn.execute(
            """
            INSERT INTO schema_migration_history(
                user_version, origin, upgrade_request_id, engine_build, committed_at_ms
            ) VALUES (2, 'created_empty', NULL, 'test-build', 1)
            """
        )
        conn.commit()
    finally:
        conn.close()


def _maintenance_request(
    db: Path,
    *,
    mode: str,
    dry_run: bool,
    request_id: str = "123e4567-e89b-42d3-a456-426614174100",
) -> MaintenanceRequestV1:
    return MaintenanceRequestV1(
        contract="ai_daily_scanner_maintenance",
        protocol_version=1,
        request_id=request_id,
        scan_db_path=str(db),
        mode=mode,
        dry_run=dry_run,
    )


def _run_maintenance(request: MaintenanceRequestV1) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(SCANNER_BIN), "maintenance"],
        input=request.model_dump_json().encode(),
        capture_output=True,
        check=False,
    )


def _seed_terminal_run(db: Path, *, finished_at_ms: int, status: str = "success") -> int:
    """插入一条 terminal run + 一条独立 inventory（非级联），返回 scan_run_id。"""
    conn = sqlite3.connect(db)
    try:
        conn.execute("PRAGMA foreign_keys = ON;")
        cur = conn.execute(
            """
            INSERT INTO scan_runs(
                request_id, canonical_request_json, request_hash_algorithm, request_hash,
                owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                finished_at_ms, final_envelope_json, audit_provenance_version, audit_size_bytes
            ) VALUES (?, '{}', 'sha256-request-v1', ?, 'owner', ?, 1, 1, 1, ?, '{}', 'full_v2', ?)
            """,
            (
                f"request-{finished_at_ms}",
                "0" * 64,
                status,
                finished_at_ms,
                finished_at_ms % 1000 + 1,
            ),
        )
        scan_run_id = cur.lastrowid
        conn.execute(
            """
            INSERT INTO scan_run_attempts(
                scan_run_id, attempt_number, owner_id, normalized_scan_db_path,
                normalized_office_worker_path, normalized_python_executable,
                normalized_python_module_root, python_document_worker_module,
                engine_fingerprint, office_worker_contract, office_worker_version,
                office_worker_build, python_worker_contract, python_worker_version,
                python_worker_build, started_at_ms, finished_at_ms, status
            ) VALUES (?, 1, 'owner', 'C:\\db\\scan_index_v2.sqlite3',
                'C:\\office.exe', 'C:\\python.exe', 'C:\\module', 'src.workers',
                'test-build', 'ai_daily_worker_v1', '1.0', 'office-build',
                'ai_daily_worker_v1', '1.0', 'python-build', 1, ?, ?)
            """,
            (scan_run_id, finished_at_ms, status),
        )
        conn.execute(
            """
            INSERT INTO run_diagnostics(
                scan_run_id, sequence, severity, error_code, message, retryable, stage
            ) VALUES (?, 0, 'warning', 'CACHE_WRITE_FAILED', 'seed', 0, 'cache')
            """,
            (scan_run_id,),
        )
        conn.commit()
        return scan_run_id
    finally:
        conn.close()


def _seed_orphan_artifact(db: Path) -> None:
    """插入一个不被任何 context_runs 引用的 artifact（orphan）。"""
    conn = sqlite3.connect(db)
    try:
        conn.execute("PRAGMA foreign_keys = ON;")
        cur = conn.execute(
            """
            INSERT INTO context_artifacts(
                snapshot_eligible, snapshot_key_sha256, snapshot_key_json,
                final_context, context_sha256, semantic_summary_json,
                artifact_size_bytes, created_at_ms, last_accessed_bucket
            ) VALUES (0, NULL, NULL, 'orphan', ?, '{}', 10, 1, '2020-01-01')
            """,
            (hashlib.sha256(b"orphan").hexdigest(),),
        )
        conn.commit()
    finally:
        conn.close()


def test_maintenance_dry_run_has_zero_mutation(tmp_path: Path) -> None:
    db = tmp_path / "scan_index_v2.sqlite3"
    _fresh_v2_db(db)

    out = _run_maintenance(_maintenance_request(db, mode="gc", dry_run=True))
    assert out.returncode == 0, out.stderr.decode(errors="replace")
    body = json.loads(out.stdout.decode("utf-8"))

    assert body["status"] == "ok"
    assert body["deleted"]["parse_cache_rows"] == 0
    assert body["deleted"]["classification_cache_rows"] == 0
    assert body["deleted"]["context_artifacts_rows"] == 0
    assert body["deleted"]["scan_runs_rows"] == 0
    assert body["before"] == body["after"]
    assert body["after_complete"] is True
    assert body["pre_integrity_check"] == "ok"
    assert body["post_integrity_check"] == "not_run"
    assert body["vacuum"]["status"] == "skipped_dry_run"
    assert body["error"] is None

    # dry-run 零写：不创建 WAL/shm sidecar，DB 文件仍可被业务 open。
    assert not db.with_name(db.name + "-wal").exists()
    assert not db.with_name(db.name + "-shm").exists()
    conn = sqlite3.connect(db)
    try:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 2
    finally:
        conn.close()


def test_incremental_vacuum_on_auto_vacuum_none_fails_cleanly(tmp_path: Path) -> None:
    db = tmp_path / "scan_index_v2.sqlite3"
    _fresh_v2_db(db, auto_vacuum="none")

    out = _run_maintenance(
        _maintenance_request(db, mode="incremental_vacuum", dry_run=False)
    )
    assert out.returncode == 1, out.stderr.decode(errors="replace")
    body = json.loads(out.stdout.decode("utf-8"))

    assert body["status"] == "error"
    assert body["error"] is not None
    assert body["error"]["error_code"] == "MAINTENANCE_MODE_UNAVAILABLE"
    assert body["error"]["stage"] == "maintenance"
    assert body["error"]["file_path"] is None
    assert body["error"]["backend"] is None
    assert body["vacuum"]["status"] == "error"
    assert body["deleted"]["scan_runs_rows"] == 0
    assert body["before"] == body["after"]


def test_maintenance_v1_database_is_schema_upgrade_required(tmp_path: Path) -> None:
    db = tmp_path / "scan_index_v2.sqlite3"
    _fresh_v2_db(db, auto_vacuum="incremental")
    conn = sqlite3.connect(db)
    try:
        conn.execute("PRAGMA user_version = 1;")
        conn.commit()
    finally:
        conn.close()

    out = _run_maintenance(_maintenance_request(db, mode="gc", dry_run=True))
    assert out.returncode == 1, out.stderr.decode(errors="replace")
    body = json.loads(out.stdout.decode("utf-8"))

    assert body["status"] == "error"
    assert body["error"]["error_code"] == "SCHEMA_UPGRADE_REQUIRED"


def test_maintenance_gc_deletes_old_runs_orphans_and_evicts_cache(tmp_path: Path) -> None:
    db = tmp_path / "scan_index_v2.sqlite3"
    _fresh_v2_db(db, auto_vacuum="incremental")

    _seed_orphan_artifact(db)
    old_run = _seed_terminal_run(db, finished_at_ms=1_000)  # 远早于 90 天前
    # 给 old_run 接一条 context_runs，使其成为被引用 run（可删除）
    conn = sqlite3.connect(db)
    try:
        conn.execute("PRAGMA foreign_keys = ON;")
        conn.execute(
            """
            INSERT INTO context_runs(
                context_run_id, scan_run_id, context_profile_hash, status,
                final_context, context_sha256, source_file_count, success_count,
                timeout_count, included_file_count, omitted_file_count,
                error_file_count, input_chars, output_chars, total_duration_ms,
                discovery_duration_ms, parse_duration_ms, compression_duration_ms,
                created_at_ms
            ) VALUES (?, ?, ?, 'success', 'ctx', ?, 1, 1, 0, 1, 0, 0, 3, 3, 1, 1, 1, 1, 1)
            """,
            (
                old_run,
                old_run,
                "b" * 64,
                hashlib.sha256(b"ctx").hexdigest(),
            ),
        )
        conn.commit()
    finally:
        conn.close()

    out = _run_maintenance(_maintenance_request(db, mode="gc", dry_run=False))
    assert out.returncode == 0, out.stderr.decode(errors="replace")
    body = json.loads(out.stdout.decode("utf-8"))

    assert body["status"] == "ok"
    assert body["deleted"]["scan_runs_rows"] >= 1
    assert body["deleted"]["scan_run_attempts_rows"] >= 1
    assert body["deleted"]["run_diagnostics_rows"] >= 1
    assert body["deleted"]["context_runs_rows"] >= 1
    assert body["deleted"]["context_artifacts_rows"] >= 1
    assert body["vacuum"]["status"] == "not_requested"
    assert body["post_integrity_check"] == "ok"
    assert body["error"] is None

    conn = sqlite3.connect(db)
    try:
        assert conn.execute(
            "SELECT count(*) FROM scan_runs WHERE scan_run_id=?1", (old_run,)
        ).fetchone()[0] == 0
        assert conn.execute("SELECT count(*) FROM context_artifacts").fetchone()[0] == 0
    finally:
        conn.close()
