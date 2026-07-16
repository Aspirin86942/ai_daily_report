"""Rust context v1 的严格 JSON 进程客户端。"""

from __future__ import annotations

import json
from datetime import date
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace
from uuid import UUID

import pytest

from src.models.scanner_contract import (
    ContextEnvelope,
    DoctorResponse,
    InspectRunResponse,
    TransportErrorResponse,
    VersionResponse,
)
from src.services.context_scheduler import ContextScheduleRequest
from src.services.json_process_client import (
    JsonProcessFailure,
    JsonProcessResult,
    run_json_process,
)
from src.services.rust_context_client import RustContextClient


PROJECT_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_DIR = (
    PROJECT_ROOT / "tests" / "fixtures" / "scanner_contract" / "v1"
)
SCANNER_BIN = (
    PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"
)


def _fixture(name: str) -> dict[str, object]:
    return json.loads((FIXTURE_DIR / name).read_text(encoding="utf-8"))


def _require_scanner_binary() -> None:
    if SCANNER_BIN.is_file():
        return
    if sys.platform == "win32":
        pytest.fail(
            "Windows integration requires cargo build --manifest-path "
            "rust/Cargo.toml --workspace --release --locked"
        )
    pytest.skip("Rust scanner release binary is not built")


def _synthetic_build_request(tmp_path: Path, request_id: str) -> dict[str, object]:
    work_dir = tmp_path / "合成 工作目录"
    work_dir.mkdir()
    (work_dir / "daily evidence.txt").write_text(
        "synthetic context evidence",
        encoding="utf-8",
    )
    request = _fixture("request.json")
    request.update(
        {
            "request_id": request_id,
            "work_dir": str(work_dir.resolve()),
            "start_date": "2000-01-01",
            "end_date": "2099-12-31",
            "scan_db_path": str((tmp_path / "scan_index_v2.sqlite3").resolve()),
            "adapters": {
                "office_worker_path": str(
                    (
                        PROJECT_ROOT
                        / "rust"
                        / "target"
                        / "release"
                        / "ai-daily-office-parser.exe"
                    ).resolve()
                ),
                "python_executable": str(Path(sys.executable).resolve()),
                "python_module_root": str(PROJECT_ROOT.resolve()),
                "python_document_worker_module": (
                    "src.workers.document_parser_worker"
                ),
            },
        }
    )
    return request


def _run_synthetic_build(
    tmp_path: Path,
    request_id: str,
) -> tuple[dict[str, object], ContextEnvelope]:
    request = _synthetic_build_request(tmp_path, request_id)
    completed = subprocess.run(
        [str(SCANNER_BIN), "build-context"],
        cwd=PROJECT_ROOT,
        input=json.dumps(request, ensure_ascii=False).encode("utf-8"),
        capture_output=True,
        check=False,
    )
    assert completed.returncode == 0, completed.stdout.decode(
        "utf-8",
        errors="replace",
    )
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    return request, ContextEnvelope.model_validate(payload)


def test_json_process_client_round_trips_strict_utf8_bytes(
    monkeypatch,
) -> None:
    response_payload = _fixture("response-ok.json")
    calls: list[tuple[object, dict[str, object]]] = []

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        request = json.loads(kwargs["input"].decode("utf-8", errors="strict"))
        assert request == {"message": "中文请求"}
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(
                response_payload,
                ensure_ascii=False,
            ).encode("utf-8"),
            stderr=b"",
        )

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "build-context"],
        request_payload={"message": "中文请求"},
        response_model=ContextEnvelope,
        timeout_seconds=5,
        expected_request_id=str(response_payload["request_id"]),
    )

    assert result.failure is None
    assert result.response is not None
    assert "合成文件" in result.response.file_context
    assert result.exit_code == 0
    assert result.stderr == ""
    assert calls[0][0] == ["ai-daily-scanner", "build-context"]
    assert calls[0][1]["timeout"] == 5.0
    assert calls[0][1]["check"] is False


