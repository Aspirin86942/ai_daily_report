"""pypdfium2 PDF 文本层检测分类器。

`pdf_text_presence_v1`：在 `min(page_count, max_pages)` 窗口内，只要存在一枚
非空白、非控制类、非替换符的 Unicode scalar 即判定 `text_in_parse_window`，
首个有效字符即可停止实际检查（spec Part 3.1）。加密/确定性损坏返回
`error`（retryable=false），瞬时 I/O 返回 `unknown`（retryable=true），
绝不抛出裸异常。
"""

from __future__ import annotations

import unicodedata

from .pdf_classifier_identity import (
    CLASSIFIER_BUILD,
    CLASSIFIER_BUILD_INPUTS,
    CLASSIFIER_CONTRACT_VERSION,
    CLASSIFIER_PROTOCOL_VERSION,
    POLICY_VERSION,
    classifier_version_json,
    classifier_version_payload,
)


def _is_valid_text_char(ch: str) -> bool:
    """pdf_text_presence_v1 的有效文字判定（spec Part 3.1）。"""
    if ch.isspace() or ch == "�":
        return False
    return unicodedata.category(ch) not in ("Cc", "Cf", "Cs", "Co")


def _diagnostic(
    *,
    error_code: str,
    message: str,
    retryable: bool,
    file_path: str,
) -> dict[str, object]:
    return {
        "error_code": error_code,
        "message": message,
        "retryable": retryable,
        "stage": "process" if retryable else "parse",
        "file_path": file_path,
        "backend": None,
    }


def classify_pdf(path: str, max_pages: int, timeout_ms: int = 2000) -> dict[str, object]:
    """对单个 PDF 执行一次 ``pdf_text_presence_v1`` 分类。

    返回 ``ClassifyResult`` 形状的 dict：status 为
    ``text_in_parse_window|no_text_in_parse_window|unknown|error``，
    unknown/error 携带 ``WorkerDiagnostic`` 形状的 diagnostic，
    不抛裸异常。``timeout_ms`` 由进程级 runner 在调用方强制执行。
    """
    # Capability handshakes only need package/source identity. Load the native
    # PDF runtime here so snapshot-warm preflight does not pay DLL startup.
    try:
        import pypdfium2 as pdfium
        from pypdfium2 import PdfiumError
    except (ImportError, OSError) as error:
        return _failure_result(
            path=path,
            status="unknown",
            error_code="PARSER_START_FAILED",
            message=f"pdf runtime is unavailable: {error}",
            retryable=True,
        )

    try:
        pdf = pdfium.PdfDocument(path)
    except PdfiumError as error:
        return _failure_result(
            path=path,
            status="error",
            error_code="PARSER_FAILED",
            message=f"pdf could not be opened: {error}",
            retryable=False,
        )
    except OSError as error:
        return _failure_result(
            path=path,
            status="unknown",
            error_code="PARSER_START_FAILED",
            message=f"pdf metadata is unavailable: {error}",
            retryable=True,
        )
    try:
        page_count = len(pdf)
        if page_count == 0:
            return _failure_result(
                path=path,
                status="error",
                error_code="PARSER_FAILED",
                message="pdf contains no pages",
                retryable=False,
            )
        window = min(page_count, max_pages)
        for index in range(window):
            page = pdf[index]
            try:
                textpage = page.get_textpage()
                try:
                    text = textpage.get_text_range() or ""
                finally:
                    textpage.close()
            finally:
                page.close()
            if any(_is_valid_text_char(ch) for ch in text):
                return {
                    "status": "text_in_parse_window",
                    "page_count": page_count,
                    "result_examined_pages": index + 1,
                    "diagnostic": None,
                }
        return {
            "status": "no_text_in_parse_window",
            "page_count": page_count,
            "result_examined_pages": window,
            "diagnostic": None,
        }
    except PdfiumError as error:
        return _failure_result(
            path=path,
            status="error",
            error_code="PARSER_FAILED",
            message=f"pdf text extraction failed: {error}",
            retryable=False,
        )
    except Exception as error:  # noqa: BLE001 - 分类器永不抛裸异常
        return _failure_result(
            path=path,
            status="unknown",
            error_code="INTERNAL_ERROR",
            message=f"pdf classification failed: {error}",
            retryable=True,
        )
    finally:
        pdf.close()


def _failure_result(
    *,
    path: str,
    status: str,
    error_code: str,
    message: str,
    retryable: bool,
) -> dict[str, object]:
    return {
        "status": status,
        "page_count": None,
        "result_examined_pages": None,
        "diagnostic": _diagnostic(
            error_code=error_code,
            message=message,
            retryable=retryable,
            file_path=path,
        ),
    }
