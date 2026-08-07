"""应用级 context 调度边界；每次运行只选择一个完整 engine。

分层：orchestration —— 引擎选择与调度。
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date
from typing import Any

from ..core.config import config
from .context_engine import ContextBuildResult, ContextEngine
from .rust_context_client import RustContextClient


_SUPPORTED_REPORT_MODES = {"daily", "weekly", "monthly"}


@dataclass(frozen=True, slots=True)
class ContextScheduleRequest:
    report_mode: str
    source: str
    start_date: date
    end_date: date
    compression_profile: str | None = None
    user_input: str | None = None


class ContextScheduler:
    """验证应用请求，调用唯一 engine，并缩小为 ContextBuildResult。"""

    def __init__(
        self,
        *,
        engine: ContextEngine | None = None,
        runtime_config: Any | None = None,
    ) -> None:
        self._engine = engine or self._engine_from_config(runtime_config or config)

    def build_context(self, request: ContextScheduleRequest) -> ContextBuildResult:
        self._validate_request(request)
        envelope = self._engine.build_context(request)
        return ContextBuildResult.from_envelope(envelope)

    @staticmethod
    def _validate_request(request: ContextScheduleRequest) -> None:
        source = request.source.strip().lower()
        report_mode = request.report_mode.strip().lower()
        if source != "scan":
            raise ValueError(f"unsupported context source: {request.source!r}")
        if report_mode not in _SUPPORTED_REPORT_MODES:
            raise ValueError(f"unsupported report_mode: {request.report_mode!r}")
        if request.start_date > request.end_date:
            raise ValueError("start_date must be earlier than or equal to end_date")

    @staticmethod
    def _engine_from_config(runtime_config: Any) -> ContextEngine:
        engine_name = runtime_config.scanner_engine
        if engine_name != "rust_v2":
            raise ValueError(f"unsupported scanner engine: {engine_name!r}")
        return RustContextClient(
            config=runtime_config,
            scanner_binary=runtime_config.rust_scanner_bin,
            scan_db_path=runtime_config.rust_index_db_path,
            office_worker_path=runtime_config.rust_office_parser_bin,
            timeout_seconds=runtime_config.rust_process_timeout_seconds,
        )


__all__ = [
    "ContextBuildResult",
    "ContextScheduleRequest",
    "ContextScheduler",
]