def test_json_process_client_maps_timeout_without_trusting_output(
    monkeypatch,
) -> None:
    def fake_run(command, **kwargs):
        raise subprocess.TimeoutExpired(command, kwargs["timeout"])

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "build-context"],
        request_payload={"request_id": "ignored"},
        response_model=ContextEnvelope,
        timeout_seconds=0.25,
        expected_request_id="ignored",
    )

    assert result.response is None
    assert result.failure is not None
    assert result.failure.kind == "timeout"
    assert result.exit_code is None


def test_json_process_client_maps_missing_executable_to_start_failure(
    monkeypatch,
) -> None:
    def fake_run(command, **kwargs):
        raise FileNotFoundError("scanner executable is missing")

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["missing-scanner", "version"],
        request_payload=None,
        response_model=ContextEnvelope,
        timeout_seconds=1,
    )

    assert result.response is None
    assert result.failure is not None
    assert result.failure.kind == "start_failed"
    assert result.failure.message == "process could not be started"


def test_json_process_client_accepts_valid_error_envelope_on_exit_one(
    monkeypatch,
) -> None:
    response_payload = _fixture("response-error.json")
    private_stderr = "low-level parser path must stay internal"

    def fake_run(command, **kwargs):
        return SimpleNamespace(
            returncode=1,
            stdout=json.dumps(response_payload).encode("utf-8"),
            stderr=private_stderr.encode("utf-8"),
        )

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "build-context"],
        request_payload={"request_id": response_payload["request_id"]},
        response_model=ContextEnvelope,
        timeout_seconds=5,
        expected_request_id=str(response_payload["request_id"]),
    )

    assert result.failure is None
    assert result.response is not None
    assert result.response.status == "error"
    assert result.exit_code == 1
    assert result.stderr == private_stderr
    assert result.response.error is not None
    assert private_stderr not in result.response.error.message


def test_json_process_client_rejects_nonzero_exit_with_invalid_json(
    monkeypatch,
) -> None:
    private_stderr = "panic location must not become a user-facing message"

    def fake_run(command, **kwargs):
        return SimpleNamespace(
            returncode=101,
            stdout=b"not-json",
            stderr=private_stderr.encode("utf-8"),
        )

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "build-context"],
        request_payload={"request_id": "ignored"},
        response_model=ContextEnvelope,
        timeout_seconds=5,
        expected_request_id="ignored",
    )

    assert result.response is None
    assert result.failure is not None
    assert result.failure.kind == "invalid_json"
    assert private_stderr not in result.failure.message
    assert result.stderr == private_stderr
    assert result.exit_code == 101


def test_json_process_client_rejects_invalid_utf8_stdout(monkeypatch) -> None:
    def fake_run(command, **kwargs):
        return SimpleNamespace(returncode=0, stdout=b"\xff", stderr=b"")

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "build-context"],
        request_payload={"request_id": "ignored"},
        response_model=ContextEnvelope,
        timeout_seconds=5,
        expected_request_id="ignored",
    )

    assert result.response is None
    assert result.failure is not None
    assert result.failure.kind == "invalid_utf8"


def test_json_process_client_rejects_request_id_mismatch(monkeypatch) -> None:
    response_payload = _fixture("response-ok.json")

    def fake_run(command, **kwargs):
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(response_payload).encode("utf-8"),
            stderr=b"",
        )

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "build-context"],
        request_payload={"request_id": "different"},
        response_model=ContextEnvelope,
        timeout_seconds=5,
        expected_request_id="different",
    )

    assert result.response is None
    assert result.failure is not None
    assert result.failure.kind == "request_id_mismatch"


def test_json_process_client_rejects_contract_version_mismatch(
    monkeypatch,
) -> None:
    response_payload = _fixture("response-ok.json")
    response_payload["protocol_version"] = 2

    def fake_run(command, **kwargs):
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(response_payload).encode("utf-8"),
            stderr=b"",
        )

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "build-context"],
        request_payload={"request_id": response_payload["request_id"]},
        response_model=ContextEnvelope,
        timeout_seconds=5,
        expected_request_id=str(response_payload["request_id"]),
    )

    assert result.response is None
    assert result.failure is not None
    assert result.failure.kind == "invalid_response"


