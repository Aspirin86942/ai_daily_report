"""文件发现边界服务。"""

from __future__ import annotations

import fnmatch
import os
from datetime import date, datetime
from pathlib import Path
from typing import List

from ..core.logger import setup_logger

logger = setup_logger()


class FileDiscoveryService:
    """负责按日期范围发现候选文件。"""

    def __init__(self, work_dir: Path, scanner_cfg: dict):
        self.work_dir = work_dir
        self.scanner_cfg = scanner_cfg

    def bootstrap_full_scan(self, start_date: date, end_date: date) -> List[Path]:
        """执行一次完整文件发现，保持现有扫描规则兼容。"""
        start_dt = datetime.combine(start_date, datetime.min.time())
        end_dt = datetime.combine(end_date, datetime.max.time())

        files: list[Path] = []
        excluded_dirs = self.scanner_cfg.get("excluded_dirs", [])
        excluded_paths = [Path(directory).resolve() for directory in excluded_dirs]

        for root, _, filenames in os.walk(self.work_dir):
            root_path = Path(root).resolve()

            if self._is_excluded_dir(root_path, excluded_paths):
                continue

            for filename in filenames:
                filename_lower = filename.lower()
                if not self._is_allowed_extension(filename_lower):
                    continue
                if self._matches_ignored_pattern(filename_lower):
                    continue

                file_path = Path(root) / filename
                try:
                    mtime = datetime.fromtimestamp(file_path.stat().st_mtime)
                    if start_dt <= mtime <= end_dt:
                        files.append(file_path)
                except Exception as exc:
                    logger.warning("无法读取文件时间 %s: %s", file_path, exc)

        return files

    def _is_excluded_dir(self, root_path: Path, excluded_paths: list[Path]) -> bool:
        """目录排除逻辑独立出来，便于保持发现边界单一职责。"""
        for excluded in excluded_paths:
            try:
                root_path.relative_to(excluded)
                return True
            except ValueError:
                continue
        return False

    def _is_allowed_extension(self, filename_lower: str) -> bool:
        """统一按小写扩展名判断，避免大小写差异漏扫。"""
        return any(
            filename_lower.endswith(str(extension).lower())
            for extension in self.scanner_cfg["allowed_extensions"]
        )

    def _matches_ignored_pattern(self, filename_lower: str) -> bool:
        """忽略规则保持 glob 语义，与原扫描器兼容。"""
        return any(
            fnmatch.fnmatch(filename_lower, str(pattern).lower())
            for pattern in self.scanner_cfg["ignored_patterns"]
        )
