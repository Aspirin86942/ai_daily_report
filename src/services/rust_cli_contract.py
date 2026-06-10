"""Shared JSON stdin/stdout contract for Rust helper CLIs."""

from __future__ import annotations

import json
import subprocess
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path
from time import perf_counter
from typing import Any, Generic, Literal, TypeVar

T = TypeVar("T")
PayloadValidator = Callable[[Any], T]

RustCliContractErrorKind = Literal[
    "request_serialization_failed",
    "timeout",
    "start_failed",
    "nonzero_exit",
    "invalid_stdout_encoding",
    "invalid_json",
    "invalid_payload",
]


@dataclass(frozen=True, slots=True)
class RustCliContractError:
    kind: RustCliContractErrorKind
    message: str
    returncode: int | None = None
    stderr: str = ""
    stdout_excerpt: str = ""


@dataclass(frozen=True, slots=True)
class RustCliJsonResult(Generic[T]):
    payload: T | None
    error: RustCliContractError | None
    duration_ms: int
    binary_path: Path


def resolve_binary_path(
    binary_path: str | Path,
    *,
    project_root: Path | None = None,
) -> Path:
    """Resolve Rust helper binary paths relative to the repository root."""
    configured = Path(binary_path)
    if configured.is_absolute():
        return configured
    root = project_root if project_root is not None else Path(__file__).resolve().parents[2]
    return root / configured


def run_rust_json_cli(
    *,
    binary_path: str | Path,
    request_payload: Mapping[str, Any],
    timeout_seconds: float,
    validator: PayloadValidator[T],
    contract_name: str,
    project_root: Path | None = None,
    json_indent: int | None = None,
) -> RustCliJsonResult[T]:
    """Run a Rust CLI that accepts JSON stdin and returns trusted JSON stdout."""
    resolved_binary = resolve_binary_path(binary_path, project_root=project_root)
    started_at = perf_counter()

    try:
        request_json = json.dumps(
            request_payload,
            ensure_ascii=False,
            indent=json_indent,
        )
    except (TypeError, ValueError) as exc:
        return _error_result(
            binary_path=resolved_binary,
            started_at=started_at,
            kind="request_serialization_failed",
            message=str(exc),
        )

    try:
        completed = subprocess.run(
            [str(resolved_binary)],
            input=request_json,
            text=True,
            encoding="utf-8",
            errors="strict",
            capture_output=True,
            timeout=float(timeout_seconds),
            check=False,
        )
    except (subprocess.TimeoutExpired, TimeoutError) as exc:
        message = str(exc) or f"{contract_name} exceeded {timeout_seconds:g}s"
        return _error_result(
            binary_path=resolved_binary,
            started_at=started_at,
            kind="timeout",
            message=message,
        )
    except UnicodeDecodeError as exc:
        return _error_result(
            binary_path=resolved_binary,
            started_at=started_at,
            kind="invalid_stdout_encoding",
            message=str(exc),
        )
    except OSError as exc:
        return _error_result(
            binary_path=resolved_binary,
            started_at=started_at,
            kind="start_failed",
            message=str(exc),
        )

    if completed.returncode != 0:
        stderr = _string_or_empty(completed.stderr)
        stdout = _string_or_empty(completed.stdout)
        message = stderr.strip() or f"exit code {completed.returncode}"
        return _error_result(
            binary_path=resolved_binary,
            started_at=started_at,
            kind="nonzero_exit",
            message=message,
            returncode=completed.returncode,
            stderr=stderr,
            stdout_excerpt=_excerpt(stdout),
        )

    stdout = _string_or_empty(completed.stdout)
    stderr = _string_or_empty(completed.stderr)
    try:
        decoded_payload = json.loads(stdout)
    except json.JSONDecodeError as exc:
        return _error_result(
            binary_path=resolved_binary,
            started_at=started_at,
            kind="invalid_json",
            message=str(exc),
            stderr=stderr,
            stdout_excerpt=_excerpt(stdout),
        )

    try:
        trusted_payload = validator(decoded_payload)
    except Exception as exc:
        return _error_result(
            binary_path=resolved_binary,
            started_at=started_at,
            kind="invalid_payload",
            message=str(exc),
            stderr=stderr,
            stdout_excerpt=_excerpt(stdout),
        )

    return RustCliJsonResult(
        payload=trusted_payload,
        error=None,
        duration_ms=_elapsed_ms(started_at),
        binary_path=resolved_binary,
    )


def _error_result(
    *,
    binary_path: Path,
    started_at: float,
    kind: RustCliContractErrorKind,
    message: str,
    returncode: int | None = None,
    stderr: str = "",
    stdout_excerpt: str = "",
) -> RustCliJsonResult[Any]:
    return RustCliJsonResult(
        payload=None,
        error=RustCliContractError(
            kind=kind,
            message=message,
            returncode=returncode,
            stderr=stderr,
            stdout_excerpt=stdout_excerpt,
        ),
        duration_ms=_elapsed_ms(started_at),
        binary_path=binary_path,
    )


def _elapsed_ms(started_at: float) -> int:
    return max(0, int((perf_counter() - started_at) * 1000))


def _string_or_empty(value: object) -> str:
    return value if isinstance(value, str) else ""


def _excerpt(value: str, max_chars: int = 2000) -> str:
    if len(value) <= max_chars:
        return value
    return value[:max_chars]

