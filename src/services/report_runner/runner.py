"""ReportRunner pipeline 占位；Task 3 将落地完整实现。"""

from __future__ import annotations

from .outcomes import ReportRunOutcome
from .requests import ReportRunRequest


class ReportRunner:
    def run(self, request: ReportRunRequest) -> ReportRunOutcome:
        raise NotImplementedError
