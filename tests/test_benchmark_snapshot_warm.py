"""Task 4: snapshot warm / cache-only warm benchmark scenario tests (spec Part 6).

在合成 fixture 上端到端验证 7d snapshot warm 与 30d/90d cache-only warm vs
snapshot warm 的语义不变量（snapshot_hit、idempotent_replay=false、cache all-hit、
context/decisions/semantic 完全一致）。绝对阈值（370/420ms、≥20%）依赖机器与
真实语料，属真实目录手工 acceptance，不在 fixture 测试中断言。
"""
from __future__ import annotations

import os
import shutil
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

import benchmark_snapshot_warm as b  # noqa: E402
from benchmark_snapshot_warm import (  # noqa: E402
    run_7d_snapshot_warm,
    run_cache_only_vs_snapshot,
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


def _build_fixture(root: Path) -> Path:
    """Synthetic corpus whose mtimes fall inside the 7d/30d/90d windows."""
    work_dir = root / "corpus"
    work_dir.mkdir()
    mtime = datetime(2026, 8, 5, 10, 0, 0, tzinfo=timezone.utc).timestamp()
    for name, content in [
        ("a.txt", "evidence line a"),
        ("b.txt", "evidence line b longer than a"),
    ]:
        path = work_dir / name
        path.write_text(content, encoding="utf-8")
        os.utime(path, (mtime, mtime))
    src_pdf = (
        PROJECT_ROOT
        / "tests"
        / "fixtures"
        / "pdf_classifier"
        / "no_text_blank_01.pdf"
    )
    if src_pdf.is_file():
        dest = work_dir / "scan.pdf"
        shutil.copyfile(src_pdf, dest)
        os.utime(dest, (mtime, mtime))
    return work_dir


@pytest.mark.perf_gate
def test_7d_snapshot_warm_semantics(tmp_path: Path) -> None:
    scanner = SCANNER_BIN
    office_worker = OFFICE_BIN
    work_dir = _build_fixture(tmp_path)
    out_root = tmp_path / "out7d"

    result = run_7d_snapshot_warm(
        scanner=scanner,
        office_worker=office_worker,
        work_dir=work_dir,
        out_root=out_root,
    )

    assert len(result["samples"]) == 3
    assert result["gates"]["cold_runs_clean"] is True
    for sample in result["samples"]:
        # 每次运行前 request_id 不存在 + 每次新 scan_run_id → idempotent_replay=false
        assert sample["idempotent_replay_false"] is True
        assert sample["snapshot_hit"] is True
        assert sample["context_hash_identical"] is True
    assert result["gates"]["all_snapshot_hit"] is True
    assert result["gates"]["all_idempotent_replay_false"] is True
    assert result["gates"]["all_context_identical_to_cold"] is True


@pytest.mark.perf_gate
def test_30d_cache_only_vs_snapshot_semantics(tmp_path: Path) -> None:
    scanner = SCANNER_BIN
    office_worker = OFFICE_BIN
    work_dir = _build_fixture(tmp_path)
    out_root = tmp_path / "out30d"

    result = run_cache_only_vs_snapshot(
        scanner=scanner,
        office_worker=office_worker,
        work_dir=work_dir,
        label="30d",
        out_root=out_root,
    )

    assert len(result["cache_only_warm_samples"]) == 3
    assert len(result["snapshot_warm_samples"]) == 3
    assert result["gates"]["cold_runs_clean"] is True
    assert result["warm_comparison"] == "completed"
    assert result["gates"]["semantic_identical"] is True
    for sample in result["cache_only_warm_samples"]:
        assert sample["snapshot_hit"] is False
        assert sample["parse_cache_lookup_count"] > 0
        assert sample["classification_cache_lookup_count"] > 0
        assert sample["parse_cache_all_hit"] is True
        assert sample["classification_cache_all_hit"] is True
    for sample in result["snapshot_warm_samples"]:
        assert sample["snapshot_hit"] is True
        assert sample["idempotent_replay_false"] is True
        assert sample["context_identical_to_cold"] is True
    assert result["gates"]["cache_warm_all_snapshot_miss_and_cache_all_hit"] is True
    assert result["gates"]["snapshot_warm_all_snapshot_hit"] is True
    assert result["gates"]["semantic_identical"] is True
    # seed 证据齐全：cache count/hash 已随 marker 校验
    assert result["seed"]["seed_sha256"]
    assert "parse_cache" in result["seed"]["cold_cache_state"]


def test_semantic_key_includes_decisions_and_semantic_counts() -> None:
    """一致性 gate 必须比较 full tuple，不只 context_sha256（brief 强制）。"""
    base = {
        "context_sha256": "0" * 64,
        "decisions": [
            ["file-a", "keep", "small_file_keep", 20, 10, 10, 0, ""]
        ],
        "semantic_counts": {
            "source_file_count": 1,
            "success_count": 1,
            "timeout_count": 0,
            "included_file_count": 1,
            "omitted_file_count": 0,
            "error_file_count": 0,
            "input_chars": 10,
            "output_chars": 10,
        },
    }
    same_sha_different_decisions = dict(
        base,
        decisions=[["file-b", "keep", "small_file_keep", 20, 10, 10, 0, ""]],
    )
    same_sha_different_counts = dict(
        base,
        semantic_counts={**base["semantic_counts"], "output_chars": 11},
    )
    assert b.semantic_key(base) == b.semantic_key(
        dict(base, decisions=[list(row) for row in base["decisions"]])
    )
    assert b.semantic_key(base) != b.semantic_key(same_sha_different_decisions)
    assert b.semantic_key(base) != b.semantic_key(same_sha_different_counts)


def test_7d_target_matches_the_redirected_spec() -> None:
    assert b.SEVEN_D_TARGETS == {"median_ms_le": 370.0, "max_ms_le": 420.0}


def test_snapshot_evidence_rejects_path_bearing_fields(tmp_path: Path) -> None:
    b.assert_portable_evidence(
        {"schema": "snapshot_warm_benchmark_v2", "corpus": "external-corpus"},
        forbidden_paths=(tmp_path,),
    )
    with pytest.raises(ValueError, match="path-bearing evidence key"):
        b.assert_portable_evidence(
            {"corpus": {"work_dir": str(tmp_path)}},
            forbidden_paths=(tmp_path,),
        )


def test_30d_records_non_clean_cold_without_raising(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """deadline-exhausted cold（Partial + inspect RUN_CORRUPT）如实记录而非 raise。"""
    work_dir = _build_fixture(tmp_path)
    out_root = tmp_path / "out"

    def fake_build_context(**kwargs):
        db_path = kwargs["db_path"]
        db_path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(db_path)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS file_inventory"
            " (file_identity TEXT, source_version TEXT, size_bytes INTEGER)"
        )
        conn.execute("INSERT OR IGNORE INTO file_inventory VALUES ('x','v',1)")
        conn.commit()
        conn.close()
        return {
            "wall_ms": 25100.0,
            "exit_code": 0,
            "request_id": kwargs["request_id"],
            "validated": True,
            "status": "partial",
            "scan_run_id": 1,
            "context_run_id": 1,
        }

    def fake_inspect(scanner, db_path, scan_run_id):
        return {"ok": False, "payload": {}, "error": "RUN_CORRUPT"}

    monkeypatch.setattr(b, "run_build_context", fake_build_context)
    monkeypatch.setattr(b, "run_inspect_v2", fake_inspect)

    result = run_cache_only_vs_snapshot(
        scanner=SCANNER_BIN,
        office_worker=OFFICE_BIN,
        work_dir=work_dir,
        label="30d",
        out_root=out_root,
    )
    assert len(result["cold"]) == 3
    assert result["warm_comparison"] == "skipped_cold_not_clean"
    assert result["gates"]["cold_runs_clean"] is False
    assert result["gates"]["passes"] is False
    assert all(item["inspect_error"] == "RUN_CORRUPT" for item in result["cold"])
    assert all(item["status"] == "partial" for item in result["cold"])
    assert all(item["deadline_exceeded"] for item in result["cold"])
