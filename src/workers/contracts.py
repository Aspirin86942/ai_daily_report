"""Python 文档 worker 的 v1 进程合同与 worker-owned parser adapter。"""

from __future__ import annotations

import importlib
import os
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path
from time import perf_counter
from typing import Any

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
PYTHON_OFFICE_BACKEND = "python_office_v1"
PYTHON_SHAREPOINT_TEXT_BACKEND = "python_sharepoint_text_v1"
PDF_TEXT_BACKEND = "pdf_text_v1"


@dataclass(frozen=True, slots=True)
class WorkerParsePayload:
    """Worker 内部解析结果；不把 application FileContext 作为进程合同。"""

    file_path: str
    file_type: str
    content: str
    error: str | None
    parser_backend: str
    truncated: bool
    error_code: str | None = None
    retryable: bool = False


DocumentParser = Callable[..., Any]
ExcelReader = Callable[[Path, int, int, int], tuple[str, bool]]


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
            PDF_TEXT_BACKEND,
            PYTHON_OFFICE_BACKEND,
            PYTHON_SHAREPOINT_TEXT_BACKEND,
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


def parse_worker_request(
    request: WorkerParseRequest,
    *,
    document_parser: DocumentParser | None = None,
) -> WorkerParseResponse:
    """执行一次严格 worker parse，并在前后验证同一 source version。"""
    started_at = perf_counter()
    version = python_worker_version_response()
    file_path = Path(request.file_path)
    try:
        observed_before, size_bytes = _observe_source(file_path)
    except OSError:
        return _error_response(
            request,
            version,
            error_code="PARSER_FAILED",
            message="file metadata is unavailable",
            retryable=False,
            observed_source_version=request.expected_source_version,
            started_at=started_at,
        )

    if size_bytes > request.max_file_size_bytes:
        return _error_response(
            request,
            version,
            error_code="FILE_TOO_LARGE",
            message="file exceeds the configured size limit",
            retryable=False,
            observed_source_version=observed_before,
            started_at=started_at,
        )
    if observed_before != request.expected_source_version:
        return _error_response(
            request,
            version,
            error_code="SOURCE_VERSION_CHANGED",
            message="file source version changed before parsing",
            retryable=False,
            observed_source_version=observed_before,
            started_at=started_at,
        )

    try:
        payload = _dispatch_worker_parse(
            request,
            document_parser=document_parser,
        )
    except Exception:
        payload = WorkerParsePayload(
            file_path=request.file_path,
            file_type=request.file_type,
            content="",
            error="document parser failed",
            parser_backend=request.backend,
            truncated=False,
            error_code="PARSER_FAILED",
            retryable=False,
        )

    try:
        observed_after, _ = _observe_source(file_path)
    except OSError:
        observed_after = observed_before
        return _error_response(
            request,
            version,
            error_code="SOURCE_VERSION_CHANGED",
            message="file source version became unavailable during parsing",
            retryable=False,
            observed_source_version=observed_after,
            started_at=started_at,
        )
    if observed_after != observed_before:
        return _error_response(
            request,
            version,
            error_code="SOURCE_VERSION_CHANGED",
            message="file source version changed during parsing",
            retryable=False,
            observed_source_version=observed_after,
            started_at=started_at,
        )
    if (
        payload.file_path != request.file_path
        or payload.file_type != request.file_type
        or payload.parser_backend != request.backend
    ):
        return _error_response(
            request,
            version,
            error_code="PARSER_INVALID_PAYLOAD",
            message="worker adapter returned a mismatched path, type, or backend",
            retryable=False,
            observed_source_version=observed_after,
            started_at=started_at,
        )
    if payload.error is not None:
        return _error_response(
            request,
            version,
            error_code=payload.error_code or "PARSER_FAILED",
            message="document parser reported an error",
            retryable=payload.retryable,
            observed_source_version=observed_after,
            started_at=started_at,
        )

    return WorkerParseResponse(
        contract="ai_daily_worker",
        protocol_version=1,
        request_id=request.request_id,
        status="ok",
        file_path=request.file_path,
        file_type=request.file_type,
        content=payload.content,
        parser_backend=request.backend,
        worker_lane="python_document_process",
        truncated=payload.truncated,
        warnings=[],
        error=None,
        duration_ms=_elapsed_ms(started_at),
        worker_contract_version=version.worker_contract_version,
        worker_version=version.worker_version,
        worker_build=version.worker_build,
        observed_source_version=observed_after,
    )


