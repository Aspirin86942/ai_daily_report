"""服务模块。"""

from __future__ import annotations

from .report_gen import ReportGenerator
from .sqlite_store import SQLiteStore

__all__ = [
    "FileDiscoveryService",
    "FileScanner",
    "ParserSupervisor",
    "ReportGenerator",
    "ScanAggregator",
    "ScanIndexStore",
    "ScanPlanner",
    "SQLiteStore",
]


def __getattr__(name: str):
    """延迟导入重模块，避免轻量子模块导入时被 file_scanner 依赖链拖重。"""
    if name == "FileScanner":
        from .file_scanner import FileScanner

        return FileScanner
    if name == "ScanAggregator":
        from .scan_aggregator import ScanAggregator

        return ScanAggregator
    if name == "FileDiscoveryService":
        from .scan_discovery import FileDiscoveryService

        return FileDiscoveryService
    if name == "ScanIndexStore":
        from .scan_index_store import ScanIndexStore

        return ScanIndexStore
    if name == "ScanPlanner":
        from .scan_planner import ScanPlanner

        return ScanPlanner
    if name == "ParserSupervisor":
        from .scan_worker_pool import ParserSupervisor

        return ParserSupervisor
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
