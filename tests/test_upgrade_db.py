"""upgrade-db 命令的 audit/apply 集成测试（spec Part 8.3）。

apply=false 是只读 audit（零写、无 sidecar）；apply=true 是唯一生产升级入口
（独占 lease → 私有 open_for_upgrade → v1→v2 原子事务 → post integrity →
独立 auto_vacuum 转换）。工具不内置备份，回滚由运维保留的升级前 DB 副本承担。
"""

from __future__ import annotations

import json
import sqlite3
import subprocess
from pathlib import Path

import pytest

from src.models.scanner_contract import UpgradeDatabaseRequestV1

PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCANNER_BIN = PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"

if not SCANNER_BIN.is_file():
    pytest.skip(
        "Rust scanner release binary is not built",
        allow_module_level=True,
    )


UPGRADE_REQUEST_ID = "123e4567-e89b-42d3-a456-426614174000"
RUN_REQUEST_ID = "323e4567-e89b-42d3-a456-426614174002"

VALID_ERROR_ENVELOPE = {
    "contract": "ai_daily_context",
    "protocol_version": 1,
    "request_id": RUN_REQUEST_ID,
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
    """从 Rust 源读取真实的 V1_DDL，避免最小 fixture 与重验逻辑漂移。"""
    schema_rs = (
        PROJECT_ROOT / "rust" / "scanner_core" / "src" / "store" / "schema.rs"
    ).read_text(encoding="utf-8")
    marker = 'pub const V1_DDL: &str = r#"'
    start = schema_rs.index(marker) + len(marker)
    end = schema_rs.index('"#;', start)
    return schema_rs[start:end]


def _v1_db(path: Path, *, bad_envelope: bool = False) -> None:
    """用真实 V1_DDL 建一个提交的 v1 库：一条 inventory、一条 legacy parse_cache、
    一条 terminal error run（final_envelope_json 可解析且通过 contract 校验）。"""
    conn = sqlite3.connect(path)
    try:
        conn.execute("PRAGMA user_version = 1;")
        conn.executescript(_v1_ddl())
        conn.execute(
            """
            INSERT INTO file_inventory(
                file_identity, absolute_path, relative_path, file_type, source_version,
                size_bytes, mtime_ns, last_seen_run_id, last_seen_at_ms
             ) VALUES (
                'C:\\work\\a.txt', 'C:\\work\\a.txt', 'a.txt', '.txt',
                'mtime_ns=1:size=5', 5, 1, 1, 1
            )
            """
        )
        conn.execute(
            """
            INSERT INTO parse_cache(
                file_identity, source_version, parse_profile_hash, content, content_sha256,
                parser_backend, worker_lane, truncated, worker_contract_version,
                worker_version, worker_build, cached_at_ms
             ) VALUES (
                'C:\\work\\a.txt', 'mtime_ns=1:size=5', ?, 'hello', ?,
                'pdf_text_v1', 'python_document_process', 0,
                'ai_daily_worker_v1', '1.0', 'legacy-build', 1
            )
            """,
            ("0" * 64, "1" * 64),
        )
        envelope = (
            "{not valid json"
            if bad_envelope
            else json.dumps(VALID_ERROR_ENVELOPE)
        )
        conn.execute(
            """
            INSERT INTO scan_runs(
                request_id, canonical_request_json, request_hash_algorithm, request_hash,
                owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                finished_at_ms, final_envelope_json
             ) VALUES (?, '{}', 'sha256-request-v1', ?, 'owner', 'error', 1, 1, 1, 1, ?)
            """,
            (RUN_REQUEST_ID, "0" * 64, envelope),
        )
        conn.commit()
    finally:
        conn.close()


def _v1_db_wal(path: Path) -> None:
    """WAL-mode v1 fixture: build the rollback-journal v1 DB, switch to WAL,
    checkpoint, and close so no `-wal`/`-shm` sidecar remains (the real-world
    state after a cleanly-closed scanner process)."""
    _v1_db(path)
    conn = sqlite3.connect(path)
    try:
        conn.execute("PRAGMA journal_mode=WAL;")
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE);")
        conn.commit()
    finally:
        conn.close()
    assert not path.with_name(path.name + "-shm").exists()
    assert not path.with_name(path.name + "-wal").exists()


