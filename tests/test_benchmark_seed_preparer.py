"""Task 4: isolated cache-only seed preparer tests (spec Part 6 / R3-26).

- seed 克隆只读源 DB，删除 run/attempt/diagnostic/current-audit/context-run/
  artifact/lease rows，保留并逐 key 校验 inventory / parse cache / classification cache；
- 源只读（clone 前后源 SHA 不变）；
- path escape / reparse point / nonce 错配 → fail closed。
"""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import uuid
from datetime import date
from pathlib import Path

import pytest

from benchmark_seed_preparer import (
    SeedPreparerError,
    _is_reparse_point,
    prepare_cache_only_seed,
    sha256_file,
    verify_cache_only_seed_marker,
)

PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCANNER_BIN = PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"
OFFICE_BIN = (
    PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-office-parser.exe"
)

if not (SCANNER_BIN.is_file() and OFFICE_BIN.is_file()):
    pytest.skip(
        "Rust scanner release binaries are not built",
        allow_module_level=True,
    )


def _cache_counts(conn: sqlite3.Connection) -> dict[str, int]:
    return {
        "inventory": conn.execute("SELECT count(*) FROM file_inventory").fetchone()[0],
        "parse_cache": conn.execute("SELECT count(*) FROM parse_cache").fetchone()[0],
        "classification_cache": conn.execute(
            "SELECT count(*) FROM classification_cache"
        ).fetchone()[0],
    }


def _cache_hash(conn: sqlite3.Connection) -> dict[str, str]:
    """Canonical per-key hash over the cache tables (PK order)."""
    result: dict[str, str] = {}
    for table, columns, pk in [
        (
            "file_inventory",
            ["file_identity", "absolute_path", "relative_path", "file_type", "source_version"],
            ["file_identity"],
        ),
        (
            "parse_cache",
            [
                "file_identity",
                "source_version",
                "source_guard_kind",
                "source_guard_sha256",
                "parse_profile_hash",
                "content",
                "content_sha256",
                "parser_backend",
                "worker_lane",
                "truncated",
            ],
            [
                "file_identity",
                "source_version",
                "source_guard_kind",
                "source_guard_sha256",
                "parse_profile_hash",
            ],
        ),
        (
            "classification_cache",
            [
                "file_identity",
                "source_version",
                "source_guard_kind",
                "source_guard_sha256",
                "classifier_profile_hash",
                "classifier_build",
                "status",
                "page_count",
                "result_examined_pages",
            ],
            [
                "file_identity",
                "source_version",
                "source_guard_kind",
                "source_guard_sha256",
                "classifier_profile_hash",
                "classifier_build",
            ],
        ),
    ]:
        rows = conn.execute(
            f"SELECT {', '.join(columns)} FROM {table} "
            f"ORDER BY {', '.join(pk)}"
        ).fetchall()
        digest = hashlib.sha256()
        for row in rows:
            digest.update(
                json.dumps(list(row), ensure_ascii=False, separators=(",", ":")).encode(
                    "utf-8"
                )
            )
        result[table] = digest.hexdigest()
    return result


