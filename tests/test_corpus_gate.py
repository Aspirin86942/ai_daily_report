"""Task 2: fixed-corpus 9-state cache consistency gate tests (spec Part 9.1).

九种 cache 组合（parse × classification = empty/randomized-partial/full）各自独立新 DB，
只按 manifest 预种该组合；运行前 artifact/run 表为空（snapshot lookup 必须 miss）。
门禁：无 deadline 时九态 semantic output（final_context + decisions + semantic counts）
完全一致；text_pdf_coverage=100%；只有 manifest 指定的 NotParsed；safety guard 未触发；
pdfplumber_invocations 等于获得 extraction slot 的 PDF cache misses；no-text 必须 0。
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

import corpus_gate  # noqa: E402

PROJECT_ROOT = Path(__file__).resolve().parents[1]
MANIFEST = PROJECT_ROOT / "scripts" / "corpus_manifest.json"
SCANNER_BIN = PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"
OFFICE_BIN = PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-office-parser.exe"

if not (SCANNER_BIN.is_file() and OFFICE_BIN.is_file()):
    pytest.skip("Rust scanner release binaries are not built", allow_module_level=True)


def test_nine_cache_combo_semantic_output_identical(tmp_path: Path) -> None:
    """九个组合（各独立新 DB）的完整 semantic tuple 完全一致，且门禁全绿。"""
    result = corpus_gate.run_gate(
        scanner=SCANNER_BIN,
        office_worker=OFFICE_BIN,
        work_dir=tmp_path / "corpus",
        out_root=tmp_path / "gate",
        manifest_path=MANIFEST,
    )
    assert result["combo_count"] == 9
    assert result["gates"]["semantic_identical"] is True
    assert result["gates"]["passes"] is True
    assert result["gates"]["calibration_matches_manifest"] is True
    assert result["gates"]["all_text_pdf_coverage_100"] is True
    assert result["gates"]["all_no_runtime_not_parsed"] is True
    assert result["gates"]["all_safety_guard_not_triggered"] is True
    assert result["gates"]["all_pdfplumber_equals_extraction_misses"] is True
    assert result["gates"]["all_no_text_zero_invocations"] is True
    for combo in result["combos"]:
        assert combo["status"] != "error"
        assert combo["semantic_counts"]["error_file_count"] == 0
        assert combo["text_pdf_coverage"] == 1.0
        assert combo["stage_deadline_exhausted_count"] == 0
        assert combo["snapshot_hit"] is False


def test_semantic_key_includes_decisions_and_semantic_counts() -> None:
    """一致性 gate 必须比较 full semantic tuple，不只 context_sha256（brief 强制）。"""
    base = {
        "context_sha256": "0" * 64,
        "decisions": [["notes.md", "keep", "small_file_keep", 30, 10, 10, 0, ""]],
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
    same_decisions = dict(
        base,
        decisions=[["notes.md", "keep", "small_file_keep", 30, 10, 10, 0, ""]],
    )
    different_decisions = dict(
        base,
        decisions=[["other.txt", "keep", "small_file_keep", 30, 10, 10, 0, ""]],
    )
    different_counts = dict(
        base,
        semantic_counts={**base["semantic_counts"], "output_chars": 11},
    )
    assert corpus_gate.semantic_key(base) == corpus_gate.semantic_key(same_decisions)
    assert corpus_gate.semantic_key(base) != corpus_gate.semantic_key(different_decisions)
    assert corpus_gate.semantic_key(base) != corpus_gate.semantic_key(different_counts)


def test_manifest_freezes_expected_fields() -> None:
    """manifest 冻结 discovery/plan/classification truth/semantic/partial subset/seed。"""
    if not MANIFEST.is_file():
        pytest.skip("corpus_manifest.json not generated")
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    assert manifest["schema"] == "corpus_gate_manifest_v1"
    assert set(manifest["combos"]) == set(corpus_gate.COMBOS)
    assert len(manifest["semantic"]["context_sha256"]) == 64
    assert manifest["plan"]["error_file_count"] == 0
    assert manifest["partial"]["seed"]
    assert manifest["partial"]["parse_paths"]
    assert manifest["partial"]["classification_paths"]
    # 每个 PDF 都有 ground truth；admitted text PDF 集合非空（coverage 分母 > 0）
    assert manifest["classification_truth"]
    assert len(manifest["files"]) == manifest["plan"]["source_file_count"]
    assert manifest["plan"]["admitted_file_count"] > 0
    # profile 必须是冻结的 v2 且含不触发 deadline 的 total_deadline_ms
    assert manifest["profile"]["schema_version"] == "scanner_profile_v2"
    assert manifest["profile"]["total_deadline_ms"] >= 60000