def parse_legacy_excel_payload(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, object],
    *,
    excel_reader: ExcelReader | None = None,
) -> WorkerParsePayload:
    """保留 `.xls` 的 pandas/xlrd 表格 preview 能力。"""
    max_sheets = _positive_int(limits.get("excel_max_sheets"), 5)
    max_rows = _positive_int(limits.get("excel_max_rows"), 50)
    max_columns = _positive_int(limits.get("excel_max_columns"), 20)
    max_chars = _positive_int(
        limits.get("document_excerpt_max_chars", limits.get("text_max_chars")),
        6000,
    )
    reader = excel_reader or parse_excel_table_content
    try:
        raw_content, budget_truncated = reader(
            file_path,
            max_sheets,
            max_rows,
            max_columns,
        )
        content, char_truncated = _truncate_text(raw_content, max_chars)
        return WorkerParsePayload(
            file_path=str(file_path),
            file_type=file_type,
            content=content,
            error=None,
            parser_backend=PYTHON_OFFICE_BACKEND,
            truncated=budget_truncated or char_truncated,
        )
    except Exception:
        return WorkerParsePayload(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error="PYTHON_OFFICE_XLS_FAILED",
            parser_backend=PYTHON_OFFICE_BACKEND,
            truncated=False,
            error_code="PARSER_FAILED",
            retryable=False,
        )


def parse_sharepoint_text_payload(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, object],
    *,
    import_module: Callable[[str], Any] = importlib.import_module,
) -> WorkerParsePayload:
    """保留 `.doc/.ppt` 的 sharepoint2text 抽取能力。"""
    try:
        sharepoint2text = import_module("sharepoint2text")
    except ModuleNotFoundError:
        return WorkerParsePayload(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error="PYTHON_SHAREPOINT_TEXT_UNAVAILABLE: sharepoint2text",
            parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
            truncated=False,
            error_code="PARSER_FAILED",
            retryable=False,
        )

    try:
        result = next(sharepoint2text.read_file(str(file_path)))
        raw_text = result.get_full_text()
    except Exception:
        return WorkerParsePayload(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error="PYTHON_SHAREPOINT_TEXT_FAILED",
            parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
            truncated=False,
            error_code="PARSER_FAILED",
            retryable=False,
        )

    max_chars = _positive_int(
        limits.get("excerpt_max_chars", limits.get("document_excerpt_max_chars")),
        6000,
    )
    content, truncated = _truncate_text(raw_text or "No Office text extracted", max_chars)
    return WorkerParsePayload(
        file_path=str(file_path),
        file_type=file_type,
        content=content,
        error=None,
        parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
        truncated=truncated,
    )


def _dispatch_worker_parse(
    request: WorkerParseRequest,
    *,
    document_parser: DocumentParser | None,
) -> WorkerParsePayload:
    limits = request.parser_limits.model_dump(mode="python")
    limits.pop("kind", None)
    file_path = Path(request.file_path)
    if request.backend == PYTHON_SHAREPOINT_TEXT_BACKEND:
        return parse_sharepoint_text_payload(
            file_path,
            request.file_type,
            limits,
        )
    if request.backend == PYTHON_OFFICE_BACKEND and request.file_type == ".xls":
        return parse_legacy_excel_payload(
            file_path,
            request.file_type,
            limits,
        )
    if request.backend not in {PYTHON_OFFICE_BACKEND, PDF_TEXT_BACKEND}:
        return WorkerParsePayload(
            file_path=request.file_path,
            file_type=request.file_type,
            content="",
            error="backend is not supported by the Python document worker",
            parser_backend=request.backend,
            truncated=False,
            error_code="PARSER_INVALID_PAYLOAD",
            retryable=False,
        )

    from src.services.document_parser import (
        DocumentParserOptions,
        parse_document_file,
    )

    parser = document_parser or parse_document_file
    if request.backend == PDF_TEXT_BACKEND:
        limits = {
            "pdf_max_pages": limits["max_pages"],
            "document_excerpt_max_chars": limits["excerpt_max_chars"],
        }
        options = DocumentParserOptions(pdf_parser_backend=PDF_TEXT_BACKEND)
    else:
        options = DocumentParserOptions(
            office_parser_backend=PYTHON_OFFICE_BACKEND,
            include_pptx_notes=bool(limits.pop("pptx_include_notes")),
        )
    context = parser(
        file_path=file_path,
        file_type=request.file_type,
        limits=limits,
        options=options,
    )
    return WorkerParsePayload(
        file_path=str(context.file_path),
        file_type=str(context.file_type),
        content=str(context.content),
        error="document parser reported an error" if context.error else None,
        parser_backend=str(context.parser_backend),
        truncated=bool(context.truncated),
        error_code="PARSER_FAILED" if context.error else None,
        retryable=False,
    )


