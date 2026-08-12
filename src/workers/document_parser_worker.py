"""Python 文档 worker 命令行入口。"""

from __future__ import annotations

import sys

from .python_worker_identity import python_worker_hello_payload

_SESSION_REQUEST_ERROR_REQUEST_ID = "00000000-0000-4000-8000-000000000000"


def main(argv: list[str] | tuple[str, ...] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if args == ["hello"]:
        _emit_json(python_worker_hello_payload())
        return 0

    from .contracts import invalid_request_response

    if args == ["session"]:
        return _handle_session()

    _emit_json(invalid_request_response().model_dump(mode="json"))
    return 2


def _handle_session() -> int:
    """Run the worker-v2 NDJSON loop: hello, then one response per request."""
    import json

    _emit_session_frame(python_worker_hello_payload())

    for line in sys.stdin.buffer:
        try:
            text = line.decode("utf-8", errors="strict")
            envelope = _validate_worker_request(json.loads(text))
        except (UnicodeError, ValueError):
            _emit_session_frame(_session_request_error())
            return 2
        try:
            response = _session_dispatch(envelope)
        except _SessionOperationError as error:
            _emit_session_frame(
                _session_rejected_error(
                    request_id=envelope["request_id"],
                    operation=envelope["operation"],
                    error=error,
                )
            )
            return 1
        except _SessionProtocolError as error:
            _emit_session_frame(
                _session_operation_error(
                    request_id=envelope["request_id"],
                    operation=envelope["operation"],
                    message=error.message,
                )
            )
            return 2
        except Exception as error:  # noqa: BLE001 - 进程 seam 绝不抛裸异常
            _emit_session_frame(
                _session_internal_error(envelope["request_id"], envelope["operation"], error)
            )
            return 1
        _emit_session_frame(response)
    return 0


def _validate_worker_request(payload: object) -> dict[str, object]:
    if not isinstance(payload, dict):
        raise ValueError("worker request must be an object")
    required = {"contract", "protocol_version", "request_id", "operation", "payload"}
    if set(payload) != required:
        raise ValueError("worker request fields mismatch")
    if payload["contract"] != "ai_daily_worker" or payload["protocol_version"] != 2:
        raise ValueError("worker request version mismatch")
    if payload["operation"] not in {
        "pdf_classify",
        "pdf_parse",
        "python_office_parse",
        "python_sharepoint_parse",
    }:
        raise ValueError("unsupported operation")
    return payload


def _session_dispatch(envelope: dict[str, object]) -> dict[str, object]:
    """执行一个 session 请求并返回 typed response；协议错误抛会话级异常。"""
    from src.models.scanner_contract import (
        PdfClassifierRequestV1,
        WorkerParseRequest,
    )
    from .contracts import parse_worker_request

    operation = str(envelope["operation"])
    request_id = str(envelope["request_id"])
    payload = envelope["payload"]
    if operation == "pdf_classify":
        try:
            request = PdfClassifierRequestV1.model_validate(payload)
        except ValueError as error:
            raise _SessionProtocolError(f"classify payload is invalid: {error}") from error
        if request.request_id != request_id:
            raise _SessionProtocolError(
                "classify payload request_id does not match the session envelope"
            )
        result = _session_classify(request)
        return _worker_ok(request_id, operation, result.model_dump(mode="json"))

    if operation in {"pdf_parse", "python_office_parse", "python_sharepoint_parse"}:
        try:
            request = WorkerParseRequest.model_validate(payload)
        except ValueError as error:
            raise _SessionProtocolError(f"parse payload is invalid: {error}") from error
        if request.request_id != request_id:
            raise _SessionProtocolError(
                "parse payload request_id does not match the session envelope"
            )
        expected_backend = {
            "pdf_parse": "python_pdf_text_v2",
            "python_office_parse": "python_office_v2",
            "python_sharepoint_parse": "python_sharepoint_text_v2",
        }[operation]
        if request.backend != expected_backend:
            raise _SessionProtocolError(
                "worker operation does not match the requested backend"
            )
        response = parse_worker_request(request)
        if response.request_id != request_id:
            raise _SessionProtocolError(
                "parse response request_id does not match the session envelope"
            )
        return _worker_ok(request_id, operation, response.model_dump(mode="json"))

    raise _SessionProtocolError(f"unsupported session operation: {operation}")


def _worker_ok(request_id: str, operation: str, result: object) -> dict[str, object]:
    return {
        "contract": "ai_daily_worker",
        "protocol_version": 2,
        "request_id": request_id,
        "operation": operation,
        "status": "ok",
        "result": result,
        "error": None,
    }


def _session_classify(
    request: "PdfClassifierRequestV1",
) -> "PdfClassifierResultV1":
    """Classify one PDF while preserving source-version checks."""
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
            retryable=True,
        )

    result = classify_pdf(request.file_path, request.max_pages)

    try:
        observed_after, _ = _observe_source(path)
    except OSError:
        return _session_classify_error(
            request,
            error_code="SOURCE_VERSION_CHANGED",
            message="file source became unavailable during classification",
            retryable=True,
        )
    if observed_after != observed_before:
        return _session_classify_error(
            request,
            error_code="SOURCE_VERSION_CHANGED",
            message="file source version changed during classification",
            retryable=True,
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
) -> None:
    # A source/metadata race means no typed classifier result can be trusted.
    # Surface it through the outer operation error instead of fabricating the
    # forbidden typed shape `status=error, retryable=true`.
    raise _SessionOperationError(
        error_code=error_code,
        message=message,
        retryable=retryable,
        file_path=request.file_path,
    )


