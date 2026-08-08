"""Python 文档 worker 命令行入口。"""

from __future__ import annotations

import sys

from .python_worker_identity import python_worker_version_json


def main(argv: list[str] | tuple[str, ...] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if args == ["version"]:
        sys.stdout.buffer.write(python_worker_version_json() + b"\n")
        return 0

    if args == ["classifier-version"]:
        from .pdf_classifier import classifier_version_json

        sys.stdout.buffer.write(classifier_version_json())
        return 0

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

    _emit_json(invalid_request_response().model_dump(mode="json"))
    return 2


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