def _run_cold(tmp_path: Path) -> tuple[Path, str]:
    """一次成功 cold run（synthetic fixture），返回 (db_path, source_sha256)。"""
    work_dir = tmp_path / "corpus"
    work_dir.mkdir()
    (work_dir / "a.txt").write_text("evidence a line", encoding="utf-8")
    (work_dir / "b.txt").write_text("evidence b content", encoding="utf-8")
    src_pdf = (
        PROJECT_ROOT
        / "tests"
        / "fixtures"
        / "pdf_classifier"
        / "no_text_blank_01.pdf"
    )
    if src_pdf.is_file():
        shutil.copyfile(src_pdf, work_dir / "scan.pdf")

    db_dir = tmp_path / "cold_db"
    db_dir.mkdir()
    db_path = db_dir / "scan_index_v2.sqlite3"
    request_id = str(uuid.uuid4())
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": request_id,
        "work_dir": str(work_dir),
        "start_date": date(2000, 1, 1).isoformat(),
        "end_date": date(2099, 12, 31).isoformat(),
        "report_mode": "weekly",
        "compression_profile": None,
        "scan_db_path": str(db_path),
        "scanner_profile": {"schema_version": "scanner_profile_v1"},
        "adapters": {
            "office_worker_path": str(OFFICE_BIN),
            "python_executable": str(Path(sys.executable).resolve()),
            "python_module_root": str(PROJECT_ROOT),
            "python_document_worker_module": "src.workers.document_parser_worker",
        },
    }
    proc = subprocess.run(
        [str(SCANNER_BIN), "build-context"],
        input=json.dumps(request).encode("utf-8"),
        capture_output=True,
        timeout=180,
    )
    envelope = json.loads(proc.stdout.decode("utf-8", errors="replace"))
    assert proc.returncode == 0, envelope
    assert envelope.get("status") == "ok", envelope

    # checkpoint/close so the main file reflects the post-cold logical state.
    conn = sqlite3.connect(db_path)
    try:
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    finally:
        conn.close()
    return db_path, sha256_file(db_path)


def test_seed_clone_keeps_caches_and_zeroes_runs(tmp_path: Path) -> None:
    cold_db, before = _run_cold(tmp_path)
    seed_dir = tmp_path / "seed"
    marker = tmp_path / "marker"

    result = prepare_cache_only_seed(src=cold_db, out_dir=seed_dir, marker=marker)

    # 源只读：clone 前后源 SHA 完全一致
    assert sha256_file(cold_db) == before

    clone = seed_dir / "scan_index_v2.sqlite3"
    assert clone.is_file()
    conn = sqlite3.connect(clone)
    try:
        # 保留 inventory/parse/classification cache，且 count/hash 与 cold 后一致
        src_conn = sqlite3.connect(f"file:{cold_db.as_posix()}?mode=ro", uri=True)
        try:
            assert _cache_counts(conn) == _cache_counts(src_conn)
            assert _cache_hash(conn) == _cache_hash(src_conn)
        finally:
            src_conn.close()
        assert conn.execute("SELECT count(*) FROM file_inventory").fetchone()[0] > 0
        # run/attempt/diagnostic/current-audit/context-run/artifact/lease count=0
        for table in [
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
        ]:
            assert conn.execute(f"SELECT count(*) FROM {table}").fetchone()[0] == 0, (
                f"{table} must be empty in the seed"
            )
        # integrity_check=ok
        assert conn.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
        # 逐 key 校验：cache 行引用的 inventory 必须存在
        assert (
            conn.execute(
                "SELECT count(*) FROM parse_cache pc"
                " LEFT JOIN file_inventory fi USING (file_identity)"
                " WHERE fi.file_identity IS NULL"
            ).fetchone()[0]
            == 0
        )
        assert (
            conn.execute(
                "SELECT count(*) FROM classification_cache cc"
                " LEFT JOIN file_inventory fi USING (file_identity)"
                " WHERE fi.file_identity IS NULL"
            ).fetchone()[0]
            == 0
        )
    finally:
        conn.close()

    # seed SHA 已保存且可复验
    assert "seed_sha256" in result
    assert result["seed_sha256"] == sha256_file(clone)
    verify_cache_only_seed_marker(marker, seed_dir)


