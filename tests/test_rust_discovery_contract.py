"""Rust discovery CLI 与 Python discovery 的输出契约测试。"""

from __future__ import annotations

from datetime import date
from pathlib import Path

import pytest

from src.services.scan_discovery import FileDiscoveryService


RUST_DISCOVERY_BIN = (
    Path(__file__).resolve().parents[1]
    / "rust/discovery/target/release/ai-daily-discovery"
)


@pytest.mark.skipif(
    not RUST_DISCOVERY_BIN.exists(),
    reason="Rust discovery release binary has not been built",
)
def test_rust_discovery_matches_python_backend_for_fixture(tmp_path: Path) -> None:
    """同一组 fixture 下 Rust 和 Python 应发现同一批文件并保持版本指纹一致。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    included_dir = work_dir / "included"
    included_dir.mkdir()
    excluded_dir = work_dir / "excluded"
    excluded_dir.mkdir()

    keep_md = included_dir / "keep.MD"
    keep_txt = included_dir / "note.txt"
    keep_md.write_text("keep", encoding="utf-8")
    keep_txt.write_text("note", encoding="utf-8")
    (included_dir / "~$draft.md").write_text("ignore", encoding="utf-8")
    (included_dir / "scratch.tmp").write_text("ignore", encoding="utf-8")
    (excluded_dir / "blocked.md").write_text("blocked", encoding="utf-8")

    target_dir = tmp_path / "targets"
    target_dir.mkdir()
    symlink_target = target_dir / "target.txt"
    symlink_target.write_text("target", encoding="utf-8")
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

    scan_date = date.today()
    python_items = python_discovery.bootstrap_full_scan(scan_date, scan_date)
    rust_items = rust_discovery.bootstrap_full_scan(scan_date, scan_date)

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

    rust_by_path = {item.path: item for item in rust_items}
    symlink_item = rust_by_path[symlink_path]
    target_stat = symlink_target.stat()
    assert symlink_item.extension == ".md"
    assert symlink_item.path == symlink_path
    assert symlink_item.file_identity == (
        f"bootstrap:{str(symlink_target.resolve()).lower()}"
    )
    assert symlink_item.source_version == (
        f"mtime_ns={target_stat.st_mtime_ns}:size={target_stat.st_size}"
    )
