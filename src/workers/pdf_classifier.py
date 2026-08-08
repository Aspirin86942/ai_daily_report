"""pypdfium2 PDF 文本层检测分类器。

`pdf_text_presence_v1`：在 `min(page_count, max_pages)` 窗口内，只要存在一枚
非空白、非控制类、非替换符的 Unicode scalar 即判定 `text_in_parse_window`，
首个有效字符即可停止实际检查（spec Part 3.1）。加密/确定性损坏返回
`error`（retryable=false），瞬时 I/O 返回 `unknown`（retryable=true），
绝不抛出裸异常。
"""

from __future__ import annotations

import hashlib
import importlib.metadata
import json
import platform
import sys
import unicodedata
from pathlib import Path

import pypdfium2 as pdfium
from pypdfium2 import PdfiumError

POLICY_VERSION = "pdf_text_presence_v1"
CLASSIFIER_CONTRACT_VERSION = "ai_daily_pdf_classifier_v1"
CLASSIFIER_PROTOCOL_VERSION = 1
_CLASSIFIER_DOMAIN = b"classifier-build-v1\0"

# 冻结的 classifier 源码 allowlist：这些文件内容变化必须改变 classifier build。
CLASSIFIER_BUILD_INPUTS = (
    "requirements.lock",
    "src/models/scanner_contract.py",
    "src/workers/document_parser_worker.py",
    "src/workers/pdf_classifier.py",
)


def _is_valid_text_char(ch: str) -> bool:
    """pdf_text_presence_v1 的有效文字判定（spec Part 3.1）。"""
    if ch.isspace() or ch == "�":
        return False
    return unicodedata.category(ch) not in ("Cc", "Cf", "Cs", "Co")


def _target_triple() -> str:
    arch = platform.machine().lower()
    if sys.platform == "win32":
        return f"{arch}-pc-windows-msvc"
    if sys.platform == "darwin":
        return f"{arch}-apple-darwin"
    return f"{arch}-unknown-linux-gnu"


def _pdfium_native_version() -> str:
    return str(pdfium.internal.PDFIUM_INFO)


def _pypdfium2_version() -> str:
    return importlib.metadata.version("pypdfium2")


def _compute_classifier_build() -> str:
    """独立 domain-separated SHA-256（spec Part 7.1）。

    输入为冻结 source allowlist + policy + 运行时身份 + exact pypdfium2/PDFium
    native 版本 + target triple；不使用含安装路径/编译时间的 ``sys.version``。
    """
    repository_root = Path(__file__).resolve().parents[2]
    digest = hashlib.sha256()
    digest.update(_CLASSIFIER_DOMAIN)
    for relative_path in CLASSIFIER_BUILD_INPUTS:
        path_bytes = relative_path.encode("utf-8", errors="strict")
        file_bytes = (repository_root / relative_path).read_bytes()
        digest.update(len(path_bytes).to_bytes(8, "little"))
        digest.update(path_bytes)
        digest.update(len(file_bytes).to_bytes(8, "little"))
        digest.update(file_bytes)
    metadata = {
        "policy_version": POLICY_VERSION,
        "python_implementation": sys.implementation.name,
        "python_version": platform.python_version(),
        "unicode_data_version": unicodedata.unidata_version,
        "pypdfium2_version": _pypdfium2_version(),
        "pdfium_version": _pdfium_native_version(),
        "target_triple": _target_triple(),
    }
    canonical = json.dumps(
        metadata,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8", errors="strict")
    digest.update(len(canonical).to_bytes(8, "little"))
    digest.update(canonical)
    return digest.hexdigest()


CLASSIFIER_BUILD = _compute_classifier_build()


def classifier_version_payload() -> dict[str, object]:
    """严格 ``ClassifierVersionResponseV1`` 的字段形状。"""
    return {
        "contract": "ai_daily_pdf_classifier",
        "protocol_version": CLASSIFIER_PROTOCOL_VERSION,
        "classifier_contract_version": CLASSIFIER_CONTRACT_VERSION,
        "classifier_build": CLASSIFIER_BUILD,
        "policy_version": POLICY_VERSION,
        "python_implementation": sys.implementation.name,
        "python_version": platform.python_version(),
        "unicode_data_version": unicodedata.unidata_version,
        "pypdfium2_version": _pypdfium2_version(),
        "pdfium_version": _pdfium_native_version(),
        "target_triple": _target_triple(),
    }


_CLASSIFIER_VERSION_JSON = json.dumps(
    classifier_version_payload(),
    ensure_ascii=False,
    separators=(",", ":"),
).encode("utf-8", errors="strict")


def classifier_version_json() -> bytes:
    """返回严格 ``classifier-version`` 单帧输出。"""
    return _CLASSIFIER_VERSION_JSON + b"\n"


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

    返回 ``PdfClassifierResultV1`` 形状的 dict：status 为
    ``text_in_parse_window|no_text_in_parse_window|unknown|error``，
    unknown/error 携带 ``PythonOperationDiagnosticV1`` 形状的 diagnostic，
    不抛裸异常。``timeout_ms`` 由进程级 runner 在调用方强制执行。
    """
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
