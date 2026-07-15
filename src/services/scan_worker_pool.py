"""扫描解析 supervisor 占位接口。"""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from typing import Any

from ..core.logger import setup_logger
from ..models.schemas import FileContext
from .scan_timeouts import (
    DEFAULT_FILE_TIMEOUT_SECONDS,
    normalize_file_timeout,
)

logger = setup_logger()


class ParserSupervisor:
    """统一承载解析入口和 timeout 语义的最小 supervisor。"""

    def __init__(
        self,
        file_timeout_seconds: float,
        file_timeout_by_extension: dict[str, float],
    ) -> None:
        self.file_timeout_seconds = file_timeout_seconds
        self.file_timeout_by_extension = file_timeout_by_extension

    def resolve_timeout(self, file_type: str) -> float:
        """按扩展名解析单文件超时预算。"""
        normalized_type = file_type.lower()
        timeout_value = self.file_timeout_by_extension.get(
            normalized_type,
            self.file_timeout_seconds,
        )
        timeout, is_valid = normalize_file_timeout(timeout_value)
        if not is_valid:
            logger.warning(
                "非法单文件超时配置 %r for %s，回退默认值 %ss",
                timeout_value,
                normalized_type,
                f"{DEFAULT_FILE_TIMEOUT_SECONDS:g}",
            )
        return timeout

    def parse_file(
        self,
        file_path: Path,
        file_type: str,
        limits: dict[str, Any],
        direct_parse: Callable[[Path, dict[str, Any]], str | FileContext],
    ) -> FileContext:
        """直接调用解析函数，并把结果规范成 FileContext。"""
        parsed_result = direct_parse(file_path, limits)
        if isinstance(parsed_result, FileContext):
            return parsed_result

        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content=str(parsed_result),
            error=None,
        )

    def handle_worker_timeout(self, file_path: Path, file_type: str) -> FileContext:
        """生成稳定的 timeout fallback，供上层继续聚合与审计。"""
        timeout_label = f"{self.resolve_timeout(file_type):g}"
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error=f"timeout: file parse exceeded {timeout_label}s",
        )

    def handle_missing_result(self, file_path: Path, file_type: str) -> FileContext:
        """生成子进程未返回结果时的稳定 fallback。"""
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error="subprocess exited without result",
        )

    def handle_invalid_payload(self, file_path: Path, file_type: str) -> FileContext:
        """生成子进程返回无效 payload 时的稳定 fallback。"""
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error="subprocess returned invalid payload",
        )