def test_json_process_client_rejects_unknown_response_fields(
    monkeypatch,
) -> None:
    response_payload = _fixture("response-ok.json")
    response_payload["unexpected"] = "must fail"

    def fake_run(command, **kwargs):
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(response_payload).encode("utf-8"),
            stderr=b"",
        )

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "build-context"],
        request_payload={"request_id": response_payload["request_id"]},
        response_model=ContextEnvelope,
        timeout_seconds=5,
        expected_request_id=str(response_payload["request_id"]),
    )

    assert result.response is None
    assert result.failure is not None
    assert result.failure.kind == "invalid_response"


def test_json_process_client_accepts_only_transport_error_on_exit_two(
    monkeypatch,
) -> None:
    response_payload = _fixture("transport-error.json")

    def fake_run(command, **kwargs):
        return SimpleNamespace(
            returncode=2,
            stdout=json.dumps(response_payload).encode("utf-8"),
            stderr=b"request decoder rejected stdin",
        )

    monkeypatch.setattr(
        "src.services.json_process_client.subprocess.run",
        fake_run,
    )

    result = run_json_process(
        command=["ai-daily-scanner", "doctor"],
        request_payload={"invalid": True},
        response_model=ContextEnvelope,
        timeout_seconds=5,
    )

    assert result.failure is None
    assert result.response is None
    assert result.transport_error is not None
    assert result.transport_error.error.error_code == "INVALID_REQUEST"
    assert result.exit_code == 2


def test_rust_context_client_builds_wire_request_from_raw_profile_only(
    tmp_path,
    monkeypatch,
) -> None:
    configured_work_dir = Path("含空格 工作目录")
    work_dir = tmp_path / configured_work_dir
    work_dir.mkdir()
    raw_profile = {
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".txt"],
        "max_workers": 2,
    }
    config = SimpleNamespace(
        work_dir=configured_work_dir,
        scanner_contract_profile=lambda: raw_profile.copy(),
    )
    response = ContextEnvelope.model_validate(_fixture("response-error.json"))
    version = VersionResponse.model_validate(
        _fixture("scanner-version-response.json")
    )
    captured: dict[str, object] = {}
    commands: list[list[str]] = []

    def fake_run_json_process(**kwargs):
        commands.append(kwargs["command"])
        if kwargs["command"][-1] == "version":
            return JsonProcessResult(
                response=version,
                transport_error=None,
                failure=None,
                exit_code=0,
                stderr="",
                duration_ms=1,
            )
        captured.update(kwargs)
        return JsonProcessResult(
            response=response,
            transport_error=None,
            failure=None,
            exit_code=1,
            stderr="",
            duration_ms=1,
        )

    monkeypatch.setattr(
        "src.services.rust_context_client.run_json_process",
        fake_run_json_process,
    )
    request_id = UUID("11111111-1111-4111-8111-111111111111")
    client = RustContextClient(
        config=config,
        project_root=tmp_path,
        request_id_factory=lambda: request_id,
    )

    result = client.build_context(
        ContextScheduleRequest(
            report_mode="daily",
            source="scan",
            start_date=date(2026, 7, 14),
            end_date=date(2026, 7, 15),
            compression_profile=None,
            user_input="绝不能进入 Rust wire payload",
        )
    )

    assert result == response
    payload = captured["request_payload"]
    assert isinstance(payload, dict)
    assert payload["scanner_profile"] == raw_profile
    assert payload["request_id"] == str(request_id)
    assert payload["work_dir"] == str(work_dir.resolve())
    assert payload["scan_db_path"] == str(
        (tmp_path / "data" / "db" / "scan_index_v2.sqlite3").resolve()
    )
    assert payload["adapters"] == {
        "office_worker_path": str(
            (
                tmp_path
                / "rust"
                / "target"
                / "release"
                / "ai-daily-office-parser.exe"
            ).resolve()
        ),
        "python_executable": str(Path(sys.executable).resolve()),
        "python_module_root": str(tmp_path.resolve()),
        "python_document_worker_module": "src.workers.document_parser_worker",
    }
    serialized = json.dumps(payload, ensure_ascii=False)
    assert "绝不能进入" not in serialized
    assert "api_key" not in serialized.lower()
    assert captured["expected_request_id"] == str(request_id)
    scanner_path = str(
        (
            tmp_path
            / "rust"
            / "target"
            / "release"
            / "ai-daily-scanner.exe"
        ).resolve()
    )
    assert commands == [
        [scanner_path, "version"],
        [scanner_path, "build-context"],
    ]


