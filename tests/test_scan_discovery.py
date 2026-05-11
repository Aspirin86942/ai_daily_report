"""测试文件发现边界。"""

from datetime import date
from pathlib import Path

from src.services.scan_discovery import DiscoveredFile, FileDiscoveryService


def test_bootstrap_full_scan_filters_extensions_patterns_and_excluded_dirs(
    tmp_path: Path,
):
    """文件发现服务应保留现有扩展名、忽略模式和排除目录行为。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    included_dir = work_dir / "included"
    included_dir.mkdir()
    excluded_dir = work_dir / "excluded"
    excluded_dir.mkdir()

    (included_dir / "keep.MD").write_text("keep", encoding="utf-8")
    (included_dir / "~$draft.md").write_text("ignore", encoding="utf-8")
    (included_dir / "scratch.tmp").write_text("ignore", encoding="utf-8")
    (excluded_dir / "blocked.md").write_text("blocked", encoding="utf-8")

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md", ".tmp"],
            "ignored_patterns": ["~$*", "*.tmp"],
            "excluded_dirs": [str(excluded_dir)],
        },
    )

    files = discovery.bootstrap_full_scan(date.today(), date.today())

    assert [item.path.relative_to(work_dir).as_posix() for item in files] == [
        "included/keep.MD"
    ]


def test_bootstrap_full_scan_skips_files_outside_date_range(tmp_path: Path):
    """文件发现服务应按修改时间范围过滤文件。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    (work_dir / "recent.md").write_text("recent", encoding="utf-8")

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
        },
    )

    files = discovery.bootstrap_full_scan(date(2000, 1, 1), date(2000, 1, 2))

    assert files == []


def test_bootstrap_full_scan_returns_discovered_file_metadata(tmp_path: Path):
    """启动发现应返回可写入库存表的文件元数据。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "report.TXT"
    sample.write_text("hello", encoding="utf-8")

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".txt"],
            "ignored_patterns": [],
            "excluded_dirs": [],
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert isinstance(item, DiscoveredFile)
    assert item.path == sample
    assert item.file_identity.startswith("bootstrap:")
    assert item.file_identity == f"bootstrap:{str(sample.resolve()).lower()}"
    assert item.extension == ".txt"
    assert item.size_bytes == sample.stat().st_size
    assert item.source_version == (
        f"mtime_ns={sample.stat().st_mtime_ns}:size={sample.stat().st_size}"
    )
