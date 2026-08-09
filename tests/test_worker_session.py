"""Python 流式 worker session（ai_daily_python_session_v1）的进程合同。

覆盖 spec Part 7：hello 首帧、逐请求响应配对、classify_pdf_v1 / parse_v1
两种 operation、错配/坏 payload 杀会话而不静默复用。
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

import pytest

from src.models.scanner_contract import (
    ClassifierVersionResponseV1,
    PythonSessionVersionResponseV1,
    WorkerParseRequest,
)


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
            "session-version",
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
    return PythonSessionVersionResponseV1.model_validate(payload).model_dump(mode="json")


def _classify_request(tmp_path: Path, request_id: str) -> dict[str, object]:
    pdf = TEXT_PDF
    if not pdf.is_file():
        pytest.skip("text pdf fixture is not built")
    return {
        "contract": "ai_daily_python_session",
        "protocol_version": 1,
        "request_id": request_id,
        "operation": "classify_pdf_v1",
        "payload": {
            "contract": "ai_daily_pdf_classifier",
            "protocol_version": 1,
            "request_id": request_id,
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
        "contract": "ai_daily_worker",
        "protocol_version": 1,
        "request_id": request_id,
        "file_path": str(pdf),
        "file_type": ".pdf",
        "backend": "pdf_text_v1",
        "remaining_timeout_ms": 30_000,
        "max_file_size_bytes": 1_000_000,
        "parser_limits": {
            "kind": "pdf",
            "max_pages": 5,
            "excerpt_max_chars": 4000,
        },
        "expected_source_version": source_version,
    }
    WorkerParseRequest.model_validate(request)
    return {
        "contract": "ai_daily_python_session",
        "protocol_version": 1,
        "request_id": request_id,
        "operation": "parse_v1",
        "payload": request,
    }


def test_session_version_is_strict_and_matches_classifier_build() -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    version = _session_version_payload()
    assert version["contract"] == "ai_daily_python_session"
    assert version["protocol_version"] == 1
    assert version["session_contract_version"] == "ai_daily_python_session_v1"
    assert version["supported_operations"] == ["classify_pdf_v1", "parse_v1"]
    assert len(version["worker_build"]) == 64
    assert len(version["classifier_build"]) == 64

    classifier_completed = subprocess.run(
        [
            sys.executable,
            "-m",
            "src.workers.document_parser_worker",
            "classifier-version",
        ],
        cwd=PROJECT_ROOT,
        input=b"",
        capture_output=True,
        check=False,
    )
    assert classifier_completed.returncode == 0
    classifier = ClassifierVersionResponseV1.model_validate_json(
        classifier_completed.stdout
    )
    assert version["classifier_build"] == classifier.classifier_build


def test_session_hello_then_classify_request_response_pairing(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello["frame"] == "hello"
        assert hello["session_contract_version"] == "ai_daily_python_session_v1"
        assert hello["supported_operations"] == ["classify_pdf_v1", "parse_v1"]

        request = _classify_request(tmp_path, "61111111-6111-4111-8111-611111111111")
        _write_request(process, request)
        response = _readline(process)
        assert response["request_id"] == request["request_id"]
        assert response["operation"] == "classify_pdf_v1"
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
        assert response["operation"] == "parse_v1"
        assert response["status"] == "ok"
        assert response["error"] is None
        result = response["result"]
        assert result["contract"] == "ai_daily_worker"
        assert result["request_id"] == request_id
        assert result["parser_backend"] == "pdf_text_v1"
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
                "contract": "ai_daily_worker",
                "protocol_version": 1,
                "request_id": "63333333-6333-4333-8333-633333333333",
                "operation": "parse_v1",
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


def test_session_rejects_mismatched_inner_request_id(tmp_path: Path) -> None:
    if not TEXT_PDF.is_file():
        pytest.skip("text pdf fixture is not built")
    process = _spawn_session()
    try:
        hello = _readline(process)
        assert hello["frame"] == "hello"

        # 外层 request_id 与内嵌 WorkerParseRequest.request_id 不一致 → 协议损坏
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
        assert hello["worker_build"] == version["worker_build"]
        assert hello["classifier_build"] == version["classifier_build"]
    finally:
        process.stdin.close()
        process.wait(timeout=10)
