"""测试显式 shadow comparison 的隔离与脱敏合同。"""

from __future__ import annotations

from datetime import date
import json
from pathlib import Path

import pytest

from scripts.compare_context_engines import (
    build_comparison_payload,
    compare_context_engines,
    prepare_ephemeral_db_paths,
)


def _file(relative_path: str, digest: str) -> dict[str, object]:
    return {
        "relative_path": relative_path,
        "extension": Path(relative_path).suffix.lower(),
        "parse_status": "success",
        "parser_backend": "light_text_v1",
        "worker_lane": "rust_core",
        "cache_status": "miss",
        "cache_miss_reason": "new_file",
        "truncated": False,
        "content_sha256": digest,
        "parse_duration_ms": 2,
        "failure_class": "",
        "fallback_backend": "",
        "fallback_reason_code": "",
    }


def _decision(relative_path: str, *, action: str = "keep") -> dict[str, object]:
    return {
        "relative_path": relative_path,
        "action": action,
        "reason": "small_file_keep",
        "priority": 30,
        "input_chars": 10,
        "output_chars": 10,
        "truncated": False,
        "error_code": "",
    }


def _observation(
    engine: str,
    *,
    files: list[dict[str, object]] | None = None,
    decisions: list[dict[str, object]] | None = None,
    fallback_count: int = 0,
) -> dict[str, object]:
    return {
        "engine": engine,
        "engine_version": f"{engine}-v1",
        "engine_build": "synthetic-build",
        "status": "ok",
        "scan_run_id": 1,
        "context_run_id": 2,
        "summary": {
            "source_file_count": len(files or []),
            "success_count": len(files or []),
            "timeout_count": 0,
            "included_file_count": len(files or []),
            "omitted_file_count": 0,
            "error_file_count": 0,
            "input_chars": 10,
            "output_chars": 10,
            "total_duration_ms": 5,
            "discovery_duration_ms": 1,
            "parse_duration_ms": 2,
            "compression_duration_ms": 1,
        },
        "context_sha256": "c" * 64,
        "files": files or [],
        "decisions": decisions or [],
        "warning_codes": [],
        "error_code": None,
        "fallback_count": fallback_count,
    }


def test_ephemeral_db_paths_are_distinct_children_of_existing_root(
    tmp_path: Path,
) -> None:
    legacy_db, rust_db = prepare_ephemeral_db_paths(tmp_path)

    assert legacy_db != rust_db
    assert legacy_db.is_relative_to(tmp_path.resolve())
    assert rust_db.is_relative_to(tmp_path.resolve())
    assert legacy_db.parent.is_dir()
    assert rust_db.parent.is_dir()


def test_ephemeral_db_root_must_already_exist(tmp_path: Path) -> None:
    missing = tmp_path / "caller-did-not-create-this"

    with pytest.raises(ValueError, match="existing directory"):
        prepare_ephemeral_db_paths(missing)

    assert not missing.exists()


def test_comparison_payload_contains_only_hashes_and_difference_metadata() -> None:
    legacy = _observation(
        "python_legacy",
        files=[_file("notes/\u4e2d\u6587.txt", "a" * 64), _file("legacy.md", "b" * 64)],
        decisions=[_decision("notes/\u4e2d\u6587.txt")],
        fallback_count=1,
    )
    rust = _observation(
        "rust_v2",
        files=[_file("notes/\u4e2d\u6587.txt", "d" * 64), _file("rust.md", "e" * 64)],
        decisions=[_decision("notes/\u4e2d\u6587.txt", action="compress")],
    )

    payload = build_comparison_payload(
        legacy=legacy,
        rust=rust,
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
        report_mode="daily",
        redact_content=True,
    )
    serialized = json.dumps(payload, ensure_ascii=False)

    assert payload["inventory_difference_count"] == 2
    assert payload["content_hash_difference_count"] == 1
    assert payload["decision_difference_count"] == 1
    assert payload["fallback_count"] == 1
    assert payload["parameters"]["content_policy"] == "hashes_only"
    assert "file_context" not in serialized
    assert '"content"' not in serialized
    assert "cell_values" not in serialized
    assert "cache_contents" not in serialized


def test_compare_runs_each_engine_once_and_leaves_cleanup_to_caller(
    tmp_path: Path,
) -> None:
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    ephemeral_root = tmp_path / "shadow"
    ephemeral_root.mkdir()
    output = ephemeral_root / "comparison.json"
    calls: list[tuple[str, Path]] = []

    def legacy_runner(request, actual_work_dir: Path, db_path: Path):
        calls.append(("python_legacy", db_path))
        assert actual_work_dir == work_dir.resolve()
        assert request.report_mode == "daily"
        Path(f"{db_path}-wal").write_bytes(b"synthetic")
        return _observation("python_legacy")

    def rust_runner(request, actual_work_dir: Path, db_path: Path):
        calls.append(("rust_v2", db_path))
        assert actual_work_dir == work_dir.resolve()
        assert request.source == "scan"
        Path(f"{db_path}-shm").write_bytes(b"synthetic")
        return _observation("rust_v2")

    result = compare_context_engines(
        work_dir=work_dir,
        start_date=date(2026, 7, 15),
        end_date=date(2026, 7, 16),
        report_mode="daily",
        redact_content=True,
        ephemeral_db_root=ephemeral_root,
        output=output,
        legacy_runner=legacy_runner,
        rust_runner=rust_runner,
    )

    assert [name for name, _ in calls] == ["python_legacy", "rust_v2"]
    assert calls[0][1] != calls[1][1]
    assert all(path.is_relative_to(ephemeral_root.resolve()) for _, path in calls)
    assert output.is_file()
    assert result["inventory_difference_count"] == 0
    assert Path(f"{calls[0][1]}-wal").is_file()
    assert Path(f"{calls[1][1]}-shm").is_file()


def test_compare_rejects_output_outside_ephemeral_root_before_running(
    tmp_path: Path,
) -> None:
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    ephemeral_root = tmp_path / "shadow"
    ephemeral_root.mkdir()
    calls: list[str] = []

    def runner(*args, **kwargs):
        calls.append("called")
        return _observation("unused")

    with pytest.raises(ValueError, match="output must stay under"):
        compare_context_engines(
            work_dir=work_dir,
            start_date=date(2026, 7, 15),
            end_date=date(2026, 7, 16),
            report_mode="daily",
            redact_content=True,
            ephemeral_db_root=ephemeral_root,
            output=tmp_path / "outside.json",
            legacy_runner=runner,
            rust_runner=runner,
        )

    assert calls == []
