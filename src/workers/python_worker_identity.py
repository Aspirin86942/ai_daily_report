"""Stdlib-only Python worker identity for the warm-cache handshake path."""

from __future__ import annotations

try:
    from _sha2 import sha256 as _sha256
except ImportError:  # Keep hashing available without third-party imports.
    try:
        from _sha256 import sha256 as _sha256
    except ImportError:
        from _hashlib import openssl_sha256 as _sha256


WORKER_CONTRACT_VERSION = "ai_daily_worker_v2"
PYTHON_WORKER_VERSION = "0.1.0"
PYTHON_WORKER_BUILD_INPUTS = (
    "requirements.lock",
    "src/models/scanner_contract.py",
    "src/models/schemas.py",
    "src/services/document_parser.py",
    "src/workers/contracts.py",
    "src/workers/document_parser_worker.py",
    "src/workers/models.py",
    "src/workers/pdf_classifier.py",
    "src/workers/pdf_classifier_identity.py",
    "src/workers/python_worker_identity.py",
)
PYTHON_OFFICE_BACKEND = "python_office_v2"
PYTHON_SHAREPOINT_TEXT_BACKEND = "python_sharepoint_text_v2"
PDF_TEXT_BACKEND = "python_pdf_text_v2"
WORKER_OPERATIONS = [
    "pdf_classify",
    "pdf_parse",
    "python_office_parse",
    "python_sharepoint_parse",
]


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


def python_worker_hello_payload() -> dict[str, object]:
    """Return the worker-v2 hello without importing parser dependencies."""
    return {
        "contract": "ai_daily_worker",
        "protocol_version": 2,
        "frame": "hello",
        "worker_contract_version": WORKER_CONTRACT_VERSION,
        "worker_kind": "python_document",
        "worker_version": PYTHON_WORKER_VERSION,
        "worker_build": PYTHON_WORKER_BUILD,
        "supported_operations": WORKER_OPERATIONS,
    }