class _SessionOperationError(Exception):
    """Operation failed before it could form a trustworthy typed result."""

    def __init__(
        self,
        *,
        error_code: str,
        message: str,
        retryable: bool,
        file_path: str | None,
    ) -> None:
        super().__init__(message)
        self.error_code = error_code
        self.message = message
        self.retryable = retryable
        self.file_path = file_path


class _SessionProtocolError(Exception):
    """请求级协议损坏；会话写出唯一错误帧后退出非 0。"""

    def __init__(self, message: str) -> None:
        super().__init__(message)
        self.message = message


def _session_request_error() -> dict[str, object]:
    """无法解析出 request_id/operation 时使用固定 sentinel 的 transport error。"""
    return _worker_error(
        _SESSION_REQUEST_ERROR_REQUEST_ID,
        "pdf_classify",
        "INVALID_REQUEST",
        "stdin line is not a valid worker request",
        False,
        "request",
    )


def _session_operation_error(
    request_id: str,
    operation: str,
    message: str,
) -> dict[str, object]:
    return _worker_error(
        request_id, operation, "INVALID_REQUEST", message[:4096], False, "request"
    )


def _session_rejected_error(
    request_id: str,
    operation: str,
    error: _SessionOperationError,
) -> dict[str, object]:
    return _worker_error(
        request_id,
        operation,
        error.error_code,
        error.message[:4096],
        error.retryable,
        "parse",
        error.file_path,
    )


def _session_internal_error(
    request_id: str,
    operation: str,
    error: Exception,
) -> dict[str, object]:
    safe_message = str(error).strip()[:4096] or "session operation failed"
    return _worker_error(
        request_id, operation, "INTERNAL_ERROR", safe_message, True, "process"
    )


def _worker_error(
    request_id: str,
    operation: str,
    error_code: str,
    message: str,
    retryable: bool,
    stage: str,
    file_path: str | None = None,
) -> dict[str, object]:
    return {
        "contract": "ai_daily_worker",
        "protocol_version": 2,
        "request_id": request_id,
        "operation": operation,
        "status": "error",
        "result": None,
        "error": {
            "error_code": error_code,
            "message": message,
            "retryable": retryable,
            "stage": stage,
            "file_path": file_path,
            "backend": None,
        },
    }


def _emit_session_frame(payload: object) -> None:
    """session 每帧必须落盘；进程 seam 禁止依赖缓冲区冲刷时机。"""
    _emit_json(payload)
    sys.stdout.buffer.flush()


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
