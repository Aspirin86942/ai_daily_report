"""严格的 UTF-8 JSON 单请求/单响应子进程边界。"""

from __future__ import annotations

import json
import subprocess
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from time import perf_counter
from typing import Any, Generic, Literal, TypeVar

from pydantic import BaseModel, ValidationError

from src.models.scanner_contract import TransportErrorResponse


ResponseT = TypeVar("ResponseT", bound=BaseModel)
JsonProcessFailureKind = Literal[
    "timeout",
    "start_failed",
    "invalid_utf8",
    "invalid_json",
    "invalid_response",
    "request_id_mismatch",
    "unexpected_exit",
]


@dataclass(frozen=True, slots=True)
class JsonProcessFailure:
    kind: JsonProcessFailureKind
    message: str


@dataclass(frozen=True, slots=True)
class JsonProcessResult(Generic[ResponseT]):
    response: ResponseT | None
    transport_error: TransportErrorResponse | None
    failure: JsonProcessFailure | None
    exit_code: int | None
    stderr: str
    duration_ms: int


def run_json_process(
    *,
    command: Sequence[str | Path],
    request_payload: Mapping[str, Any] | None,
    response_model: type[ResponseT],
    timeout_seconds: float,
    expected_request_id: str | None = None,
    cwd: Path | None = None,
) -> JsonProcessResult[ResponseT]:
    """执行无 shell 的单次 JSON 调用，并只返回合同验证后的 DTO。"""
    started_at = perf_counter()
    request_bytes = None
    if request_payload is not None:
        request_bytes = json.dumps(
            request_payload,
            ensure_ascii=False,
            separators=(",", ":"),
        ).encode("utf-8", errors="strict")

    try:
        completed = subprocess.run(
            [str(part) for part in command],
            input=request_bytes,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=float(timeout_seconds),
            check=False,
            cwd=cwd,
        )
    except (subprocess.TimeoutExpired, TimeoutError):
        return _failure_result(
            started_at,
            "timeout",
            "process exceeded its deadline",
        )
    except OSError:
        return _failure_result(
            started_at,
            "start_failed",
            "process could not be started",
        )
    stderr = _decode_stderr(completed.stderr)
    stdout = completed.stdout
    if completed.returncode == 2:
        try:
            transport_error = TransportErrorResponse.model_validate_json(stdout)
        except (ValidationError, TypeError, ValueError) as exc:
            failure_kind = _validation_failure_kind(exc, stdout)
            return _failure_result(
                started_at,
                failure_kind,
                _validation_failure_message(
                    failure_kind,
                    invalid_response="exit 2 requires a transport error response",
                ),
                exit_code=completed.returncode,
                stderr=stderr,
            )
        return JsonProcessResult(
            response=None,
            transport_error=transport_error,
            failure=None,
            exit_code=completed.returncode,
            stderr=stderr,
            duration_ms=_elapsed_ms(started_at),
        )
    try:
        response = response_model.model_validate_json(stdout)
    except (ValidationError, TypeError, ValueError) as exc:
        failure_kind = _validation_failure_kind(exc, stdout)
        return _failure_result(
            started_at,
            failure_kind,
            _validation_failure_message(
                failure_kind,
                invalid_response="process response violates its contract",
            ),
            exit_code=completed.returncode,
            stderr=stderr,
        )
    if completed.returncode not in {0, 1}:
        return _failure_result(
            started_at,
            "unexpected_exit",
            "process returned an unexpected exit code",
            exit_code=completed.returncode,
            stderr=stderr,
        )
    response_status = getattr(response, "status", None)
    if (
        (completed.returncode == 0 and response_status == "error")
        or (completed.returncode == 1 and response_status != "error")
    ):
        return _failure_result(
            started_at,
            "unexpected_exit",
            "process exit code does not match response status",
            exit_code=completed.returncode,
            stderr=stderr,
        )
    if expected_request_id is not None and (
        getattr(response, "request_id", None) != expected_request_id
    ):
        return _failure_result(
            started_at,
            "request_id_mismatch",
            "response request id does not match the request",
            exit_code=completed.returncode,
            stderr=stderr,
        )
    return JsonProcessResult(
        response=response,
        transport_error=None,
        failure=None,
        exit_code=completed.returncode,
        stderr=stderr,
        duration_ms=_elapsed_ms(started_at),
    )


def _failure_result(
    started_at: float,
    kind: JsonProcessFailureKind,
    message: str,
    *,
    exit_code: int | None = None,
    stderr: str = "",
) -> JsonProcessResult[Any]:
    return JsonProcessResult(
        response=None,
        transport_error=None,
        failure=JsonProcessFailure(kind=kind, message=message),
        exit_code=exit_code,
        stderr=stderr,
        duration_ms=_elapsed_ms(started_at),
    )


def _decode_stderr(value: object) -> str:
    if not isinstance(value, bytes):
        return ""
    return value.decode("utf-8", errors="replace")


def _validation_failure_kind(
    error: ValidationError | TypeError | ValueError,
    stdout: bytes,
) -> JsonProcessFailureKind:
    if isinstance(error, ValidationError) and any(
        item.get("type") == "json_invalid" for item in error.errors()
    ):
        try:
            stdout.decode("utf-8", errors="strict")
        except UnicodeDecodeError:
            return "invalid_utf8"
        return "invalid_json"
    return "invalid_response"


def _validation_failure_message(
    failure_kind: JsonProcessFailureKind,
    *,
    invalid_response: str,
) -> str:
    if failure_kind == "invalid_utf8":
        return "process stdout is not valid UTF-8"
    if failure_kind == "invalid_json":
        return "process stdout is not one valid JSON response"
    return invalid_response


def _elapsed_ms(started_at: float) -> int:
    return max(0, int((perf_counter() - started_at) * 1000))
