"""Stdlib-only Python worker identity for the warm-cache handshake path."""

from __future__ import annotations

try:
    from _sha2 import sha256 as _sha256
except ImportError:  # Python 3.10-3.13 expose the same primitive here.
    try:
        from _sha256 import sha256 as _sha256
    except ImportError:
        from _hashlib import openssl_sha256 as _sha256


WORKER_CONTRACT_VERSION = "ai_daily_worker_v1"
PYTHON_WORKER_VERSION = "0.1.0"
PYTHON_WORKER_BUILD_INPUTS = (
    "requirements.lock",
    "src/models/scanner_contract.py",
    "src/models/schemas.py",
    "src/services/document_parser.py",
    "src/workers/contracts.py",
    "src/workers/document_parser_worker.py",
    "src/workers/python_worker_identity.py",
)
PYTHON_OFFICE_BACKEND = "python_office_v1"
PYTHON_SHAREPOINT_TEXT_BACKEND = "python_sharepoint_text_v1"
PDF_TEXT_BACKEND = "pdf_text_v1"


def _compute_python_worker_build() -> str:
    """Hash only the frozen repository-relative worker/parser source allowlist."""
    repository_root = __file__.replace("\\", "/").rsplit("/", 3)[0]
    digest = _sha256()
    for relative_path in PYTHON_WORKER_BUILD_INPUTS:
        path_bytes = relative_path.encode("utf-8", errors="strict")
        with open(f"{repository_root}/{relative_path}", "rb") as source:
            file_bytes = source.read()
        digest.update(len(path_bytes).to_bytes(8, "little"))
        digest.update(path_bytes)
        digest.update(len(file_bytes).to_bytes(8, "little"))
        digest.update(file_bytes)
    return digest.hexdigest()


PYTHON_WORKER_BUILD = _compute_python_worker_build()


def python_worker_version_payload() -> dict[str, object]:
    """Return the strict identity without importing Pydantic or parser modules."""
    return {
        "contract": "ai_daily_worker",
        "protocol_version": 1,
        "worker_kind": "python_document",
        "worker_contract_version": WORKER_CONTRACT_VERSION,
        "worker_version": PYTHON_WORKER_VERSION,
        "worker_build": PYTHON_WORKER_BUILD,
        "supported_backends": [
            PDF_TEXT_BACKEND,
            PYTHON_OFFICE_BACKEND,
            PYTHON_SHAREPOINT_TEXT_BACKEND,
        ],
        "supported_extensions": [
            ".doc",
            ".docx",
            ".pdf",
            ".ppt",
            ".pptx",
            ".xls",
            ".xlsx",
        ],
    }


_PYTHON_WORKER_VERSION_JSON = (
    b'{"contract":"ai_daily_worker","protocol_version":1,'
    b'"worker_kind":"python_document",'
    b'"worker_contract_version":"ai_daily_worker_v1",'
    b'"worker_version":"0.1.0","worker_build":"'
    + PYTHON_WORKER_BUILD.encode("ascii", errors="strict")
    + b'","supported_backends":["pdf_text_v1","python_office_v1",'
    b'"python_sharepoint_text_v1"],"supported_extensions":'
    b'[".doc",".docx",".pdf",".ppt",".pptx",".xls",".xlsx"]}'
)


def python_worker_version_json() -> bytes:
    """Return the strict version response without importing JSON machinery."""
    return _PYTHON_WORKER_VERSION_JSON
