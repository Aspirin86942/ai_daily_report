"""Task 10 synthetic parity/cache gate and performance threshold tests."""

from __future__ import annotations

from datetime import date, datetime
import json
from pathlib import Path
import sys

import pytest

from scripts.scanner_cutover_gate import (
    _process_delta_evidence,
    _temporary_real_comparison_root,
    _wait_for_scanner_processes,
    all_cutover_gates_pass,
    build_gate_config,
    create_synthetic_corpus,
    run_cache_gate,
    run_parity_gate,
    run_performance_gate,
    summarize_performance,
)
from src.core.config import config


PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCANNER_BIN = (
    PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"
)
OFFICE_BIN = (
    PROJECT_ROOT
    / "rust"
    / "target"
    / "release"
    / "ai-daily-office-parser.exe"
)
REQUIRED_EXTENSIONS = {
    ".csv",
    ".docx",
    ".json",
    ".log",
    ".md",
    ".pdf",
    ".pptx",
    ".txt",
    ".xlsx",
}
CONTEXT_GOLDEN = (
    PROJECT_ROOT
    / "tests"
    / "fixtures"
    / "scanner_cutover"
    / "task10_expected_context_hashes.json"
)


def _require_release_binaries() -> None:
    if SCANNER_BIN.is_file() and OFFICE_BIN.is_file():
        return
    if sys.platform == "win32":
        pytest.fail("Task 10 requires the Windows Rust release binaries")
    pytest.skip("Task 10 Windows release binaries are unavailable")


def test_synthetic_corpus_covers_required_formats_and_fault_shapes(
    tmp_path: Path,
) -> None:
    work_dir = tmp_path / "synthetic corpus"
    work_dir.mkdir()

    fixture_date = date(2026, 7, 15)
    manifest = create_synthetic_corpus(work_dir, scan_date=fixture_date)

    assert {Path(path).suffix.lower() for path in manifest["scanned_files"]} == (
        REQUIRED_EXTENSIONS
    )
    assert manifest["corrupt_office_count"] == 1
    assert manifest["oversized_file_count"] == 1
    assert manifest["slow_worker_fixture_count"] == 1
    assert any(" " in path for path in manifest["scanned_files"])
    assert any("\u4e2d\u6587" in path for path in manifest["scanned_files"])
    assert {
        datetime.fromtimestamp(path.stat().st_mtime).date()
        for path in work_dir.rglob("*")
        if path.is_file()
    } == {fixture_date}


def test_performance_summary_enforces_all_three_regression_limits() -> None:
    passing = summarize_performance(
        legacy_cold=[100, 100, 100, 100, 100],
        legacy_warm=[50, 50, 50, 50, 50],
        rust_cold=[105, 105, 105, 105, 105],
        rust_warm=[49, 49, 49, 49, 49],
    )
    cold_median_failure = summarize_performance(
        legacy_cold=[100, 100, 100, 100, 100],
        legacy_warm=[50, 50, 50, 50, 50],
        rust_cold=[111, 111, 111, 111, 111],
        rust_warm=[49, 49, 49, 49, 49],
    )
    warm_failure = summarize_performance(
        legacy_cold=[100, 100, 100, 100, 100],
        legacy_warm=[50, 50, 50, 50, 50],
        rust_cold=[100, 100, 100, 100, 100],
        rust_warm=[51, 51, 51, 51, 51],
    )

    assert passing["passed"] is True
    assert passing["criteria"] == {
        "cold_median_within_10_percent": True,
        "cold_p95_within_20_percent": True,
        "warm_median_no_regression": True,
    }
    assert cold_median_failure["passed"] is False
    assert cold_median_failure["criteria"][
        "cold_median_within_10_percent"
    ] is False
    assert warm_failure["passed"] is False
    assert warm_failure["criteria"]["warm_median_no_regression"] is False


def test_cutover_result_requires_every_gate_and_no_process_growth() -> None:
    passing = {"passed": True}

    assert all_cutover_gates_pass(passing, passing, passing) is True
    assert all_cutover_gates_pass(passing, {"passed": False}, passing) is False
    assert all_cutover_gates_pass() is False

    no_growth = _process_delta_evidence(
        {
            "scanner": [],
            "office": [10],
            "discovery": [],
            "python_document_worker": [],
            "legacy_spawn_worker": [],
            "python_process": [20],
            "rust_target_process": [],
        },
        {
            "scanner": [],
            "office": [],
            "discovery": [],
            "python_document_worker": [],
            "legacy_spawn_worker": [],
            "python_process": [20],
            "rust_target_process": [],
        },
    )
    orphan = _process_delta_evidence(
        {
            "scanner": [],
            "office": [10],
            "discovery": [],
            "python_document_worker": [],
            "legacy_spawn_worker": [],
            "python_process": [20],
            "rust_target_process": [],
        },
        {
            "scanner": [],
            "office": [11],
            "discovery": [],
            "python_document_worker": [],
            "legacy_spawn_worker": [],
            "python_process": [20],
            "rust_target_process": [],
        },
    )
    assert no_growth["passed"] is True
    assert orphan["passed"] is False


