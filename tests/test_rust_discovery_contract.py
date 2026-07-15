"""Rust discovery CLI 与 Python discovery 的输出契约测试。"""

from __future__ import annotations

import logging
import os
from datetime import date, datetime
from pathlib import Path

import pytest

from src.services.rust_cli_contract import (
    RustCliJsonResult,
    resolve_binary_path,
)
from src.services.scan_discovery import (
    FileDiscoveryService,
    RustDiscoveryRunner,
)


def test_rust_discovery_logs_successful_stderr_as_structured_warning(
    tmp_path: Path,
    caplog: pytest.LogCaptureFixture,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    binary_path = tmp_path / "discovery.exe"
    monkeypatch.setattr(
        "src.services.scan_discovery.run_rust_json_cli",
        lambda **_kwargs: RustCliJsonResult(
            payload=[],
            error=None,
            duration_ms=7,
            binary_path=binary_path,
            stderr="warning: cannot stat fixture\n",
        ),
    )
    runner = RustDiscoveryRunner(
        {
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "rust_discovery_bin": str(binary_path),
            "discovery_timeout_seconds": 10,
        }
    )

    with caplog.at_level(logging.WARNING):
        assert runner.discover(tmp_path, SCAN_DATE, SCAN_DATE) == []

    assert "Rust discovery stderr warning" in caplog.text
    assert "warning: cannot stat fixture" in caplog.text
    assert "backend=rust_discovery" in caplog.text
    assert f"binary_path={binary_path}" in caplog.text


RUST_DISCOVERY_BIN = resolve_binary_path(
    "rust/target/release/ai-daily-discovery",
    project_root=Path(__file__).resolve().parents[1],
)


def _require_rust_discovery_binary() -> None:
    if RUST_DISCOVERY_BIN.is_file():
        return
    if os.name == "nt":
        pytest.fail(
            "Windows integration requires the built Rust discovery .exe; "
            "run cargo build --manifest-path rust/Cargo.toml --workspace "
            "--release --locked"
        )
    pytest.skip("Rust discovery release binary has not been built")


SCAN_DATE = date(2026, 5, 25)
SCAN_TIMESTAMP = datetime(2026, 5, 25, 12, 0, 0).timestamp()


def _touch_scan_date(path: Path) -> None:
    os.utime(path, (SCAN_TIMESTAMP, SCAN_TIMESTAMP))


def test_rust_discovery_matches_python_backend_for_fixture(
    tmp_path: Path,
    caplog: pytest.LogCaptureFixture,
) -> None:
    """同一组 fixture 下 Rust 和 Python 应发现同一批文件并保持版本指纹一致。"""
    _require_rust_discovery_binary()
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    included_dir = work_dir / "included"
    included_dir.mkdir()
    excluded_dir = work_dir / "excluded"
    excluded_dir.mkdir()

    keep_md = included_dir / "keep.MD"
    keep_txt = included_dir / "note.txt"
    keep_unicode = included_dir / "中文报告.MD"
    keep_md.write_text("keep", encoding="utf-8")
    _touch_scan_date(keep_md)
    keep_txt.write_text("note", encoding="utf-8")
    _touch_scan_date(keep_txt)
    keep_unicode.write_text("unicode", encoding="utf-8")
    _touch_scan_date(keep_unicode)
    ignored_draft = included_dir / "~$draft.md"
    ignored_tmp = included_dir / "scratch.tmp"
    excluded_file = excluded_dir / "blocked.md"
    ignored_draft.write_text("ignore", encoding="utf-8")
    _touch_scan_date(ignored_draft)
    ignored_tmp.write_text("ignore", encoding="utf-8")
    _touch_scan_date(ignored_tmp)
    excluded_file.write_text("blocked", encoding="utf-8")
    _touch_scan_date(excluded_file)

    target_dir = tmp_path / "targets"
    target_dir.mkdir()
    symlink_target = target_dir / "target.txt"
    symlink_target.write_text("target", encoding="utf-8")
    _touch_scan_date(symlink_target)
    symlink_path = included_dir / "linked.MD"
    symlink_path.symlink_to(symlink_target)

    base_cfg = {
        "allowed_extensions": [".md", ".txt", ".tmp"],
        "ignored_patterns": ["~$*", "*.tmp"],
        "excluded_dirs": [str(excluded_dir)],
    }
    python_discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={**base_cfg, "discovery_backend": "python"},
    )
    rust_discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            **base_cfg,
            "discovery_backend": "rust",
            "rust_discovery_bin": str(RUST_DISCOVERY_BIN),
            "discovery_timeout_seconds": 10,
        },
    )

    python_items = python_discovery.bootstrap_full_scan(SCAN_DATE, SCAN_DATE)
    with caplog.at_level(logging.WARNING, logger="ai_daily_report"):
        rust_items = rust_discovery.bootstrap_full_scan(SCAN_DATE, SCAN_DATE)

    assert "Rust discovery 失败，回退 Python discovery" not in caplog.text

    def comparable(items):
        return sorted(
            (
                item.file_identity,
                item.path.resolve(),
                item.extension,
                item.size_bytes,
                item.source_version,
            )
            for item in items
        )

    assert comparable(rust_items) == comparable(python_items)

    python_by_path = {item.path: item for item in python_items}
    rust_by_path = {item.path: item for item in rust_items}
    assert symlink_path in python_by_path
    assert symlink_path in rust_by_path

    python_symlink_item = python_by_path[symlink_path]
    rust_symlink_item = rust_by_path[symlink_path]
    target_stat = symlink_target.stat()
    expected_file_identity = f"bootstrap:{str(symlink_target.resolve()).lower()}"
    expected_source_version = (
        f"mtime_ns={target_stat.st_mtime_ns}:size={target_stat.st_size}"
    )
    for symlink_item in (python_symlink_item, rust_symlink_item):
        assert symlink_item.path == symlink_path
        assert symlink_item.extension == ".md"
        assert symlink_item.file_identity == expected_file_identity
        assert symlink_item.source_version == expected_source_version
