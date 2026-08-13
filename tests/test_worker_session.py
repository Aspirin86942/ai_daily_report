"""统一 worker v2 长驻进程合同。"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

from src.workers.models import ParseRequest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
TEXT_PDF = PROJECT_ROOT / "tests" / "fixtures" / "pdf_benchmark" / "case_01.pdf"


def _source_version(path: Path) -> str:
    stat = path.stat()
    return f"mtime_ns={stat.st_mtime_ns}:size={stat.st_size}"


def _spawn_session() -> subprocess.Popen:
    return subprocess.Popen(
        [
            sys.executable,
            "-m",
            "src.workers.document_parser_worker",
            "session",
        ],
        cwd=PROJECT_ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    )


def _readline(process: subprocess.Popen) -> dict[str, object]:
    line = process.stdout.readline()
    if not line:
        raise AssertionError(
            f"session stdout reached EOF (stderr={_drain_stderr(process)})"
        )
    return json.loads(line.decode("utf-8", errors="strict"))


def _drain_stderr(process: subprocess.Popen) -> str:
    if process.stderr is None:
        return ""
    try:
        return process.stderr.read().decode("utf-8", errors="replace")[-4096:]
    except Exception:  # noqa: BLE001 - 测试诊断不抛裸异常
        return ""


def _write_request(
    process: subprocess.Popen,
    request: dict[str, object],
) -> None:
    assert process.stdin is not None
    process.stdin.write((json.dumps(request, ensure_ascii=False) + "\n").encode("utf-8"))
    process.stdin.flush()


def _session_version_payload() -> dict[str, object]:
    completed = subprocess.run(
        [
            sys.executable,
            "-m",
            "src.workers.document_parser_worker",
            "hello",
        ],
        cwd=PROJECT_ROOT,
        input=b"",
        capture_output=True,
        check=False,
    )
    assert completed.returncode == 0, completed.stderr.decode(
        "utf-8", errors="replace"
    )
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    return payload


def test_worker_hello_does_not_import_heavy_pdf_runtime() -> None:
    """Snapshot warm preflight must not load PDFium or the Pydantic model graph."""
    completed = subprocess.run(
        [
            sys.executable,
            "-S",
            "-X",
            "importtime",
            "-m",
            "src.workers.document_parser_worker",
            "hello",
        ],
        cwd=PROJECT_ROOT,
        input=b"",
        capture_output=True,
        check=False,
    )
    assert completed.returncode == 0
    imports = completed.stderr.decode("utf-8", errors="replace")
    assert "pypdfium2" not in imports
    assert "pydantic" not in imports


def _classify_request(tmp_path: Path, request_id: str) -> dict[str, object]:
    pdf = TEXT_PDF
    if not pdf.is_file():
        pytest.skip("text pdf fixture is not built")
    return {
        "contract": "ai_daily_worker",
        "protocol_version": 2,
        "request_id": request_id,
        "operation": "pdf_classify",
        "payload": {
            "file_path": str(pdf),
            "source_version": _source_version(pdf),
            "max_pages": 5,
            "policy_version": "pdf_text_presence_v1",
        },
    }


def _parse_request(tmp_path: Path, request_id: str) -> dict[str, object]:
    pdf = TEXT_PDF
    if not pdf.is_file():
        pytest.skip("text pdf fixture is not built")
    source_version = _source_version(pdf)
    request = {
        "file_path": str(pdf),
        "file_type": ".pdf",
        "backend": "python_pdf_text_v2",
        "remaining_timeout_ms": 30_000,
        "max_file_size_bytes": 1_000_000,
        "parser_limits": {
            "kind": "pdf",
            "max_pages": 5,
            "excerpt_max_chars": 4000,
        },
        "expected_source_version": source_version,
    }
    ParseRequest.model_validate(request)
    return {
        "contract": "ai_daily_worker",
        "protocol_version": 2,
        "request_id": request_id,
        "operation": "pdf_parse",
        "payload": request,
    }


def test_worker_hello_is_strict_and_complete() -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    version = _session_version_payload()
    assert version["contract"] == "ai_daily_worker"
    assert version["protocol_version"] == 2
    assert version["worker_contract_version"] == "ai_daily_worker_v2"
    assert version["supported_operations"] == [
        "pdf_classify",
        "pdf_parse",
        "python_office_parse",
        "python_sharepoint_parse",
    ]
    assert len(version["worker_build"]) == 64

def test_session_hello_then_classify_request_response_pairing(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello["frame"] == "hello"
        assert hello["worker_contract_version"] == "ai_daily_worker_v2"

        request = _classify_request(tmp_path, "61111111-6111-4111-8111-611111111111")
        _write_request(process, request)
        response = _readline(process)
        assert response["request_id"] == request["request_id"]
        assert response["operation"] == "pdf_classify"
        assert response["status"] == "ok"
        assert response["error"] is None
        result = response["result"]
        assert result["status"] in {
            "text_in_parse_window",
            "no_text_in_parse_window",
        }
        assert result["diagnostic"] is None
    finally:
        process.stdin.close()
        process.wait(timeout=10)


def test_session_source_version_change_is_outer_retryable_error(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello["frame"] == "hello"

        request = _classify_request(tmp_path, "61222222-6122-4122-8122-612222222222")
        request["payload"]["source_version"] = "mtime_ns=1:size=1"
        _write_request(process, request)

        response = _readline(process)
        assert response["request_id"] == request["request_id"]
        assert response["operation"] == "pdf_classify"
        assert response["status"] == "error"
        assert response["result"] is None
        assert response["error"]["error_code"] == "SOURCE_VERSION_CHANGED"
        assert response["error"]["retryable"] is True

        process.wait(timeout=10)
        assert process.returncode not in (0, None)
    finally:
        if process.poll() is None:
            process.stdin.close()
            process.kill()
            process.wait(timeout=10)


def test_session_parse_request_pairs_request_id(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello["frame"] == "hello"

        request_id = "62222222-6222-4222-8222-622222222222"
        request = _parse_request(tmp_path, request_id)
        _write_request(process, request)
        response = _readline(process)
        assert response["request_id"] == request_id
        assert response["operation"] == "pdf_parse"
        assert response["status"] == "ok"
        assert response["error"] is None
        result = response["result"]
        assert "contract" not in result
        assert "request_id" not in result
        assert result["parser_backend"] == "python_pdf_text_v2"
        assert result["observed_source_version"] == request["payload"]["expected_source_version"]
    finally:
        process.stdin.close()
        process.wait(timeout=10)


def test_session_kills_on_contract_mismatch(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello["frame"] == "hello"

        # 错误的 contract + 空 payload → outer error frame，随后会话退出非 0
        _write_request(
            process,
            {
            "contract": "wrong",
            "protocol_version": 2,
                "request_id": "63333333-6333-4333-8333-633333333333",
            "operation": "pdf_parse",
                "payload": {},
            },
        )
        error_frame = _readline(process)
        assert error_frame["status"] == "error"
        assert error_frame["result"] is None
        diagnostic = error_frame["error"]
        assert diagnostic["error_code"] == "INVALID_REQUEST"
        assert diagnostic["stage"] == "request"

        process.wait(timeout=10)
        assert process.returncode not in (0, None), (
            "会话必须在发送错误帧后以非 0 退出"
        )
    finally:
        if process.poll() is None:
            process.stdin.close()
            process.kill()
            process.wait(timeout=10)


def test_session_rejects_bad_json_with_single_error_frame(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello["frame"] == "hello"

        assert process.stdin is not None
        process.stdin.write(b"not-json\n")
        process.stdin.flush()

        error_frame = _readline(process)
        assert error_frame["status"] == "error"
        assert error_frame["error"]["error_code"] == "INVALID_REQUEST"

        process.wait(timeout=10)
        assert process.returncode not in (0, None)
    finally:
        if process.poll() is None:
            process.stdin.close()
            process.kill()
            process.wait(timeout=10)


def test_session_rejects_unknown_parse_payload_field(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello["frame"] == "hello"

        # request id 只属于外层 envelope；payload 出现未知传输字段即协议损坏。
        request = _parse_request(tmp_path, "62222222-6222-4222-8222-622222222222")
        request["payload"]["request_id"] = "64444444-6444-4444-8444-644444444444"
        _write_request(process, request)
        error_frame = _readline(process)
        assert error_frame["status"] == "error"
        assert error_frame["error"]["error_code"] == "INVALID_REQUEST"

        process.wait(timeout=10)
        assert process.returncode not in (0, None)
    finally:
        if process.poll() is None:
            process.stdin.close()
            process.kill()
            process.wait(timeout=10)


def test_session_hello_build_matches_preflight_session_version(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    version = _session_version_payload()
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello == version
    finally:
        process.stdin.close()
        process.wait(timeout=10)
