"""Scanner 性能指标模型与计时工具。"""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass, field
from time import perf_counter
from typing import Callable, Iterator


STAGE_TO_FIELD = {
    "discovery": "discovery_duration_ms",
    "inventory_cache": "inventory_cache_duration_ms",
    "parse": "parse_duration_ms",
    "aggregation": "aggregation_duration_ms",
}


def is_timeout_error(error: str | None) -> bool:
    """只按稳定 timeout 前缀识别超时，避免普通异常误入 timeout 统计。"""
    return bool(error and error.startswith("timeout:"))


@dataclass(slots=True)
class ExtensionMetrics:
    """单个扩展名的重解析指标。"""

    extension: str
    file_count: int = 0
    parse_duration_ms: int = 0
    success_count: int = 0
    error_count: int = 0
    timeout_count: int = 0

    def record(self, duration_ms: int, error: str | None) -> None:
        """追加一次文件解析结果。"""
        self.file_count += 1
        self.parse_duration_ms += max(0, int(duration_ms))
        if error:
            self.error_count += 1
            if is_timeout_error(error):
                self.timeout_count += 1
        else:
            self.success_count += 1

    def to_dict(self) -> dict[str, int | str]:
        """转成可序列化结构，供 SQLite 和 benchmark 共用。"""
        return {
            "extension": self.extension,
            "file_count": self.file_count,
            "parse_duration_ms": self.parse_duration_ms,
            "success_count": self.success_count,
            "error_count": self.error_count,
            "timeout_count": self.timeout_count,
        }


@dataclass(slots=True)
class ReparseDetail:
    """单个重解析文件的 cache miss 与解析结果明细。"""

    path: str
    extension: str
    file_identity: str
    source_version: str
    cache_status: str
    cache_miss_reason: str
    previous_source_version: str | None = None
    parse_duration_ms: int = 0
    parse_status: str = "success"
    parse_error: str = ""
    parser_backend: str = ""
    truncated: bool = False

    def to_dict(self) -> dict[str, int | str | bool | None]:
        """转成 benchmark JSON / Markdown 共用的稳定结构。"""
        return {
            "path": self.path,
            "extension": self.extension,
            "file_identity": self.file_identity,
            "source_version": self.source_version,
            "cache_status": self.cache_status,
            "cache_miss_reason": self.cache_miss_reason,
            "previous_source_version": self.previous_source_version,
            "parse_duration_ms": max(0, int(self.parse_duration_ms)),
            "parse_status": self.parse_status,
            "parse_error": self.parse_error,
            "parser_backend": self.parser_backend,
            "truncated": bool(self.truncated),
        }


@dataclass(slots=True)
class ScanRunMetrics:
    """单次 scanner 运行的完整指标。"""

    total_duration_ms: int = 0
    discovery_duration_ms: int = 0
    inventory_cache_duration_ms: int = 0
    parse_duration_ms: int = 0
    aggregation_duration_ms: int = 0
    discovered_count: int = 0
    reused_count: int = 0
    reparsed_count: int = 0
    success_count: int = 0
    error_count: int = 0
    timeout_count: int = 0
    extension_metrics: list[ExtensionMetrics] = field(default_factory=list)

    def to_summary_line(self) -> str:
        """生成稳定日志摘要，方便从运行日志中直接判断瓶颈位置。"""
        return (
            "扫描指标: "
            f"总耗时 {self.total_duration_ms}ms, "
            f"discovery {self.discovery_duration_ms}ms, "
            f"inventory/cache {self.inventory_cache_duration_ms}ms, "
            f"parse {self.parse_duration_ms}ms, "
            f"aggregation {self.aggregation_duration_ms}ms, "
            f"发现 {self.discovered_count} 个, "
            f"缓存复用 {self.reused_count} 个, "
            f"重解析 {self.reparsed_count} 个, "
            f"成功 {self.success_count} 个, "
            f"失败 {self.error_count} 个, "
            f"超时 {self.timeout_count} 个"
        )

    def to_dict(self) -> dict[str, int | list[dict[str, int | str]]]:
        """转成 benchmark 友好的 JSON 结构。"""
        return {
            "total_duration_ms": self.total_duration_ms,
            "discovery_duration_ms": self.discovery_duration_ms,
            "inventory_cache_duration_ms": self.inventory_cache_duration_ms,
            "parse_duration_ms": self.parse_duration_ms,
            "aggregation_duration_ms": self.aggregation_duration_ms,
            "discovered_count": self.discovered_count,
            "reused_count": self.reused_count,
            "reparsed_count": self.reparsed_count,
            "success_count": self.success_count,
            "error_count": self.error_count,
            "timeout_count": self.timeout_count,
            "extension_metrics": [
                extension_metric.to_dict()
                for extension_metric in self.extension_metrics
            ],
        }


