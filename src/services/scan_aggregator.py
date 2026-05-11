"""扫描聚合边界服务。"""

from __future__ import annotations

from pathlib import Path
from typing import Union

from ..models.schemas import FileContext, ScanResult


class ScanAggregator:
    """负责汇总扫描上下文并应用全局字符预算。"""

    def __init__(self, total_max_chars: int):
        self.total_max_chars = total_max_chars
        self.contexts: list[FileContext] = []
        self.success_count = 0
        self.error_count = 0
        self.total_chars = 0
        self.truncated_by_global_limit = False

    def add_context(self, context: FileContext) -> None:
        """追加一个处理结果，并在需要时省略后续内容。"""
        if context.error:
            self.error_count += 1
        else:
            self.success_count += 1

        self.total_chars += len(context.content)
        if self.total_chars > self.total_max_chars and not self.truncated_by_global_limit:
            self.truncated_by_global_limit = True

        if self.truncated_by_global_limit and not context.error:
            context = FileContext(
                file_path=context.file_path,
                file_type=context.file_type,
                content="(已达全局字符上限，内容省略)",
                error=None,
            )

        self.contexts.append(context)

    def add_cached_context(self, context: FileContext) -> None:
        """缓存命中上下文沿用与实时解析相同的聚合规则。"""
        self.add_context(context)

    def add_exception(self, file_path: Union[Path, str], error: Exception) -> None:
        """把未捕获异常转为可审计的文件级错误。"""
        normalized_path = Path(file_path)
        self.error_count += 1
        self.contexts.append(
            FileContext(
                file_path=str(normalized_path),
                file_type=normalized_path.suffix,
                content="",
                error=str(error),
            )
        )

    def build_result(self, total_files: int) -> ScanResult:
        """输出最终扫描结果。"""
        return ScanResult(
            total_files=total_files,
            success_count=self.success_count,
            error_count=self.error_count,
            contexts=self.contexts,
        )
