"""Python 文档 worker 的 v1 进程合同常量。"""

from __future__ import annotations

import os

from src.models.scanner_contract import (
    Diagnostic,
    TransportErrorResponse,
    WorkerParseRequest,
    WorkerParseResponse,
    WorkerVersionResponse,
)


WORKER_CONTRACT_VERSION = "ai_daily_worker_v1"
PYTHON_WORKER_VERSION = "0.1.0"
PYTHON_WORKER_BUILD = os.environ.get(
    "AI_DAILY_PYTHON_WORKER_BUILD",
    "dev-python-document-worker",
)


def python_worker_version_response() -> WorkerVersionResponse:
    """返回排序稳定、可被 Rust 预检的 worker 身份。"""
    return WorkerVersionResponse(
        contract="ai_daily_worker",
        protocol_version=1,
        worker_kind="python_document",
        worker_contract_version=WORKER_CONTRACT_VERSION,
        worker_version=PYTHON_WORKER_VERSION,
        worker_build=PYTHON_WORKER_BUILD,
        supported_backends=[
            "pdf_text_v1",
            "python_office_v1",
            "python_sharepoint_text_v1",
        ],
        supported_extensions=[
            ".doc",
            ".docx",
            ".pdf",
            ".ppt",
            ".pptx",
            ".xls",
            ".xlsx",
        ],
    )


def python_not_implemented_response(
    request: WorkerParseRequest,
) -> WorkerParseResponse:
    """Task 4 占位响应保持完整身份和请求回显，便于 Rust 严格校验。"""
    version = python_worker_version_response()
    return WorkerParseResponse(
        contract="ai_daily_worker",
        protocol_version=1,
        request_id=request.request_id,
        status="error",
        file_path=request.file_path,
        file_type=request.file_type,
        content="",
        parser_backend=request.backend,
        worker_lane="python_document_process",
        truncated=False,
        warnings=[],
        error=Diagnostic(
            error_code="NOT_IMPLEMENTED",
            message="Python document worker parse is not implemented in Task 4",
            retryable=False,
            stage="parse",
            file_path=request.file_path,
            backend=request.backend,
        ),
        duration_ms=0,
        worker_contract_version=version.worker_contract_version,
        worker_version=version.worker_version,
        worker_build=version.worker_build,
        observed_source_version=request.expected_source_version,
    )


def invalid_request_response() -> TransportErrorResponse:
    """无可信 request id 时只返回固定、无输入回显的 transport error。"""
    return TransportErrorResponse(
        contract="ai_daily_transport",
        protocol_version=1,
        status="error",
        error=Diagnostic(
            error_code="INVALID_REQUEST",
            message="stdin is not a valid worker request",
            retryable=False,
            stage="request",
            file_path=None,
            backend=None,
        ),
    )