def test_preparer_fails_closed_on_path_escape(tmp_path: Path) -> None:
    cold_db, _ = _run_cold(tmp_path)
    seed_dir = tmp_path / "seed"
    marker = tmp_path / "marker"
    result = prepare_cache_only_seed(src=cold_db, out_dir=seed_dir, marker=marker)
    clone = seed_dir / "scan_index_v2.sqlite3"
    original_bytes = clone.read_bytes()

    def _rewrite_marker(mutate) -> None:
        data = json.loads(marker.read_text(encoding="utf-8"))
        mutate(data)
        marker.write_text(json.dumps(data), encoding="utf-8")

    # 1) nonce 错配
    _rewrite_marker(lambda data: data.update(nonce="deadbeef" * 4))
    with pytest.raises(SeedPreparerError):
        verify_cache_only_seed_marker(marker, seed_dir, expected_nonce=result["nonce"])
    # 修正 nonce，继续后续场景
    _rewrite_marker(lambda data: data.update(nonce=result["nonce"]))

    # 2) clone 不是 root 的普通文件后代（指向 root 之外）
    _rewrite_marker(
        lambda data: data.update(
            clone_path=str((tmp_path / "outside" / "scan_index_v2.sqlite3").resolve())
        )
    )
    with pytest.raises(SeedPreparerError):
        verify_cache_only_seed_marker(marker, seed_dir)
    _rewrite_marker(
        lambda data: data.update(clone_path=str((seed_dir / "scan_index_v2.sqlite3").resolve()))
    )

    # 3) reparse point：clone 被替换成指向他处的 junction/symlink
    junction_dir = tmp_path / "junction_target"
    junction_dir.mkdir()
    os.replace(clone, seed_dir / "clone_backup.sqlite3")
    try:
        if os.name == "nt":
            # 目录 junction 不需要管理员权限；无法创建文件 symlink 时用它。
            import subprocess

            result = subprocess.run(
                ["cmd", "/c", "mklink", "/J", str(clone), str(junction_dir)],
                capture_output=True,
                text=True,
            )
            if result.returncode != 0:
                os.replace(seed_dir / "clone_backup.sqlite3", clone)
                pytest.skip(f"junction creation failed: {result.stderr.strip()}")
        else:
            os.symlink(junction_dir, clone)
    except OSError:
        os.replace(seed_dir / "clone_backup.sqlite3", clone)
        pytest.skip("reparse point creation is not available on this platform")
    try:
        assert _is_reparse_point(clone) is True
        with pytest.raises(SeedPreparerError):
            verify_cache_only_seed_marker(marker, seed_dir)
    finally:
        # 目录 junction 只能被当作目录删除（rmtree 会拒绝 symlink）。
        try:
            os.rmdir(clone)
        except OSError:
            os.unlink(clone)
        shutil.rmtree(junction_dir)
        os.replace(seed_dir / "clone_backup.sqlite3", clone)

    # 4) clone 是目录（非普通文件）
    os.replace(clone, seed_dir / "clone_backup2.sqlite3")
    clone.mkdir()
    try:
        with pytest.raises(SeedPreparerError):
            verify_cache_only_seed_marker(marker, seed_dir)
    finally:
        shutil.rmtree(clone)
        os.replace(seed_dir / "clone_backup2.sqlite3", clone)

    # 5) marker 记录的 clone 已被替换内容（seed_sha256 不匹配）
    clone.write_bytes(b"tampered")
    try:
        with pytest.raises(SeedPreparerError):
            verify_cache_only_seed_marker(marker, seed_dir)
    finally:
        clone.write_bytes(original_bytes)

    # 恢复后可复验
    verify_cache_only_seed_marker(marker, seed_dir)


def test_preparer_rejects_forbidden_default_config_db(tmp_path: Path) -> None:
    """clone 等于当前配置/default DB 时 fail closed。"""
    cold_db, _ = _run_cold(tmp_path)
    seed_dir = tmp_path / "seed"
    marker = tmp_path / "marker"
    result = prepare_cache_only_seed(src=cold_db, out_dir=seed_dir, marker=marker)
    default_db = PROJECT_ROOT / "data" / "db" / "scan_index_v2.sqlite3"
    data = json.loads(marker.read_text(encoding="utf-8"))
    data["clone_path"] = str(default_db.resolve())
    data["nonce"] = result["nonce"]
    marker.write_text(json.dumps(data), encoding="utf-8")
    with pytest.raises(SeedPreparerError):
        verify_cache_only_seed_marker(marker, seed_dir)