def test_rust_context_client_maps_untrusted_process_failure_without_stderr_leak(
    tmp_path,
    monkeypatch,
) -> None:
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    config = SimpleNamespace(
        work_dir=work_dir,
        scanner_contract_profile=lambda: {
            "schema_version": "scanner_profile_v1"
        },
    )
    private_stderr = "panic at private implementation path"

    def fake_run_json_process(**kwargs):
        return JsonProcessResult(
            response=None,
            transport_error=None,
            failure=JsonProcessFailure(
                kind="unexpected_exit",
                message="process returned an unexpected exit code",
            ),
            exit_code=101,
            stderr=private_stderr,
            duration_ms=19,
        )

    monkeypatch.setattr(
        "src.services.rust_context_client.run_json_process",
        fake_run_json_process,
    )
    client = RustContextClient(
        config=config,
        project_root=tmp_path,
        request_id_factory=lambda: UUID(
            "11111111-1111-4111-8111-111111111111"
        ),
    )

    response = client.build_context(
        ContextScheduleRequest(
            report_mode="daily",
            source="scan",
            start_date=date(2026, 7, 14),
            end_date=date(2026, 7, 15),
        )
    )

    assert response.status == "error"
    assert response.error is not None
    assert response.error.error_code == "RUST_CORE_CRASHED"
    assert private_stderr not in response.error.message
    assert response.summary.total_duration_ms == 19


def test_rust_context_client_rejects_engine_build_change_after_handshake(
    tmp_path,
    monkeypatch,
) -> None:
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    config = SimpleNamespace(
        work_dir=work_dir,
        scanner_contract_profile=lambda: {
            "schema_version": "scanner_profile_v1"
        },
    )
    version = VersionResponse.model_validate(
        _fixture("scanner-version-response.json")
    )
    changed_response = ContextEnvelope.model_validate(
        _fixture("response-error.json")
    ).model_copy(update={"engine_build": "replaced-after-handshake"})
    results = iter(
        [
            JsonProcessResult(
                response=version,
                transport_error=None,
                failure=None,
                exit_code=0,
                stderr="",
                duration_ms=2,
            ),
            JsonProcessResult(
                response=changed_response,
                transport_error=None,
                failure=None,
                exit_code=1,
                stderr="private replacement detail",
                duration_ms=3,
            ),
        ]
    )
    monkeypatch.setattr(
        "src.services.rust_context_client.run_json_process",
        lambda **kwargs: next(results),
    )
    client = RustContextClient(
        config=config,
        project_root=tmp_path,
        request_id_factory=lambda: UUID(
            "11111111-1111-4111-8111-111111111111"
        ),
    )

    response = client.build_context(
        ContextScheduleRequest(
            report_mode="daily",
            source="scan",
            start_date=date(2026, 7, 14),
            end_date=date(2026, 7, 15),
        )
    )

    assert response.status == "error"
    assert response.error is not None
    assert response.error.error_code == "RUST_CORE_CRASHED"
    assert "replacement" not in response.error.message
    assert response.summary.total_duration_ms == 5