def _run_upgrade(request: UpgradeDatabaseRequestV1) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(SCANNER_BIN), "upgrade-db"],
        input=request.model_dump_json().encode(),
        capture_output=True,
        check=False,
    )


def _upgrade_request(db: Path, *, apply: bool, request_id: str) -> UpgradeDatabaseRequestV1:
    return UpgradeDatabaseRequestV1(
        contract="ai_daily_scanner_upgrade",
        protocol_version=1,
        request_id=request_id,
        scan_db_path=str(db),
        apply=apply,
    )


def test_upgrade_audit_is_read_only(tmp_path: Path) -> None:
    db = tmp_path / "scan.sqlite3"
    _v1_db(db)
    before = db.read_bytes()

    out = _run_upgrade(_upgrade_request(db, apply=False, request_id=UPGRADE_REQUEST_ID))
    assert out.returncode == 0, out.stderr.decode(errors="replace")
    payload = json.loads(out.stdout.decode("utf-8"))

    assert payload["status"] == "ok"
    assert payload["apply"] is False
    assert payload["source_user_version"] == 1
    assert payload["target_user_version"] == 2
    assert payload["schema_migrated"] is False
    assert payload["auto_vacuum_converted"] is False
    assert payload["legacy_parse_cache_rows_detected"] == 1
    assert payload["invalidated_parse_cache_rows"] == 0
    assert payload["pre_integrity_check"] == "ok"
    assert payload["post_integrity_check"] == "not_run"
    assert payload["warnings"] == []
    assert payload["error"] is None

    assert db.read_bytes() == before
    assert not list(tmp_path.glob("*.sidecar"))


def test_upgrade_audit_is_read_only_on_wal_database(tmp_path: Path) -> None:
    db = tmp_path / "scan.sqlite3"
    _v1_db_wal(db)
    before = db.read_bytes()
    shm = db.with_name(db.name + "-shm")
    wal = db.with_name(db.name + "-wal")
    assert not shm.exists() and not wal.exists()

    out = _run_upgrade(_upgrade_request(db, apply=False, request_id=UPGRADE_REQUEST_ID))
    assert out.returncode == 0, out.stderr.decode(errors="replace")
    payload = json.loads(out.stdout.decode("utf-8"))

    assert payload["status"] == "ok"
    assert payload["apply"] is False
    assert payload["source_user_version"] == 1
    assert payload["schema_migrated"] is False
    assert payload["auto_vacuum_converted"] is False
    assert payload["legacy_parse_cache_rows_detected"] == 1
    assert payload["invalidated_parse_cache_rows"] == 0
    assert payload["pre_integrity_check"] == "ok"
    assert payload["post_integrity_check"] == "not_run"
    assert payload["error"] is None

    assert db.read_bytes() == before
    assert not shm.exists(), "audit must not leave a WAL -shm sidecar"
    assert not wal.exists(), "audit must not leave a WAL -wal sidecar"
    assert not list(tmp_path.glob("*.sidecar"))


def test_upgrade_apply_migrates_v1_to_v2(tmp_path: Path) -> None:
    db = tmp_path / "scan.sqlite3"
    _v1_db(db)

    out = _run_upgrade(_upgrade_request(db, apply=True, request_id=UPGRADE_REQUEST_ID))
    assert out.returncode == 0, out.stderr.decode(errors="replace")
    payload = json.loads(out.stdout.decode("utf-8"))

    assert payload["status"] == "ok"
    assert payload["apply"] is True
    assert payload["source_user_version"] == 1
    assert payload["schema_migrated"] is True
    assert payload["auto_vacuum_converted"] is True
    assert payload["legacy_parse_cache_rows_detected"] == 1
    assert payload["invalidated_parse_cache_rows"] == 1
    assert payload["pre_integrity_check"] == "ok"
    assert payload["post_integrity_check"] == "ok"
    assert payload["warnings"] == []
    assert payload["error"] is None

    conn = sqlite3.connect(db)
    try:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 2
        history = conn.execute(
            "SELECT origin, upgrade_request_id FROM schema_migration_history WHERE user_version=2"
        ).fetchall()
        assert history == [("upgraded_v1", UPGRADE_REQUEST_ID)]
        assert conn.execute("SELECT count(*) FROM parse_cache").fetchone()[0] == 0
        assert conn.execute(
            "SELECT audit_provenance_version FROM scan_runs WHERE scan_run_id=1"
        ).fetchone()[0] == "migrated_v1"
    finally:
        conn.close()


