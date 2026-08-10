# scripts/benchmark_seed_preparer.py
"""Isolated cache-only seed DB clone (spec Part 6 / R3-26).

冷扫成功后，scanner 已退出、源 DB 已 checkpoint/close。本 preparer：

1. **只读**源 DB（`mode=ro` URI + `Connection.backup`），复制到本次 harness 新建的
   临时目录；绝不原地清理 cold/用户 DB。
2. 写 marker sidecar：canonical harness root、canonical clone path、随机 nonce、
   复制前源 SHA-256；随后重新 resolve 并校验：clone 是 root 的普通文件后代、
   非 reparse point、与当前配置/default DB 不同、nonce 匹配——任一不符 fail closed。
3. 在 **clone** 中删除 run/attempt/diagnostic/current-audit/context-run/artifact/
   lease rows；保留并逐 key 校验 inventory / parse cache / classification cache；
   要求 `integrity_check=ok`、run/artifact/lease count=0、两类 cache count/hash 与
   cold 后一致；关闭连接并保存 seed SHA-256。

本模块只进入 benchmark 证据，不给 production binary/profile 暴露 snapshot bypass。
"""
from __future__ import annotations

import hashlib
import json
import os
import secrets
import sqlite3
import sys
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

DEFAULT_DB_REL = Path("data") / "db" / "scan_index_v2.sqlite3"
SEED_CLONE_NAME = "scan_index_v2.sqlite3"

# Windows reparse-point attribute (junctions/symlinks).
_FILE_ATTRIBUTE_REPARSE_POINT = 0x400

# Tables that are deleted from the clone. `scan_runs` cascade-deletes its
# current-audit + context_runs rows; `context_artifacts` cascade-deletes its
# owned file/decision rows. Listed explicitly for auditability.
DELETED_TABLES = [
    "engine_lease",
    "scan_runs",
    "context_artifacts",
]


class SeedPreparerError(RuntimeError):
    """A fail-closed condition rejected a seed clone."""


def _now_ms() -> int:
    return int(time.time() * 1000)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(128 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _resolve(path: Path) -> Path:
    return Path(os.path.abspath(str(path)))


def _is_reparse_point(path: Path) -> bool:
    """True when the file/dir is a reparse point (symlink/junction on Windows)."""
    if os.name != "nt":
        return path.is_symlink()
    try:
        stat = os.lstat(path)
    except FileNotFoundError:
        return False
    return bool(getattr(stat, "st_file_attributes", 0) & _FILE_ATTRIBUTE_REPARSE_POINT)


def forbidden_db_paths() -> list[Path]:
    """当前配置/default DB 的 canonical 路径集合（seed 永不指向它们）。"""
    forbidden: list[Path] = [DEFAULT_DB_REL.resolve()]
    try:
        from src.core.config import config

        forbidden.append(Path(config.rust_index_db_path).resolve())
        if config.installed_mode:
            forbidden.append(Path(config.db_dir).resolve() / "scan_index_v2.sqlite3")
    except Exception:
        # 配置不可读时仍保留 default；fail closed 是保守方向。
        pass
    return forbidden


def _is_forbidden_db(path: Path) -> bool:
    resolved = _resolve(path)
    return any(resolved == _resolve(f) for f in forbidden_db_paths())


# ---------------------------------------------------------------------------
# marker read/write
# ---------------------------------------------------------------------------


def write_seed_marker(
    *,
    marker_path: Path,
    harness_root: Path,
    clone_path: Path,
    source_path: Path,
    source_sha256: str,
    nonce: str,
    seed_sha256: str | None = None,
) -> None:
    marker_path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema": "cache_only_seed_marker_v1",
        "harness_root": str(_resolve(harness_root)),
        "clone_path": str(_resolve(clone_path)),
        "source_path": str(_resolve(source_path)),
        "source_sha256": source_sha256,
        "nonce": nonce,
        "created_at_ms": _now_ms(),
        "seed_sha256": seed_sha256,
    }
    marker_path.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2),
        encoding="utf-8",
    )


