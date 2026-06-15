"""Scanner item adapter helpers.

这些 helper 只负责把旧测试/兼容路径中的 ``Path`` 项与落库后的
``InventoryItem`` / ``DiscoveredFile`` 统一成 scanner 运行期需要的字段。
这样 FileScanner 可以继续保留对旧 monkeypatch 的兼容，而不会把路径指纹
和 source_version 规则散落在主扫描流程里。
"""

from __future__ import annotations

from datetime import datetime
from pathlib import Path

from .scan_discovery import DiscoveredFile
from .scan_index_store import InventoryItem

ScannerItem = Path | InventoryItem


def normalize_discovered_files(
    discovered_files: list[Path | DiscoveredFile],
) -> list[DiscoveredFile]:
    """兼容旧 Path monkeypatch，同时统一生成 inventory 所需元数据。"""
    normalized: list[DiscoveredFile] = []
    for item in discovered_files:
        if isinstance(item, DiscoveredFile):
            normalized.append(item)
            continue

        file_path = Path(item)
        stat_result = file_path.stat()
        resolved_path = file_path.resolve()
        normalized.append(
            DiscoveredFile(
                file_identity=f"bootstrap:{str(resolved_path).lower()}",
                path=file_path,
                extension=file_path.suffix.lower(),
                modified_at=datetime.fromtimestamp(stat_result.st_mtime),
                size_bytes=stat_result.st_size,
                source_version=(
                    f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
                ),
            )
        )
    return normalized


def item_path(item: ScannerItem) -> Path:
    """统一读取候选路径。"""
    return item if isinstance(item, Path) else Path(item.path)


def item_identity(item: ScannerItem) -> str:
    """统一读取缓存身份。"""
    if isinstance(item, Path):
        return f"bootstrap:{str(item.resolve()).lower()}"
    return item.file_identity


def item_extension(item: ScannerItem) -> str:
    """统一读取扩展名。"""
    return item.suffix.lower() if isinstance(item, Path) else item.extension


def item_source_version(item: ScannerItem) -> str:
    """统一读取 discovery 版本指纹。"""
    if isinstance(item, Path):
        stat_result = item.stat()
        return f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
    return item.source_version
