"""`ai-daily-scanner` 的私有 Python 进程适配器。"""

from __future__ import annotations

import os
from pathlib import Path
import sys
from typing import TYPE_CHECKING, Any, Callable
from uuid import UUID, uuid4

from src.models.scanner_contract import (
    BuildContextRequest,
    ContextEnvelope,
    ContextSummary,
    Diagnostic,
    InspectRunRequest,
    InspectRunResponse,
    build_rust_core_crashed_envelope,
)

from .json_process_client import JsonProcessResult, run_json_process

if TYPE_CHECKING:
    from .context_scheduler import ContextScheduleRequest


DEFAULT_SCANNER_BINARY = "rust/target/release/ai-daily-scanner"
DEFAULT_OFFICE_WORKER_BINARY = "rust/target/release/ai-daily-office-parser"
DEFAULT_SCAN_DB = "data/db/scan_index_v2.sqlite3"


class RustContextClient:
    """组装无敏感信息的 wire request，并执行一次深层 context 调用。"""

    def __init__(
        self,
        *,
        config: Any,
        project_root: Path | None = None,
        scanner_binary: str | Path = DEFAULT_SCANNER_BINARY,
        scan_db_path: str | Path = DEFAULT_SCAN_DB,
        office_worker_path: str | Path = DEFAULT_OFFICE_WORKER_BINARY,
        python_executable: str | Path | None = None,
        python_module_root: str | Path | None = None,
        python_document_worker_module: str = (
            "src.workers.document_parser_worker"
        ),
        timeout_seconds: float = 900,
        request_id_factory: Callable[[], UUID | str] = uuid4,
    ) -> None:
        self._config = config
        self._project_root = (
            project_root or Path(__file__).resolve().parents[2]
        ).resolve()
        self._scanner_binary = self._resolve_executable(scanner_binary)
        self._scan_db_path = self._resolve_path(scan_db_path)
        self._office_worker_path = self._resolve_executable(office_worker_path)
        self._python_executable = self._resolve_path(
            python_executable or sys.executable
        )
        self._python_module_root = self._resolve_path(
            python_module_root or self._project_root
        )
        self._python_document_worker_module = python_document_worker_module
        self._timeout_seconds = float(timeout_seconds)
        if self._timeout_seconds <= 0:
            raise ValueError("timeout_seconds must be positive")
        self._request_id_factory = request_id_factory

    def build_context(
        self,
        request: ContextScheduleRequest,
    ) -> ContextEnvelope:
        request_id = str(self._request_id_factory())
        wire_request = BuildContextRequest(
            contract="ai_daily_context",
            protocol_version=1,
            request_id=request_id,
            work_dir=str(self._resolve_path(self._config.work_dir)),
            start_date=request.start_date.isoformat(),
            end_date=request.end_date.isoformat(),
            report_mode=request.report_mode,
            compression_profile=request.compression_profile,
            scan_db_path=str(self._scan_db_path),
            scanner_profile=self._config.scanner_contract_profile(),
            adapters={
                "office_worker_path": str(self._office_worker_path),
                "python_executable": str(self._python_executable),
                "python_module_root": str(self._python_module_root),
                "python_document_worker_module": (
                    self._python_document_worker_module
                ),
            },
        )
        result = run_json_process(
            command=[str(self._scanner_binary), "build-context"],
            request_payload=wire_request.model_dump(
                mode="json",
                exclude_unset=True,
            ),
            response_model=ContextEnvelope,
            timeout_seconds=self._timeout_seconds,
            expected_request_id=request_id,
            cwd=self._project_root,
        )
        if result.response is not None:
            return result.response
        return self._crashed_envelope(request_id, result)

    def inspect_run(
        self,
        scan_run_id: int,
        *,
        include_content: bool = False,
    ) -> InspectRunResponse:
        """通过稳定 DTO 读取 Rust-owned run；不暴露或查询表结构。"""
        request_id = str(self._request_id_factory())
        request = InspectRunRequest(
            contract="ai_daily_context",
            protocol_version=1,
            request_id=request_id,
            scan_db_path=str(self._scan_db_path),
            scan_run_id=scan_run_id,
            include_content=include_content,
        )
        result = run_json_process(
            command=[str(self._scanner_binary), "inspect-run"],
            request_payload=request.model_dump(mode="json"),
            response_model=InspectRunResponse,
            timeout_seconds=self._timeout_seconds,
            expected_request_id=request_id,
            cwd=self._project_root,
        )
        if result.response is None:
            raise RuntimeError("Rust inspect-run did not return a trusted response")
        return result.response

    def _resolve_path(self, value: str | Path) -> Path:
        path = Path(value)
        if not path.is_absolute():
            path = self._project_root / path
        return path.resolve()

    def _resolve_executable(self, value: str | Path) -> Path:
        path = self._resolve_path(value)
        if os.name == "nt" and path.suffix.lower() != ".exe":
            path = Path(f"{path}.exe")
        return path

    @staticmethod
    def _crashed_envelope(
        request_id: str,
        result: JsonProcessResult[Any],
    ) -> ContextEnvelope:
        if result.transport_error is None:
            return build_rust_core_crashed_envelope(
                request_id=request_id,
                duration_ms=result.duration_ms,
            )
        diagnostic = result.transport_error.error
        return ContextEnvelope(
            contract="ai_daily_context",
            protocol_version=1,
            request_id=request_id,
            engine_version="unknown",
            engine_build="unknown",
            status="error",
            file_context="",
            summary=ContextSummary(
                source_file_count=0,
                success_count=0,
                timeout_count=0,
                included_file_count=0,
                omitted_file_count=0,
                error_file_count=0,
                input_chars=0,
                output_chars=0,
                total_duration_ms=result.duration_ms,
                discovery_duration_ms=0,
                parse_duration_ms=0,
                compression_duration_ms=0,
            ),
            scan_run_id=None,
            context_run_id=None,
            warnings=[],
            error=diagnostic,
        )
