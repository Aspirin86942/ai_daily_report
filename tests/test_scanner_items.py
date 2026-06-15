from datetime import datetime
from pathlib import Path

from src.services.scan_discovery import DiscoveredFile
from src.services.scan_index_store import InventoryItem
from src.services.scanner_items import (
    item_extension,
    item_identity,
    item_path,
    item_source_version,
    normalize_discovered_files,
)


def test_normalize_discovered_files_preserves_discovered_file_instances():
    discovered = DiscoveredFile(
        file_identity="bootstrap:/work/report.md",
        path=Path("/work/report.md"),
        extension=".md",
        modified_at=datetime(2026, 6, 11, 8, 30),
        size_bytes=10,
        source_version="mtime_ns=1:size=10",
    )

    assert normalize_discovered_files([discovered]) == [discovered]


def test_normalize_discovered_files_converts_legacy_path_items(tmp_path):
    report_path = tmp_path / "Report.MD"
    report_path.write_text("hello", encoding="utf-8")

    [normalized] = normalize_discovered_files([report_path])
    stat_result = report_path.stat()

    assert normalized.file_identity == f"bootstrap:{str(report_path.resolve()).lower()}"
    assert normalized.path == report_path
    assert normalized.extension == ".md"
    assert normalized.modified_at == datetime.fromtimestamp(stat_result.st_mtime)
    assert normalized.size_bytes == stat_result.st_size
    assert normalized.source_version == (
        f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
    )


def test_item_helpers_support_legacy_path_and_inventory_item(tmp_path):
    report_path = tmp_path / "report.md"
    report_path.write_text("hello", encoding="utf-8")
    stat_result = report_path.stat()
    inventory_item = InventoryItem(
        file_identity="bootstrap:/work/report.md",
        path=Path("/work/report.md"),
        extension=".md",
        modified_date=datetime(2026, 6, 11).date(),
        size_bytes=10,
        source_version="mtime_ns=1:size=10",
    )

    assert item_path(report_path) == report_path
    assert item_identity(report_path) == f"bootstrap:{str(report_path.resolve()).lower()}"
    assert item_extension(report_path) == ".md"
    assert item_source_version(report_path) == (
        f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
    )

    assert item_path(inventory_item) == Path("/work/report.md")
    assert item_identity(inventory_item) == "bootstrap:/work/report.md"
    assert item_extension(inventory_item) == ".md"
    assert item_source_version(inventory_item) == "mtime_ns=1:size=10"