def read_seed_marker(marker_path: Path) -> dict:
    try:
        payload = json.loads(marker_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SeedPreparerError(
            f"seed marker {marker_path} is unreadable: {error}"
        ) from error
    if not isinstance(payload, dict):
        raise SeedPreparerError("seed marker is not a JSON object")
    return payload


# ---------------------------------------------------------------------------
# fail-closed re-verification
# ---------------------------------------------------------------------------


def verify_cache_only_seed_marker(
    marker_path: Path,
    out_dir: Path,
    *,
    expected_nonce: str | None = None,
) -> dict:
    """Re-resolve the marker and fail closed on any mismatch.

    Conditions:
    - marker clone path re-resolves to the same canonical path as
      ``out_dir / SEED_CLONE_NAME``;
    - the clone is a plain file descendant of the marker's harness root;
    - the clone is not a reparse point;
    - the clone is not the current config/default DB;
    - the marker nonce matches ``expected_nonce`` when provided;
    - when the marker records a seed SHA-256, the live clone file hash matches
      (the seed was not mutated after processing).
    """
    marker = read_seed_marker(marker_path)
    if marker.get("schema") != "cache_only_seed_marker_v1":
        raise SeedPreparerError("seed marker schema is not cache_only_seed_marker_v1")

    root = _resolve(Path(marker.get("harness_root", "")))
    clone = _resolve(Path(marker.get("clone_path", "")))
    expected_clone = _resolve(out_dir / SEED_CLONE_NAME)

    # nonce
    nonce = marker.get("nonce")
    if not isinstance(nonce, str) or len(nonce) < 16:
        raise SeedPreparerError("seed marker nonce is missing or too short")
    if expected_nonce is not None and not secrets.compare_digest(nonce, expected_nonce):
        raise SeedPreparerError("seed marker nonce mismatch")

    # canonical clone path
    if clone != expected_clone:
        raise SeedPreparerError(
            f"seed clone path {clone} does not match out_dir clone {expected_clone}"
        )
    # clone must be a plain file descendant of the harness root
    if not clone.is_relative_to(root):
        raise SeedPreparerError(f"seed clone {clone} is not under harness root {root}")
    if not clone.is_file():
        raise SeedPreparerError(f"seed clone {clone} is not a plain file")
    if _is_reparse_point(clone):
        raise SeedPreparerError(f"seed clone {clone} is a reparse point")
    if _is_forbidden_db(clone):
        raise SeedPreparerError(f"seed clone {clone} matches the config/default DB")

    # seed SHA-256: processed clone hash must match the marker record
    seed_sha256 = marker.get("seed_sha256")
    if seed_sha256:
        if not isinstance(seed_sha256, str) or len(seed_sha256) != 64:
            raise SeedPreparerError("seed marker seed_sha256 is malformed")
        if sha256_file(clone) != seed_sha256:
            raise SeedPreparerError(
                f"seed clone {clone} content no longer matches marker seed_sha256"
            )
    return marker


# ---------------------------------------------------------------------------
# cache snapshot / verification
# ---------------------------------------------------------------------------

_CACHE_TABLES: dict[str, tuple[list[str], list[str]]] = {
    "file_inventory": (
        [
            "file_identity",
            "absolute_path",
            "relative_path",
            "file_type",
            "source_version",
        ],
        ["file_identity"],
    ),
    "parse_cache": (
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
    "classification_cache": (
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
}


def snapshot_caches(conn: sqlite3.Connection) -> dict[str, dict]:
    """Per-key canonical hash + count of the three preserved tables."""
    result: dict[str, dict] = {}
    for table, (columns, pk) in _CACHE_TABLES.items():
        rows = conn.execute(
            f"SELECT {', '.join(columns)} FROM {table} ORDER BY {', '.join(pk)}"
        ).fetchall()
        digest = hashlib.sha256()
        for row in rows:
            digest.update(
                json.dumps(
                    list(row), ensure_ascii=False, separators=(",", ":")
                ).encode("utf-8")
            )
        result[table] = {
            "count": len(rows),
            "sha256": digest.hexdigest(),
        }
    return result


def verify_caches(conn: sqlite3.Connection) -> None:
    """Per-key validation: every cache row's file_identity must exist in inventory."""
    for cache in ("parse_cache", "classification_cache"):
        orphaned = conn.execute(
            f"SELECT count(*) FROM {cache} c"
            " LEFT JOIN file_inventory fi USING (file_identity)"
            " WHERE fi.file_identity IS NULL"
        ).fetchone()[0]
        if orphaned:
            raise SeedPreparerError(
                f"{cache} has {orphaned} rows without a file_inventory reference"
            )


# ---------------------------------------------------------------------------
# SQLite copy + clone processing
# ---------------------------------------------------------------------------


def copy_sqlite_db(src: Path, dest: Path) -> None:
    """Consistent snapshot copy via the SQLite online-backup API."""
    dest = _resolve(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    src_conn = sqlite3.connect(
        f"file:{_resolve(src).as_posix()}?mode=ro", uri=True, timeout=30
    )
    try:
        dest_conn = sqlite3.connect(dest, timeout=30)
        try:
            src_conn.backup(dest_conn)
        finally:
            dest_conn.close()
    finally:
        src_conn.close()


def _checkpoint_main_file(path: Path) -> None:
    """Checkpoint/truncate WAL so the main file reflects the logical state."""
    conn = sqlite3.connect(path, timeout=30)
    try:
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    finally:
        conn.close()


def _delete_run_artifact_lease(conn: sqlite3.Connection) -> None:
    """Delete run/attempt/diagnostic/current-audit/context-run/artifact/lease rows.

    `scan_runs` cascade-deletes its current-audit rows (attempts, diagnostics,
    file_results, stage/extension/execution metrics) and `context_runs` (+ its
    context_decisions); `context_artifacts` cascade-deletes its owned
    file/decision rows. `file_inventory.last_seen_run_id` is SET NULL.
    """
    with conn:
        for table in DELETED_TABLES:
            conn.execute(f"DELETE FROM {table}")
        # Every affected table must now be empty.
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
            count = conn.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
            if count:
                raise SeedPreparerError(
                    f"expected {table} to be empty after clone processing, got {count}"
                )


def prepare_cache_only_seed(
    *,
    src: Path,
    out_dir: Path,
    marker: Path,
) -> dict:
    """Full seed flow: read-only copy -> marker -> fail-closed verify -> process.

    Returns aggregate evidence (no real paths/file bodies are required by
    callers beyond the resolved paths already in the marker).
    """
    src = _resolve(src)
    if not src.is_file():
        raise SeedPreparerError(f"source DB {src} is not a file")
    if _is_forbidden_db(src):
        raise SeedPreparerError(f"source DB {src} matches the config/default DB")
    if src.is_relative_to(_resolve(out_dir)):
        raise SeedPreparerError("source DB must live outside the clone out_dir")

    out_dir = _resolve(out_dir)
    if out_dir.exists():
        raise SeedPreparerError(
            f"clone out_dir {out_dir} already exists; refusing to reuse a stale dir"
        )
    out_dir.mkdir(parents=True)

    source_sha256 = sha256_file(src)

    # Verify the source is a v2 scanner DB before copying (fail closed early).
    probe = sqlite3.connect(f"file:{src.as_posix()}?mode=ro", uri=True)
    try:
        version = probe.execute("PRAGMA user_version").fetchone()[0]
        if version != 2:
            raise SeedPreparerError(f"source DB user_version={version}, expected 2")
    finally:
        probe.close()

    clone_path = out_dir / SEED_CLONE_NAME
    copy_sqlite_db(src, clone_path)

    # Post-cold reference cache state from the consistent copy.
    reference = sqlite3.connect(clone_path, timeout=30)
    try:
        cold_cache_state = snapshot_caches(reference)
        verify_caches(reference)
    finally:
        reference.close()

    # Write marker (random nonce + source sha before copy), then re-verify.
    nonce = secrets.token_hex(24)
    write_seed_marker(
        marker_path=marker,
        harness_root=out_dir,
        clone_path=clone_path,
        source_path=src,
        source_sha256=source_sha256,
        nonce=nonce,
        seed_sha256=None,
    )
    verify_cache_only_seed_marker(marker, out_dir, expected_nonce=nonce)

    # Process the clone: delete run/artifact/lease rows, keep caches.
    conn = sqlite3.connect(clone_path, timeout=30)
    try:
        conn.execute("PRAGMA foreign_keys = ON")
        _delete_run_artifact_lease(conn)
        integrity = conn.execute("PRAGMA integrity_check").fetchone()[0]
        if integrity != "ok":
            raise SeedPreparerError(
                f"seed clone integrity_check={integrity!r}, expected 'ok'"
            )
        kept_state = snapshot_caches(conn)
        verify_caches(conn)
    finally:
        conn.close()

    # Cache count/hash must match the post-cold state.
    for table in ("file_inventory", "parse_cache", "classification_cache"):
        if kept_state[table] != cold_cache_state[table]:
            raise SeedPreparerError(
                f"seed clone {table} state changed: {kept_state[table]} != {cold_cache_state[table]}"
            )

    # Checkpoint so the main file reflects the processed state, then save seed SHA.
    _checkpoint_main_file(clone_path)
    seed_sha256 = sha256_file(clone_path)
    write_seed_marker(
        marker_path=marker,
        harness_root=out_dir,
        clone_path=clone_path,
        source_path=src,
        source_sha256=source_sha256,
        nonce=nonce,
        seed_sha256=seed_sha256,
    )
    verify_cache_only_seed_marker(marker, out_dir, expected_nonce=nonce)

    return {
        "source_sha256": source_sha256,
        "seed_sha256": seed_sha256,
        "nonce": nonce,
        "harness_root": str(_resolve(out_dir)),
        "clone_path": str(clone_path),
        "cold_cache_state": cold_cache_state,
        "kept_cache_state": kept_state,
        "integrity_check": integrity,
        "deleted_tables": DELETED_TABLES,
    }


def main(argv: list[str] | None = None) -> int:
    import argparse

    parser = argparse.ArgumentParser(
        description="Prepare an isolated cache-only seed DB clone."
    )
    parser.add_argument("--src", type=Path, required=True, help="cold-run source DB")
    parser.add_argument("--out-dir", type=Path, required=True, help="fresh clone dir")
    parser.add_argument("--marker", type=Path, required=True, help="marker sidecar path")
    args = parser.parse_args(argv)
    try:
        result = prepare_cache_only_seed(
            src=args.src, out_dir=args.out_dir, marker=args.marker
        )
    except SeedPreparerError as error:
        print(f"seed preparer failed closed: {error}")
        return 1
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
