"""私有 context engine 合同和最小应用结果。"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol

from ..models.scanner_contract import (
    ContextEnvelope,
    ContextStatus,
    ContextSummary,
    Diagnostic,
)


@dataclass(frozen=True, slots=True)
class ContextBuildResult:
    """应用层唯一需要的 scanner/context 结果。"""

    file_context: str
    status: ContextStatus
    summary: ContextSummary
    scan_run_id: int | None
    context_run_id: int | None
    warnings: list[Diagnostic]
    error: Diagnostic | None

    @classmethod
    def from_envelope(cls, envelope: ContextEnvelope) -> "ContextBuildResult":
        """丢弃 engine 私有字段，只保留应用策略所需 DTO。"""
        return cls(
            file_context=envelope.file_context,
            status=envelope.status,
            summary=envelope.summary,
            scan_run_id=envelope.scan_run_id,
            context_run_id=envelope.context_run_id,
            warnings=list(envelope.warnings),
            error=envelope.error,
        )


class ContextEngine(Protocol):
    """仅供 ContextScheduler 注入的完整 engine adapter。"""

    def build_context(self, request: object) -> ContextEnvelope:
        ...


__all__ = ["ContextBuildResult", "ContextEngine"]