def test_rust_scanner_version_is_requestless_strict_json() -> None:
    _require_scanner_binary()

    completed = subprocess.run(
        [str(SCANNER_BIN), "version"],
        cwd=PROJECT_ROOT,
        input=b"version must not read stdin",
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr.decode(
        "utf-8",
        errors="replace",
    )
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    version = VersionResponse.model_validate(payload)
    assert version.binary_name == "ai-daily-scanner"
    assert version.supported_commands == [
        "version",
        "doctor",
        "build-context",
        "inspect-run",
    ]
    assert version.office_worker_contract_version == "ai_daily_worker_v1"
    assert version.python_worker_contract_version == "ai_daily_worker_v1"


def test_rust_scanner_invalid_json_returns_transport_error_exit_two() -> None:
    _require_scanner_binary()

    completed = subprocess.run(
        [str(SCANNER_BIN), "build-context"],
        cwd=PROJECT_ROOT,
        input=b"not-json",
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 2
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    transport = TransportErrorResponse.model_validate(payload)
    assert transport.error.error_code == "INVALID_REQUEST"
    assert transport.error.stage == "request"


def test_rust_scanner_build_context_completes_synthetic_fixture(tmp_path) -> None:
    _require_scanner_binary()
    request, response = _run_synthetic_build(
        tmp_path,
        "11111111-1111-4111-8111-111111111111",
    )

    assert response.request_id == request["request_id"]
    assert response.status == "ok"
    assert "synthetic context evidence" in response.file_context
    assert response.scan_run_id is not None
    assert response.context_run_id is not None
    assert response.error is None
    assert response.summary.source_file_count == 1
    assert response.summary.success_count == 1
    assert response.summary.included_file_count == 1


def test_rust_scanner_inspect_run_returns_stable_read_only_dto(tmp_path) -> None:
    _require_scanner_binary()
    build_request, build_response = _run_synthetic_build(
        tmp_path,
        "31111111-3111-4111-8111-311111111111",
    )
    assert build_response.scan_run_id is not None
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "41111111-4111-4111-8111-411111111111",
        "scan_db_path": build_request["scan_db_path"],
        "scan_run_id": build_response.scan_run_id,
        "include_content": False,
    }

    completed = subprocess.run(
        [str(SCANNER_BIN), "inspect-run"],
        cwd=PROJECT_ROOT,
        input=json.dumps(request).encode("utf-8"),
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 0
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    response = InspectRunResponse.model_validate(payload)
    assert response.request_id == request["request_id"]
    assert response.scan_run_id == request["scan_run_id"]
    assert response.status == "ok"
    assert response.run_status == "success"
    assert response.context_run_id == build_response.context_run_id
    assert response.error is None
    assert len(response.files) == 1
    assert response.files[0].worker_lane == "rust_core"
    assert response.files[0].cache_status == "miss"
    assert len(response.decisions) == 1
    assert response.decisions[0].action == "keep"
    assert "file_context" not in payload


def test_rust_scanner_doctor_checks_db_parent_and_both_worker_handshakes(
    tmp_path,
) -> None:
    _require_scanner_binary()
    db_parent = tmp_path / "状态 数据库"
    db_parent.mkdir()
    scan_db_path = db_parent / "scan_index_v2.sqlite3"
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "22222222-2222-4222-8222-222222222222",
        "scan_db_path": str(scan_db_path.resolve()),
        "adapters": {
            "office_worker_path": str(
                (
                    PROJECT_ROOT
                    / "rust"
                    / "target"
                    / "release"
                    / "ai-daily-office-parser.exe"
                ).resolve()
            ),
            "python_executable": str(Path(sys.executable).resolve()),
            "python_module_root": str(PROJECT_ROOT.resolve()),
            "python_document_worker_module": (
                "src.workers.document_parser_worker"
            ),
        },
    }

    completed = subprocess.run(
        [str(SCANNER_BIN), "doctor"],
        cwd=tmp_path,
        input=json.dumps(request, ensure_ascii=False).encode("utf-8"),
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr.decode(
        "utf-8",
        errors="replace",
    )
    assert completed.stderr == b""
    payload = json.loads(completed.stdout.decode("utf-8", errors="strict"))
    response = DoctorResponse.model_validate(payload)
    assert response.request_id == request["request_id"]
    assert response.status == "ok"
    assert [check.name for check in response.checks] == [
        "scan_db_parent",
        "office_worker_handshake",
        "python_worker_handshake",
    ]
    assert all(check.status == "ok" for check in response.checks)
    assert response.error is None
    assert response.warnings == []
    assert not scan_db_path.exists()


def test_rust_scanner_doctor_accepts_worker_capability_supersets(
    tmp_path,
) -> None:
    _require_scanner_binary()
    payload = _fixture("python-worker-version-response.json")
    payload["supported_backends"] = sorted(
        [*payload["supported_backends"], "custom_backend_v1"]
    )
    payload["supported_extensions"] = sorted(
        [*payload["supported_extensions"], ".csv"]
    )
    module = tmp_path / "fake_worker.py"
    module.write_text(
        "import json\n"
        f"print(json.dumps({payload!r}, ensure_ascii=False))\n",
        encoding="utf-8",
    )
    db_parent = tmp_path / "db"
    db_parent.mkdir()
    scan_db_path = db_parent / "scan.sqlite3"
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "22222222-2222-4222-8222-222222222222",
        "scan_db_path": str(scan_db_path.resolve()),
        "adapters": {
            "office_worker_path": str(
                (
                    PROJECT_ROOT
                    / "rust"
                    / "target"
                    / "release"
                    / "ai-daily-office-parser.exe"
                ).resolve()
            ),
            "python_executable": str(Path(sys.executable).resolve()),
            "python_module_root": str(tmp_path.resolve()),
            "python_document_worker_module": "fake_worker",
        },
    }

    completed = subprocess.run(
        [str(SCANNER_BIN), "doctor"],
        cwd=tmp_path,
        input=json.dumps(request).encode("utf-8"),
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 0
    assert completed.stderr == b""
    response = DoctorResponse.model_validate_json(completed.stdout)
    assert response.status == "ok"
    assert response.error is None
    assert not scan_db_path.exists()


@pytest.mark.parametrize(
    "failure_case",
    [
        "wrong_kind",
        "wrong_contract",
        "wrong_version",
        "missing_backend",
        "missing_extension",
        "empty_build",
        "extra_stdout",
        "nonzero_exit",
    ],
)
def test_rust_scanner_doctor_rejects_invalid_python_worker_handshake(
    tmp_path,
    failure_case,
) -> None:
    _require_scanner_binary()
    payload = _fixture("python-worker-version-response.json")
    if failure_case == "wrong_kind":
        payload["worker_kind"] = "office"
    elif failure_case == "wrong_contract":
        payload["worker_contract_version"] = "ai_daily_worker_v2"
    elif failure_case == "wrong_version":
        payload["worker_version"] = "9.9.9"
    elif failure_case == "missing_backend":
        payload["supported_backends"] = ["pdf_text_v1", "python_office_v1"]
    elif failure_case == "missing_extension":
        payload["supported_extensions"] = payload["supported_extensions"][:-1]
    elif failure_case == "empty_build":
        payload["worker_build"] = ""

    stdout = json.dumps(payload)
    if failure_case == "extra_stdout":
        stdout = f"{stdout}\n{stdout}"
    exit_code = 3 if failure_case == "nonzero_exit" else 0
    module = tmp_path / "fake_worker.py"
    module.write_text(
        "import sys\n"
        f"sys.stdout.write({stdout!r})\n"
        f"raise SystemExit({exit_code})\n",
        encoding="utf-8",
    )
    db_parent = tmp_path / "db"
    db_parent.mkdir()
    scan_db_path = db_parent / "scan.sqlite3"
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": "22222222-2222-4222-8222-222222222222",
        "scan_db_path": str(scan_db_path.resolve()),
        "adapters": {
            "office_worker_path": str(
                (
                    PROJECT_ROOT
                    / "rust"
                    / "target"
                    / "release"
                    / "ai-daily-office-parser.exe"
                ).resolve()
            ),
            "python_executable": str(Path(sys.executable).resolve()),
            "python_module_root": str(tmp_path.resolve()),
            "python_document_worker_module": "fake_worker",
        },
    }

    completed = subprocess.run(
        [str(SCANNER_BIN), "doctor"],
        cwd=tmp_path,
        input=json.dumps(request).encode("utf-8"),
        capture_output=True,
        check=False,
    )

    assert completed.returncode == 1
    assert completed.stderr == b""
    response = DoctorResponse.model_validate_json(completed.stdout)
    assert response.status == "error"
    assert response.error is not None
    assert response.error.error_code == "WORKER_HANDSHAKE_FAILED"
    python_check = next(
        check
        for check in response.checks
        if check.name == "python_worker_handshake"
    )
    assert python_check.status == "error"
    assert not scan_db_path.exists()
