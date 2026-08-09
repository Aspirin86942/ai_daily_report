"""Python 文档 worker 命令行入口。"""

from __future__ import annotations

import sys

from .python_worker_identity import python_worker_version_json

# 独立 session 契约（spec Part 7.1）：与共享 ai_daily_worker_v1 完全分离。
SESSION_CONTRACT_VERSION = "ai_daily_python_session_v1"
_SESSION_REQUEST_ERROR_REQUEST_ID = "00000000-0000-4000-8000-000000000000"


def main(argv: list[str] | tuple[str, ...] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if args == ["version"]:
        sys.stdout.buffer.write(python_worker_version_json() + b"\n")
        return 0

    if args == ["classifier-version"]:
        from .pdf_classifier import classifier_version_json

        sys.stdout.buffer.write(classifier_version_json())
        return 0

    if args == ["session-version"]:
        return _handle_session_version()

    import json

    from .contracts import invalid_request_response, parse_worker_request

    if args == ["parse"]:
        from src.models.scanner_contract import WorkerParseRequest

        try:
            request_json = sys.stdin.buffer.read().decode(
                "utf-8",
                errors="strict",
            )
            request = WorkerParseRequest.model_validate(json.loads(request_json))
        except (UnicodeError, ValueError):
            _emit_json(invalid_request_response().model_dump(mode="json"))
            return 2
        response = parse_worker_request(request)
        _emit_json(response.model_dump(mode="json"))
        return 0 if response.status == "ok" else 1

    if args == ["classify-pdf"]:
        return _handle_classify_pdf()

    if args == ["session"]:
        return _handle_session()

    _emit_json(invalid_request_response().model_dump(mode="json"))
    return 2


def _handle_session_version() -> int:
    """输出严格 ``PythonSessionVersionResponseV1`` 单帧（spec Part 7.1）。"""
    import json

    from src.models.scanner_contract import PythonSessionVersionResponseV1
    from src.workers.pdf_classifier import CLASSIFIER_BUILD
    from .python_worker_identity import PYTHON_WORKER_BUILD

    payload = PythonSessionVersionResponseV1(
        contract="ai_daily_python_session",
        protocol_version=1,
        session_contract_version=SESSION_CONTRACT_VERSION,
        worker_build=PYTHON_WORKER_BUILD,
        classifier_build=CLASSIFIER_BUILD,
        supported_operations=["classify_pdf_v1", "parse_v1"],
    )
    sys.stdout.buffer.write(
        json.dumps(
            payload.model_dump(mode="json"),
            ensure_ascii=False,
            separators=(",", ":"),
        ).encode("utf-8", errors="strict")
        + b"\n"
    )
    return 0


def _handle_session() -> int:
    """长驻 NDJSON 流式 session 主循环（spec Part 7.2/7.3）。

    首帧严格 ``PythonSessionHelloV1``；此后每行一个请求 envelope，每行一个
    typed response。outer ``status=ok`` 携带完整 typed result（含 typed
    error/unknown），outer ``status=error`` 只表示 transport/session 失败并
    在写出唯一一行错误帧后以非 0 退出。
    """
    import json

    from src.models.scanner_contract import (
        PythonSessionHelloV1,
        PythonSessionRequestV1,
        PythonSessionResponseV1,
    )
    from .python_worker_identity import PYTHON_WORKER_BUILD
    from .pdf_classifier import CLASSIFIER_BUILD

    hello = PythonSessionHelloV1(
        contract="ai_daily_python_session",
        protocol_version=1,
        frame="hello",
        session_contract_version=SESSION_CONTRACT_VERSION,
        worker_build=PYTHON_WORKER_BUILD,
        classifier_build=CLASSIFIER_BUILD,
        supported_operations=["classify_pdf_v1", "parse_v1"],
    )
    _emit_session_frame(hello.model_dump(mode="json"))

    for line in sys.stdin.buffer:
        try:
            text = line.decode("utf-8", errors="strict")
            envelope = PythonSessionRequestV1.model_validate(json.loads(text))
        except (UnicodeError, ValueError):
            _emit_session_frame(_session_request_error().model_dump(mode="json"))
            return 2
        try:
            response = _session_dispatch(envelope)
        except _SessionProtocolError as error:
            _emit_session_frame(
                _session_operation_error(
                    request_id=envelope.request_id,
                    operation=envelope.operation,
                    message=error.message,
                ).model_dump(mode="json")
            )
            return 2
        except Exception as error:  # noqa: BLE001 - 进程 seam 绝不抛裸异常
            _emit_session_frame(
                _session_internal_error(envelope.request_id, envelope.operation, error).model_dump(
                    mode="json"
                )
            )
            return 1
        _emit_session_frame(response.model_dump(mode="json"))
    return 0


def _session_dispatch(envelope: "PythonSessionRequestV1") -> "PythonSessionResponseV1":
    """执行一个 session 请求并返回 typed response；协议错误抛会话级异常。"""
    from src.models.scanner_contract import (
        PdfClassifierRequestV1,
        PdfClassifierResultV1,
        PythonOperationDiagnosticV1,
        PythonSessionResponseV1,
        WorkerParseRequest,
        WorkerParseResponse,
    )
    from .contracts import parse_worker_request

    if envelope.operation == "classify_pdf_v1":
        try:
            request = PdfClassifierRequestV1.model_validate(envelope.payload)
        except ValueError as error:
            raise _SessionProtocolError(f"classify payload is invalid: {error}") from error
        if request.request_id != envelope.request_id:
            raise _SessionProtocolError(
                "classify payload request_id does not match the session envelope"
            )
        result = _session_classify(request)
        return PythonSessionResponseV1(
            contract="ai_daily_python_session",
            protocol_version=1,
            request_id=envelope.request_id,
            operation="classify_pdf_v1",
            status="ok",
            result=result,
            error=None,
        )

    if envelope.operation == "parse_v1":
        try:
            request = WorkerParseRequest.model_validate(envelope.payload)
        except ValueError as error:
            raise _SessionProtocolError(f"parse payload is invalid: {error}") from error
        if request.request_id != envelope.request_id:
            raise _SessionProtocolError(
                "parse payload request_id does not match the session envelope"
            )
        # spec Part 7.2：session 只执行 pdf_text_v1；Office/SharePoint 继续 one-shot。
        if request.backend != "pdf_text_v1":
            raise _SessionProtocolError(
                "session parse_v1 only supports the pdf_text_v1 backend"
            )
        response = parse_worker_request(request)
        if response.request_id != envelope.request_id:
            raise _SessionProtocolError(
                "parse response request_id does not match the session envelope"
            )
        return PythonSessionResponseV1(
            contract="ai_daily_python_session",
            protocol_version=1,
            request_id=envelope.request_id,
            operation="parse_v1",
            status="ok",
            result=response,
            error=None,
        )

    raise _SessionProtocolError(f"unsupported session operation: {envelope.operation}")


def _session_classify(
    request: "PdfClassifierRequestV1",
) -> "PdfClassifierResultV1":
    """执行一次 ``classify_pdf_v1``，保留 source-version 前后校验（spec 7.3）。"""
    from pathlib import Path

    from src.models.scanner_contract import (
        PdfClassifierResultV1,
        PythonOperationDiagnosticV1,
    )
    from .contracts import _observe_source
    from .pdf_classifier import classify_pdf

    path = Path(request.file_path)
    try:
        observed_before, _ = _observe_source(path)
    except OSError:
        return _session_classify_error(
            request,
            error_code="PARSER_START_FAILED",
            message="file metadata is unavailable before classification",
            retryable=True,
        )
    if observed_before != request.source_version:
        return _session_classify_error(
            request,
            error_code="SOURCE_VERSION_CHANGED",
            message="file source version changed before classification",
            retryable=False,
        )

    result = classify_pdf(request.file_path, request.max_pages)

    try:
        observed_after, _ = _observe_source(path)
    except OSError:
        return _session_classify_error(
            request,
            error_code="SOURCE_VERSION_CHANGED",
            message="file source became unavailable during classification",
            retryable=False,
        )
    if observed_after != observed_before:
        return _session_classify_error(
            request,
            error_code="SOURCE_VERSION_CHANGED",
            message="file source version changed during classification",
            retryable=False,
        )

    if result["status"] in ("text_in_parse_window", "no_text_in_parse_window"):
        return PdfClassifierResultV1(
            status=result["status"],
            page_count=result["page_count"],
            result_examined_pages=result["result_examined_pages"],
            diagnostic=None,
        )
    return PdfClassifierResultV1(
        status=result["status"],
        page_count=result["page_count"],
        result_examined_pages=result["result_examined_pages"],
        diagnostic=PythonOperationDiagnosticV1(**result["diagnostic"]),
    )


def _session_classify_error(
    request: "PdfClassifierRequestV1",
    *,
    error_code: str,
    message: str,
    retryable: bool,
) -> "PdfClassifierResultV1":
    from src.models.scanner_contract import (
        PdfClassifierResultV1,
        PythonOperationDiagnosticV1,
    )

    return PdfClassifierResultV1(
        status="error",
        page_count=None,
        result_examined_pages=None,
        diagnostic=PythonOperationDiagnosticV1(
            error_code=error_code,
            message=message[:4096],
            retryable=retryable,
            stage="parse",
            file_path=request.file_path,
            backend=None,
        ),
    )


class _SessionProtocolError(Exception):
    """请求级协议损坏；会话写出唯一错误帧后退出非 0。"""

    def __init__(self, message: str) -> None:
        super().__init__(message)
        self.message = message


def _session_request_error() -> "PythonSessionResponseV1":
    """无法解析出 request_id/operation 时使用固定 sentinel 的 transport error。"""
    from src.models.scanner_contract import (
        PythonOperationDiagnosticV1,
        PythonSessionResponseV1,
    )

    return PythonSessionResponseV1(
        contract="ai_daily_python_session",
        protocol_version=1,
        request_id=_SESSION_REQUEST_ERROR_REQUEST_ID,
        operation="classify_pdf_v1",
        status="error",
        result=None,
        error=PythonOperationDiagnosticV1(
            error_code="INVALID_REQUEST",
            message="stdin line is not a valid session request",
            retryable=False,
            stage="request",
            file_path=None,
            backend=None,
        ),
    )


def _session_operation_error(
    request_id: str,
    operation: str,
    message: str,
) -> "PythonSessionResponseV1":
    from src.models.scanner_contract import (
        PythonOperationDiagnosticV1,
        PythonSessionResponseV1,
    )

    return PythonSessionResponseV1(
        contract="ai_daily_python_session",
        protocol_version=1,
        request_id=request_id,
        operation=operation,
        status="error",
        result=None,
        error=PythonOperationDiagnosticV1(
            error_code="INVALID_REQUEST",
            message=message[:4096],
            retryable=False,
            stage="request",
            file_path=None,
            backend=None,
        ),
    )


def _session_internal_error(
    request_id: str,
    operation: str,
    error: Exception,
) -> "PythonSessionResponseV1":
    from src.models.scanner_contract import (
        PythonOperationDiagnosticV1,
        PythonSessionResponseV1,
    )

    safe_message = str(error).strip()[:4096] or "session operation failed"
    return PythonSessionResponseV1(
        contract="ai_daily_python_session",
        protocol_version=1,
        request_id=request_id,
        operation=operation,
        status="error",
        result=None,
        error=PythonOperationDiagnosticV1(
            error_code="INTERNAL_ERROR",
            message=safe_message,
            retryable=True,
            stage="process",
            file_path=None,
            backend=None,
        ),
    )


def _emit_session_frame(payload: object) -> None:
    """session 每帧必须落盘；进程 seam 禁止依赖缓冲区冲刷时机。"""
    _emit_json(payload)
    sys.stdout.buffer.flush()


def _handle_classify_pdf() -> int:
    """执行一次严格 one-shot ``classify-pdf``。

    ``unknown/error`` 是完整的 typed domain result（外层 status=ok、result
    内带 diagnostic），不是 transport 失败；只有请求不可解析才返回外层 error。
    """
    import json

    from src.models.scanner_contract import (
        PdfClassifierRequestV1,
        PdfClassifierResponseV1,
        PdfClassifierResultV1,
        PythonOperationDiagnosticV1,
    )

    try:
        request_json = sys.stdin.buffer.read().decode("utf-8", errors="strict")
        request = PdfClassifierRequestV1.model_validate(json.loads(request_json))
    except (UnicodeError, ValueError):
        _emit_json(_classifier_transport_error().model_dump(mode="json"))
        return 2

    from .pdf_classifier import classify_pdf

    try:
        result = classify_pdf(request.file_path, request.max_pages)
        if result["status"] in ("text_in_parse_window", "no_text_in_parse_window"):
            typed_result = PdfClassifierResultV1(
                status=result["status"],
                page_count=result["page_count"],
                result_examined_pages=result["result_examined_pages"],
                diagnostic=None,
            )
        else:
            typed_result = PdfClassifierResultV1(
                status=result["status"],
                page_count=result["page_count"],
                result_examined_pages=result["result_examined_pages"],
                diagnostic=PythonOperationDiagnosticV1(**result["diagnostic"]),
            )
        response = PdfClassifierResponseV1(
            contract="ai_daily_pdf_classifier",
            protocol_version=1,
            request_id=request.request_id,
            status="ok",
            result=typed_result,
            error=None,
        )
    except Exception as error:  # noqa: BLE001 - 进程 seam 绝不抛裸异常
        response = _classifier_internal_error(request.request_id, error)
        _emit_json(response.model_dump(mode="json"))
        return 1
    _emit_json(response.model_dump(mode="json"))
    return 0


def _classifier_transport_error() -> "PdfClassifierResponseV1":
    from src.models.scanner_contract import (
        PdfClassifierResponseV1,
        PythonOperationDiagnosticV1,
    )

    return PdfClassifierResponseV1(
        contract="ai_daily_pdf_classifier",
        protocol_version=1,
        request_id="00000000-0000-4000-8000-000000000000",
        status="error",
        result=None,
        error=PythonOperationDiagnosticV1(
            error_code="INVALID_REQUEST",
            message="stdin is not a valid classifier request",
            retryable=False,
            stage="request",
            file_path=None,
            backend=None,
        ),
    )


def _classifier_internal_error(
    request_id: str,
    error: Exception,
) -> "PdfClassifierResponseV1":
    from src.models.scanner_contract import (
        PdfClassifierResponseV1,
        PythonOperationDiagnosticV1,
    )

    safe_message = str(error).strip()[:4096] or "classifier failed"
    return PdfClassifierResponseV1(
        contract="ai_daily_pdf_classifier",
        protocol_version=1,
        request_id=request_id,
        status="error",
        result=None,
        error=PythonOperationDiagnosticV1(
            error_code="INTERNAL_ERROR",
            message=safe_message,
            retryable=True,
            stage="process",
            file_path=None,
            backend=None,
        ),
    )


def _emit_json(payload: object) -> None:
    """绕过环境文本编码，保证进程合同始终输出 UTF-8 字节。"""
    import json

    response = json.dumps(payload, ensure_ascii=False).encode(
        "utf-8",
        errors="strict",
    )
    sys.stdout.buffer.write(response + b"\n")


if __name__ == "__main__":
    exit_code = main()
    if sys.platform == "win32":
        # This worker serves exactly one request. Flush the contract bytes and
        # skip CPython's process-wide finalizer walk; Windows reclaims every
        # remaining process handle after the already-complete request exits.
        sys.stdout.flush()
        sys.stderr.flush()
        import nt

        nt._exit(exit_code)
    raise SystemExit(exit_code)