def test_process_wait_uses_pid_sets_and_real_temp_content_is_removed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    baseline = {
        "scanner": [],
        "office": [],
        "discovery": [],
        "python_document_worker": [],
        "legacy_spawn_worker": [],
        "python_process": [20],
        "rust_target_process": [],
    }
    monkeypatch.setattr(
        "scripts.scanner_cutover_gate._scanner_process_snapshot",
        lambda: dict(baseline),
    )

    snapshot, settled = _wait_for_scanner_processes(baseline)

    assert settled is True
    assert snapshot == baseline

    with _temporary_real_comparison_root() as temporary_root:
        retained_path = temporary_root
        (temporary_root / "content-bearing.sqlite3").write_bytes(b"fixture")
    assert not retained_path.exists()

    with pytest.raises(RuntimeError, match="synthetic cleanup probe"):
        with _temporary_real_comparison_root() as exceptional_root:
            exceptional_path = exceptional_root
            (exceptional_root / "content-bearing.sqlite3").write_bytes(b"fixture")
            raise RuntimeError("synthetic cleanup probe")
    assert not exceptional_path.exists()


def test_performance_exception_still_runs_process_cleanup(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    baseline = {
        "scanner": [],
        "office": [],
        "discovery": [],
        "python_document_worker": [],
        "legacy_spawn_worker": [],
        "python_process": [20],
        "rust_target_process": [],
    }
    snapshots: list[dict[str, list[int]]] = []

    def snapshot() -> dict[str, list[int]]:
        snapshots.append(dict(baseline))
        return dict(baseline)

    def fail_collection(**_kwargs: object) -> dict[str, object]:
        raise RuntimeError("synthetic performance failure")

    monkeypatch.setattr(
        "scripts.scanner_cutover_gate._scanner_process_snapshot",
        snapshot,
    )
    monkeypatch.setattr(
        "scripts.scanner_cutover_gate._collect_performance_samples",
        fail_collection,
    )

    with pytest.raises(RuntimeError, match="synthetic performance failure"):
        run_performance_gate(
            gate_root=tmp_path,
            gate_config=object(),  # type: ignore[arg-type]
            scan_date=date.today(),
            samples=5,
        )

    assert len(snapshots) >= 2


def test_complete_synthetic_context_parity_is_deterministic(
    tmp_path: Path,
) -> None:
    _require_release_binaries()
    gate_root = tmp_path / "parity"
    gate_root.mkdir()
    gate_config = build_gate_config(config)

    evidence = run_parity_gate(
        gate_root=gate_root,
        gate_config=gate_config,
        scan_date=date.today(),
    )

    assert evidence["passed"] is True, evidence
    assert evidence["inventory_difference_count"] == 0
    assert evidence["text_pdf_hash_difference_count"] == 0
    assert evidence["decision_difference_count"] == 0
    assert evidence["deterministic"] == {
        "python_legacy": True,
        "rust_v2": True,
    }
    assert evidence["cross_engine_final_context_equal"] is False
    expected = json.loads(CONTEXT_GOLDEN.read_text(encoding="utf-8"))
    assert evidence["normalized_context_sha256"] == expected[
        "normalized_context_sha256"
    ]
    assert evidence["intentional_context_difference_golden_matches"] is True
    assert evidence["fallback_count"] == 0


def test_cold_warm_mutation_and_profile_cache_rules(
    tmp_path: Path,
) -> None:
    _require_release_binaries()
    gate_root = tmp_path / "cache"
    gate_root.mkdir()
    gate_config = build_gate_config(config)

    evidence = run_cache_gate(
        gate_root=gate_root,
        gate_config=gate_config,
        scan_date=date.today(),
    )

    assert evidence["passed"] is True, evidence
    for engine in ("python_legacy", "rust_v2"):
        assert evidence["engines"][engine] == {
            "cold_successful_misses": evidence["successful_file_count"],
            "warm_successful_fresh": evidence["successful_file_count"],
            "single_file_successful_misses": 1,
            "single_file_successful_fresh": (
                evidence["successful_file_count"] - 1
            ),
            "single_file_identity_matched": True,
            "profile_change_successful_misses": evidence[
                "successful_file_count"
            ],
        }