class ScanMetricsCollector:
    """收集单次扫描的阶段耗时和扩展名指标。"""

    def __init__(self, clock: Callable[[], float] = perf_counter) -> None:
        self._clock = clock
        self._started_at = self._clock()
        self._stage_durations: dict[str, int] = {stage: 0 for stage in STAGE_TO_FIELD}
        self._extension_metrics: dict[str, ExtensionMetrics] = {}
        self._discovered_count = 0
        self._reused_count = 0
        self._reparsed_count = 0
        self._success_count = 0
        self._error_count = 0

    @classmethod
    def start(cls) -> "ScanMetricsCollector":
        """语义化构造入口，便于 scanner 编排层表达指标开始点。"""
        return cls()

    @contextmanager
    def measure_stage(self, stage: str) -> Iterator[None]:
        """记录阶段 wall clock 耗时。"""
        started_at = self._clock()
        try:
            yield
        finally:
            elapsed_ms = int(round((self._clock() - started_at) * 1000))
            self.record_stage_duration(stage, elapsed_ms)

    def record_stage_duration(self, stage: str, duration_ms: int) -> None:
        """直接记录阶段耗时，测试和少数手工口径可使用。"""
        if stage not in STAGE_TO_FIELD:
            raise ValueError(f"Unknown scan metrics stage: {stage}")
        self._stage_durations[stage] += max(0, int(duration_ms))

    def set_discovered_count(self, discovered_count: int) -> None:
        self._discovered_count = max(0, int(discovered_count))

    def set_plan_counts(self, reused_count: int, reparsed_count: int) -> None:
        self._reused_count = max(0, int(reused_count))
        self._reparsed_count = max(0, int(reparsed_count))

    def set_result_counts(self, success_count: int, error_count: int) -> None:
        self._success_count = max(0, int(success_count))
        self._error_count = max(0, int(error_count))

    def record_extension_result(
        self,
        extension: str,
        duration_ms: int,
        error: str | None,
    ) -> None:
        """记录一个实际重解析文件的扩展名指标。"""
        normalized_extension = extension.lower() if extension else "(none)"
        if normalized_extension not in self._extension_metrics:
            self._extension_metrics[normalized_extension] = ExtensionMetrics(
                extension=normalized_extension
            )
        self._extension_metrics[normalized_extension].record(duration_ms, error)

    def finish(self, total_duration_ms: int | None = None) -> ScanRunMetrics:
        """冻结本次扫描指标，返回可持久化对象。"""
        if total_duration_ms is None:
            total_duration_ms = int(round((self._clock() - self._started_at) * 1000))

        extension_metrics = [
            self._extension_metrics[key]
            for key in sorted(self._extension_metrics)
        ]
        timeout_count = sum(item.timeout_count for item in extension_metrics)
        return ScanRunMetrics(
            total_duration_ms=max(0, int(total_duration_ms)),
            discovery_duration_ms=self._stage_durations["discovery"],
            inventory_cache_duration_ms=self._stage_durations["inventory_cache"],
            parse_duration_ms=self._stage_durations["parse"],
            aggregation_duration_ms=self._stage_durations["aggregation"],
            discovered_count=self._discovered_count,
            reused_count=self._reused_count,
            reparsed_count=self._reparsed_count,
            success_count=self._success_count,
            error_count=self._error_count,
            timeout_count=timeout_count,
            extension_metrics=extension_metrics,
        )
