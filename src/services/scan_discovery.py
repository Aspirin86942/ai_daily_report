"""文件发现边界服务。"""

from __future__ import annotations

import fnmatch
import json
import os
import subprocess
from dataclasses import dataclass
from datetime import date, datetime
from pathlib import Path
from typing import List

from ..core.logger import setup_logger

logger = setup_logger()


@dataclass(slots=True)
class DiscoveredFile:
    """启动扫描阶段的文件元数据。"""

    file_identity: str
    path: Path
    extension: str
    modified_at: datetime
    size_bytes: int
    source_version: str


class RustDiscoveryError(RuntimeError):
    """Rust discovery backend failed before producing a trusted contract."""


class RustDiscoveryRunner:
    """通过 Rust CLI 执行文件发现，并校验 stdout JSON 契约。"""

    def __init__(self, scanner_cfg: dict):
        self.scanner_cfg = scanner_cfg

    def discover(
        self,
        work_dir: Path,
        start_date: date,
        end_date: date,
    ) -> list[DiscoveredFile]:
        request = {
            "work_dir": str(work_dir),
            "start_date": start_date.isoformat(),
            "end_date": end_date.isoformat(),
            "allowed_extensions": self.scanner_cfg["allowed_extensions"],
            "ignored_patterns": self.scanner_cfg["ignored_patterns"],
            "excluded_dirs": self.scanner_cfg.get("excluded_dirs", []),
        }
        completed = subprocess.run(
            [str(self._resolve_binary_path())],
            input=json.dumps(request, ensure_ascii=False),
            text=True,
            capture_output=True,
            timeout=float(self.scanner_cfg.get("discovery_timeout_seconds", 30)),
            check=False,
        )
        if completed.returncode != 0:
            message = completed.stderr.strip() or f"exit code {completed.returncode}"
            raise RustDiscoveryError(message)
        try:
            raw_items = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            raise RustDiscoveryError(f"invalid JSON stdout: {exc}") from exc
        if not isinstance(raw_items, list):
            raise RustDiscoveryError("stdout JSON must be a list")
        return [self._to_discovered_file(item) for item in raw_items]

    def _resolve_binary_path(self) -> Path:
        configured = Path(
            str(
                self.scanner_cfg.get(
                    "rust_discovery_bin",
                    "rust/discovery/target/release/ai-daily-discovery",
                )
            )
        )
        if configured.is_absolute():
            return configured
        project_root = Path(__file__).resolve().parent.parent.parent
        return project_root / configured

    def _to_discovered_file(self, item: object) -> DiscoveredFile:
        if not isinstance(item, dict):
            raise RustDiscoveryError("discovered file item must be an object")
        try:
            return DiscoveredFile(
                file_identity=str(item["file_identity"]),
                path=Path(str(item["path"])),
                extension=str(item["extension"]).lower(),
                modified_at=datetime.fromisoformat(str(item["modified_at"])),
                size_bytes=int(item["size_bytes"]),
                source_version=str(item["source_version"]),
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise RustDiscoveryError(f"invalid discovered file item: {item}") from exc


class FileDiscoveryService:
    """负责按日期范围发现候选文件。"""

    def __init__(self, work_dir: Path, scanner_cfg: dict):
        self.work_dir = work_dir
        self.scanner_cfg = scanner_cfg

    def bootstrap_full_scan(
        self,
        start_date: date,
        end_date: date,
    ) -> List[DiscoveredFile]:
        """执行一次完整文件发现，并返回可落库存的文件元数据。"""
        backend = str(self.scanner_cfg.get("discovery_backend", "rust")).lower()
        if backend == "rust":
            try:
                return RustDiscoveryRunner(self.scanner_cfg).discover(
                    work_dir=self.work_dir,
                    start_date=start_date,
                    end_date=end_date,
                )
            except (OSError, subprocess.SubprocessError, RustDiscoveryError) as exc:
                logger.warning("Rust discovery 失败，回退 Python discovery: %s", exc)
        return self._bootstrap_full_scan_python(start_date, end_date)

    def _bootstrap_full_scan_python(
        self,
        start_date: date,
        end_date: date,
    ) -> list[DiscoveredFile]:
        """保留现有 Python discovery 作为默认实现和 Rust fallback。"""
        start_dt = datetime.combine(start_date, datetime.min.time())
        end_dt = datetime.combine(end_date, datetime.max.time())

        files: list[DiscoveredFile] = []
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
                    stat_result = file_path.stat()
                    mtime = datetime.fromtimestamp(stat_result.st_mtime)
                    if start_dt <= mtime <= end_dt:
                        resolved_path = file_path.resolve()
                        files.append(
                            DiscoveredFile(
                                file_identity=(
                                    f"bootstrap:{str(resolved_path).lower()}"
                                ),
                                path=file_path,
                                extension=file_path.suffix.lower(),
                                modified_at=mtime,
                                size_bytes=stat_result.st_size,
                                source_version=(
                                    f"mtime_ns={stat_result.st_mtime_ns}:"
                                    f"size={stat_result.st_size}"
                                ),
                            )
                        )
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
