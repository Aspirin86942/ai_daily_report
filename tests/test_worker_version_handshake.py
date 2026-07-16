"""Worker v1 的无请求体 version 进程合同。"""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys

import pytest

from src.models.scanner_contract import (
    TransportErrorResponse,
    WorkerParseResponse,
    WorkerVersionResponse,
)


PROJECT_ROOT = Path(__file__).resolve().parents[1]
OFFICE_WORKER_BIN = (
    PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-office-parser.exe"
)


def _require_office_worker() -> None:
    if OFFICE_WORKER_BIN.is_file():
        return
    if sys.platform == "win32":
        pytest.fail(
            "Windows integration requires cargo build --manifest-path "
            "rust/Cargo.toml --workspace --release --locked"
        )
    pytest.skip("Rust Office worker release binary is not built")


def test_python_document_worker_version_is_strict_requestless_json() -> None:
    completed = subprocess.run(
        [
            sys.executable,
            "-m",
            "src.workers.document_parser_worker",
            "version",
        ],
        cwd=PROJECT_ROOT,
        input=b"",
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr.decode(
        "utf-8",
        errors="replace",
    )
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    version = WorkerVersionResponse.model_validate(payload)
    assert version.worker_kind == "python_document"
    assert version.worker_contract_version == "ai_daily_worker_v1"
    assert version.supported_backends == [
        "pdf_text_v1",
        "python_office_v1",
        "python_sharepoint_text_v1",
    ]
    assert version.supported_extensions == [
        ".doc",
        ".docx",
        ".pdf",
        ".ppt",
        ".pptx",
        ".xls",
        ".xlsx",
    ]


def test_python_document_worker_parse_returns_transitional_not_implemented() -> None:
    request_path = (
        PROJECT_ROOT
        / "tests"
        / "fixtures"
        / "scanner_contract"
        / "v1"
        / "worker-parse-pdf-request.json"
    )
    request = json.loads(request_path.read_text(encoding="utf-8"))
    worker_env = os.environ.copy()
    worker_env["PYTHONIOENCODING"] = "cp1252"

    completed = subprocess.run(
        [
            sys.executable,
            "-m",
            "src.workers.document_parser_worker",
            "parse",
        ],
        cwd=PROJECT_ROOT,
        input=json.dumps(request, ensure_ascii=False).encode("utf-8"),
        capture_output=True,
        check=False,
        env=worker_env,
    )

    assert completed.returncode == 1
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    response = WorkerParseResponse.model_validate(payload)
    assert response.request_id == request["request_id"]
    assert response.status == "error"
    assert response.content == ""
    assert response.parser_backend == request["backend"]
    assert response.worker_lane == "python_document_process"
    assert response.observed_source_version == request["expected_source_version"]
    assert response.error is not None
    assert response.error.error_code == "NOT_IMPLEMENTED"
    assert response.error.retryable is False


def test_python_document_worker_invalid_request_uses_transport_error() -> None:
    completed = subprocess.run(
        [
            sys.executable,
            "-m",
            "src.workers.document_parser_worker",
            "parse",
        ],
        cwd=PROJECT_ROOT,
        input=b"not-json",
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 2
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    response = TransportErrorResponse.model_validate(payload)
    assert response.error.error_code == "INVALID_REQUEST"
    assert response.error.stage == "request"
    assert response.error.file_path is None
    assert response.error.backend is None


def test_office_worker_version_preserves_legacy_binary_and_ignores_stdin() -> None:
    _require_office_worker()

    completed = subprocess.run(
        [str(OFFICE_WORKER_BIN), "version"],
        cwd=PROJECT_ROOT,
        input=b"version commands do not read stdin",
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr.decode(
        "utf-8",
        errors="replace",
    )
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    version = WorkerVersionResponse.model_validate(payload)
    assert version.worker_kind == "office"
    assert version.worker_contract_version == "ai_daily_worker_v1"
    assert version.supported_backends == [
        "rust_office_oxide_v1",
        "rust_xlsx_bounded_v1",
    ]
    assert version.supported_extensions == [".docx", ".pptx", ".xlsx"]
