import json
import subprocess
from pathlib import Path
from types import SimpleNamespace

from src.services.rust_cli_contract import run_rust_json_cli


def test_run_rust_json_cli_resolves_relative_binary_and_returns_validated_payload(
    tmp_path, monkeypatch
):
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        request = json.loads(kwargs["input"])
        assert request == {"message": "hello"}
        assert kwargs["text"] is True
        assert kwargs["encoding"] == "utf-8"
        assert kwargs["errors"] == "strict"
        assert kwargs["capture_output"] is True
        assert kwargs["timeout"] == 4.0
        assert kwargs["check"] is False
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps({"value": 42}),
            stderr="",
        )

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"message": "hello"},
        timeout_seconds=4,
        validator=lambda payload: payload["value"],
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert result.error is None
    assert result.payload == 42
    assert result.binary_path == tmp_path / "bin/helper"
    assert result.duration_ms >= 0
    assert calls[0][0][0] == [str(tmp_path / "bin/helper")]


def test_run_rust_json_cli_returns_serialization_error_without_starting_process(
    tmp_path, monkeypatch
):
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        raise AssertionError("subprocess should not start")

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"bad": {Path("not-json")}},
        timeout_seconds=4,
        validator=lambda payload: payload,
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert calls == []
    assert result.payload is None
    assert result.error is not None
    assert result.error.kind == "request_serialization_failed"
    assert result.duration_ms >= 0


def test_run_rust_json_cli_maps_timeout_to_contract_error(tmp_path, monkeypatch):
    def fake_run(*args, **kwargs):
        raise subprocess.TimeoutExpired(cmd=args[0], timeout=kwargs["timeout"])

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"ok": True},
        timeout_seconds=2,
        validator=lambda payload: payload,
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert result.payload is None
    assert result.error is not None
    assert result.error.kind == "timeout"
    assert result.duration_ms >= 0


def test_run_rust_json_cli_maps_start_failure_to_contract_error(tmp_path, monkeypatch):
    def fake_run(*args, **kwargs):
        raise OSError("missing binary")

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"ok": True},
        timeout_seconds=2,
        validator=lambda payload: payload,
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert result.payload is None
    assert result.error is not None
    assert result.error.kind == "start_failed"
    assert "missing binary" in result.error.message


def test_run_rust_json_cli_maps_nonzero_exit_to_contract_error(tmp_path, monkeypatch):
    def fake_run(*args, **kwargs):
        return SimpleNamespace(returncode=3, stdout="ignored", stderr="boom")

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"ok": True},
        timeout_seconds=2,
        validator=lambda payload: payload,
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert result.payload is None
    assert result.error is not None
    assert result.error.kind == "nonzero_exit"
    assert result.error.message == "boom"
    assert result.error.returncode == 3
    assert result.error.stderr == "boom"
    assert result.error.stdout_excerpt == "ignored"


def test_run_rust_json_cli_uses_exit_code_when_stderr_is_empty(tmp_path, monkeypatch):
    def fake_run(*args, **kwargs):
        return SimpleNamespace(returncode=7, stdout="", stderr="")

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"ok": True},
        timeout_seconds=2,
        validator=lambda payload: payload,
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert result.error is not None
    assert result.error.kind == "nonzero_exit"
    assert result.error.message == "exit code 7"


def test_run_rust_json_cli_maps_invalid_stdout_encoding(tmp_path, monkeypatch):
    def fake_run(*args, **kwargs):
        raise UnicodeDecodeError("utf-8", b"\xff", 0, 1, "invalid start byte")

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"ok": True},
        timeout_seconds=2,
        validator=lambda payload: payload,
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert result.payload is None
    assert result.error is not None
    assert result.error.kind == "invalid_stdout_encoding"


def test_run_rust_json_cli_maps_invalid_json_to_contract_error(tmp_path, monkeypatch):
    def fake_run(*args, **kwargs):
        return SimpleNamespace(returncode=0, stdout="not-json", stderr="")

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"ok": True},
        timeout_seconds=2,
        validator=lambda payload: payload,
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert result.payload is None
    assert result.error is not None
    assert result.error.kind == "invalid_json"
    assert result.error.stdout_excerpt == "not-json"


def test_run_rust_json_cli_maps_validator_failure_to_contract_error(
    tmp_path, monkeypatch
):
    def fake_run(*args, **kwargs):
        return SimpleNamespace(returncode=0, stdout=json.dumps({"bad": True}), stderr="")

    def validate_payload(payload):
        raise ValueError("missing value")

    monkeypatch.setattr("src.services.rust_cli_contract.subprocess.run", fake_run)

    result = run_rust_json_cli(
        binary_path="bin/helper",
        request_payload={"ok": True},
        timeout_seconds=2,
        validator=validate_payload,
        contract_name="test_helper",
        project_root=tmp_path,
    )

    assert result.payload is None
    assert result.error is not None
    assert result.error.kind == "invalid_payload"
    assert "missing value" in result.error.message