def _error_response(
    request: WorkerParseRequest,
    version: WorkerVersionResponse,
    *,
    error_code: str,
    message: str,
    retryable: bool,
    observed_source_version: str,
    started_at: float,
) -> WorkerParseResponse:
    safe_message = str(message).strip()[:4096] or "worker parse failed"
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
            error_code=error_code,
            message=safe_message,
            retryable=retryable,
            stage="parse",
            file_path=request.file_path,
            backend=request.backend,
        ),
        duration_ms=_elapsed_ms(started_at),
        worker_contract_version=version.worker_contract_version,
        worker_version=version.worker_version,
        worker_build=version.worker_build,
        observed_source_version=observed_source_version,
    )


def _observe_source(file_path: Path) -> tuple[str, int]:
    stat = file_path.stat()
    return f"mtime_ns={stat.st_mtime_ns}:size={stat.st_size}", stat.st_size


def parse_excel_table_content(
    file_path: Path,
    max_sheets: int,
    max_rows: int,
    max_columns: int,
) -> tuple[str, bool]:
    """Build a bounded legacy-XLS preview and report any budget truncation."""
    import pandas as pd

    content_parts: list[str] = []
    truncated = False
    with pd.ExcelFile(file_path) as excel_file:
        sheet_names = list(excel_file.sheet_names)
        if len(sheet_names) > max_sheets:
            truncated = True
        for sheet_index, sheet_name in enumerate(sheet_names[:max_sheets]):
            sheet = excel_file.book.sheet_by_index(sheet_index)
            read_columns = min(int(sheet.ncols), max_columns)
            if int(sheet.nrows) > max_rows + 1 or int(sheet.ncols) > max_columns:
                truncated = True
            content_parts.append(f"## {sheet_name}")
            if read_columns == 0:
                continue
            dataframe = pd.read_excel(
                excel_file,
                sheet_name=sheet_name,
                nrows=max_rows,
                usecols=range(read_columns),
            ).dropna(how="all")
            if not dataframe.empty:
                content_parts.append(_dataframe_to_markdown(dataframe, pd))
    return "\n\n".join(content_parts), truncated


def _dataframe_to_markdown(dataframe: Any, pandas_module: Any) -> str:
    """Render the bounded XLS preview without pandas' optional tabulate extra."""

    def cell_text(value: object) -> str:
        try:
            missing = bool(pandas_module.isna(value))
        except (TypeError, ValueError):
            missing = False
        if missing:
            return ""
        return (
            str(value)
            .replace("\\", "\\\\")
            .replace("|", "\\|")
            .replace("\r\n", "<br>")
            .replace("\r", "<br>")
            .replace("\n", "<br>")
        )

    headers = [cell_text(column) for column in dataframe.columns]
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join("---" for _ in headers) + " |",
    ]
    for row in dataframe.itertuples(index=False, name=None):
        lines.append("| " + " | ".join(cell_text(value) for value in row) + " |")
    return "\n".join(lines)


def _positive_int(value: object, default: int) -> int:
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return default
    return parsed if parsed > 0 else default


def _truncate_text(text: str, max_chars: int) -> tuple[str, bool]:
    if len(text) <= max_chars:
        return text, False
    return text[:max_chars], True


def _elapsed_ms(started_at: float) -> int:
    return max(0, int((perf_counter() - started_at) * 1000))


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