def test_upgrade_apply_bad_envelope_rolls_back(tmp_path: Path) -> None:
    db = tmp_path / "scan.sqlite3"
    _v1_db(db, bad_envelope=True)

    out = _run_upgrade(_upgrade_request(db, apply=True, request_id=UPGRADE_REQUEST_ID))
    assert out.returncode == 1
    payload = json.loads(out.stdout.decode("utf-8"))

    assert payload["status"] == "error"
    assert payload["source_user_version"] == 1
    assert payload["schema_migrated"] is False
    assert payload["auto_vacuum_converted"] is False
    assert payload["legacy_parse_cache_rows_detected"] == 1
    assert payload["invalidated_parse_cache_rows"] == 0
    assert payload["error"] is not None
    assert payload["error"]["error_code"] == "SCHEMA_MIGRATION_FAILED"
    assert payload["error"]["stage"] == "maintenance"
    assert payload["error"]["file_path"] is None
    assert payload["error"]["backend"] is None

    conn = sqlite3.connect(db)
    try:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 1
        assert conn.execute("SELECT count(*) FROM parse_cache").fetchone()[0] == 1
    finally:
        conn.close()


def test_upgrade_apply_on_v2_is_idempotent_ok(tmp_path: Path) -> None:
    db = tmp_path / "scan.sqlite3"
    _v1_db(db)

    first = _run_upgrade(_upgrade_request(db, apply=True, request_id=UPGRADE_REQUEST_ID))
    assert first.returncode == 0, first.stderr.decode(errors="replace")

    second = _run_upgrade(
        _upgrade_request(
            db,
            apply=True,
            request_id="223e4567-e89b-42d3-a456-426614174001",
        )
    )
    assert second.returncode == 0, second.stderr.decode(errors="replace")
    payload = json.loads(second.stdout.decode("utf-8"))

    assert payload["status"] == "ok"
    assert payload["source_user_version"] == 2
    assert payload["schema_migrated"] is False
    assert payload["auto_vacuum_converted"] is False
    assert payload["legacy_parse_cache_rows_detected"] == 0
    assert payload["invalidated_parse_cache_rows"] == 0
    assert payload["post_integrity_check"] == "not_run"
    assert payload["error"] is None

    conn = sqlite3.connect(db)
    try:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 2
        history_count = conn.execute(
            "SELECT count(*) FROM schema_migration_history WHERE origin='upgraded_v1'"
        ).fetchone()[0]
        assert history_count == 1
    finally:
        conn.close()


def test_upgrade_apply_too_new_fails_closed(tmp_path: Path) -> None:
    db = tmp_path / "scan.sqlite3"
    _v1_db(db)
    conn = sqlite3.connect(db)
    try:
        conn.execute("PRAGMA user_version = 3;")
        conn.commit()
    finally:
        conn.close()

    out = _run_upgrade(_upgrade_request(db, apply=True, request_id=UPGRADE_REQUEST_ID))
    assert out.returncode == 1
    payload = json.loads(out.stdout.decode("utf-8"))

    assert payload["status"] == "error"
    assert payload["source_user_version"] == 3
    assert payload["schema_migrated"] is False
    assert payload["auto_vacuum_converted"] is False
    assert payload["invalidated_parse_cache_rows"] == 0
    assert payload["error"] is not None

    conn = sqlite3.connect(db)
    try:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 3
    finally:
        conn.close()
