"""Task 4: snapshot warm / cache-only warm benchmark scenario tests (spec Part 6).

在合成 fixture 上端到端验证 7d snapshot warm 与 30d/90d cache-only warm vs
snapshot warm 的语义不变量（snapshot_hit、idempotent_replay=false、cache all-hit、
context/decisions/semantic 完全一致）。绝对阈值（330/400ms、≥20%）依赖机器与
真实语料，属真实目录手工 acceptance，不在 fixture 测试中断言。
"""
from __future__ import annotations

import os
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

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
    for sample in result["samples"]:
        # 每次运行前 request_id 不存在 + 每次新 scan_run_id → idempotent_replay=false
        assert sample["idempotent_replay_false"] is True
        assert sample["snapshot_hit"] is True
        assert sample["context_hash_identical"] is True
    assert result["gates"]["all_snapshot_hit"] is True
    assert result["gates"]["all_idempotent_replay_false"] is True
    assert result["gates"]["all_context_identical_to_cold"] is True


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
