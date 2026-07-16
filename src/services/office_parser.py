"""Office parser backend orchestration: Rust primary plus Python fallback."""

from __future__ import annotations

import importlib
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from time import perf_counter
from typing import Any

from ..models.schemas import FileContext
from .rust_cli_contract import RustCliContractError, run_rust_json_cli

RUST_OFFICE_BACKEND = "rust_office_oxide_v1"
RUST_XLSX_BOUNDED_BACKEND = "rust_xlsx_bounded_v1"
PYTHON_OFFICE_BACKEND = "python_office_v1"
PYTHON_SHAREPOINT_TEXT_BACKEND = "python_sharepoint_text_v1"
NOT_PARSED_BACKEND = "not_parsed"
OFFICE_FAILURE_DETERMINISTIC = "deterministic"
OFFICE_FAILURE_ENVIRONMENT_UNAVAILABLE = "environment_unavailable"
OFFICE_FAILURE_CONTRACT = "contract_failure"
OFFICE_FAILURE_RECOVERABLE = "recoverable_parser_failure"
OFFICE_FALLBACK_POLICY_VERSION = "hybrid_v1"
OFFICE_RUST_FILE_TYPES = {".docx", ".xlsx", ".pptx", ".doc", ".xls", ".ppt"}
DEFAULT_RUST_OFFICE_PARSER_BIN = (
    "rust/target/release/ai-daily-office-parser"
)

_MODERN_OFFICE_FILE_TYPES = {".docx", ".xlsx", ".pptx"}
_LEGACY_OFFICE_FILE_TYPES = {".doc", ".xls", ".ppt"}
_DEFAULT_FALLBACK_ORDER = (
    PYTHON_OFFICE_BACKEND,
    PYTHON_SHAREPOINT_TEXT_BACKEND,
)


@dataclass(frozen=True, slots=True)
class OfficeParseAudit:
    attempted_backend: str = ""
    fallback_backend: str = ""
    fallback_reason: str = ""
    rust_duration_ms: int = 0
    fallback_duration_ms: int = 0
    failure_class: str = ""


@dataclass(frozen=True, slots=True)
class OfficeParseOutcome:
    context: FileContext
    audit: OfficeParseAudit


class RustOfficeParserRunner:
    """Run the Rust Office parser CLI and validate its FileContext payload."""

    def __init__(self, binary_path: str | Path):
        self.binary_path = Path(binary_path)

    def parse(
        self,
        file_path: Path,
        file_type: str,
        limits: Mapping[str, Any],
        timeout_seconds: float,
    ) -> tuple[FileContext, int]:
        normalized_type = file_type.lower()
        request = {
            "path": str(file_path),
            "file_path": str(file_path),
            "file_type": normalized_type,
            "limits": dict(limits),
            "parser_backend": RUST_OFFICE_BACKEND,
        }

        result = run_rust_json_cli(
            binary_path=self.binary_path,
            request_payload=request,
            timeout_seconds=timeout_seconds,
            validator=lambda payload: _validate_rust_payload_context_from_json(
                payload,
                expected_file_path=str(file_path),
                expected_file_type=normalized_type,
            ),
            contract_name="rust_office_parser",
            json_indent=2,
        )

        if result.error is None and result.payload is not None:
            return result.payload, result.duration_ms

        error = result.error or RustCliContractError(
            kind="invalid_payload",
            message="Rust Office parser returned no payload",
        )
        public_error = _rust_office_error_from_contract(
            error,
            timeout_seconds=timeout_seconds,
        )
        return (
            _error_context(file_path, normalized_type, public_error),
            result.duration_ms,
        )


def _rust_office_error_from_contract(
    error: RustCliContractError,
    *,
    timeout_seconds: float,
) -> str:
    if error.kind == "timeout":
        return f"RUST_OFFICE_TIMEOUT: file parse exceeded {timeout_seconds:g}s"
    if error.kind == "start_failed":
        return f"RUST_OFFICE_START_FAILED: {error.message}"
    if error.kind == "nonzero_exit":
        return f"RUST_OFFICE_PARSE_FAILED: {error.message}"
    if error.kind in {"invalid_stdout_encoding", "invalid_json"}:
        return f"RUST_OFFICE_INVALID_JSON: {error.message}"
    return f"RUST_OFFICE_INVALID_PAYLOAD: {error.message}"


def _validate_rust_payload_context_from_json(
    payload: object,
    *,
    expected_file_path: str,
    expected_file_type: str,
) -> FileContext:
    if not isinstance(payload, Mapping):
        raise ValueError("stdout JSON must be an object")
    context = FileContext(**payload)
    _validate_rust_payload_context(
        context,
        expected_file_path=expected_file_path,
        expected_file_type=expected_file_type,
    )
    return context


PythonFallback = Callable[[Path, str, Mapping[str, Any]], FileContext]


@dataclass(frozen=True, slots=True)
class OfficeFallbackDecision:
    failure_class: str
    allow_fallback: bool
    reason: str


def classify_office_failure(
    *,
    file_type: str,
    rust_backend: str,
    rust_error: str,
    scanner_cfg: Mapping[str, Any],
) -> OfficeFallbackDecision:
    fallback_enabled = bool(scanner_cfg.get("office_parser_fallback_enabled", True))
    fallback_after_timeout = bool(scanner_cfg.get("office_fallback_after_timeout", False))
    normalized_type = file_type.lower()

    # 分类优先看 Rust error contract，避免 scanner benchmark 继续从文本原因里猜策略。
    if rust_error.startswith("RUST_OFFICE_TIMEOUT:"):
        return OfficeFallbackDecision(
            failure_class=OFFICE_FAILURE_DETERMINISTIC,
            allow_fallback=fallback_enabled and fallback_after_timeout,
            reason="timeout",
        )

    if (
        normalized_type == ".xlsx"
        and rust_backend == RUST_XLSX_BOUNDED_BACKEND
        and rust_error.startswith("RUST_XLSX_BOUNDED_PARSE_FAILED: ZIP error:")
    ):
        return OfficeFallbackDecision(
            failure_class=OFFICE_FAILURE_DETERMINISTIC,
            allow_fallback=False,
            reason="deterministic_xlsx_zip_error",
        )

    if rust_error.startswith("RUST_OFFICE_START_FAILED:"):
        return OfficeFallbackDecision(
            failure_class=OFFICE_FAILURE_ENVIRONMENT_UNAVAILABLE,
            allow_fallback=fallback_enabled,
            reason="rust_binary_unavailable",
        )

    if rust_error.startswith("RUST_OFFICE_INVALID_JSON:") or rust_error.startswith(
        "RUST_OFFICE_INVALID_PAYLOAD:"
    ):
        return OfficeFallbackDecision(
            failure_class=OFFICE_FAILURE_CONTRACT,
            allow_fallback=fallback_enabled,
            reason="rust_python_contract_failed",
        )

    return OfficeFallbackDecision(
        failure_class=OFFICE_FAILURE_RECOVERABLE,
        allow_fallback=fallback_enabled,
        reason="rust_parse_failed",
    )


def parse_office_with_fallback(
    *,
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
    scanner_cfg: Mapping[str, Any],
    timeout_seconds: float,
    rust_runner: RustOfficeParserRunner | None = None,
    python_fallback: PythonFallback | None = None,
) -> OfficeParseOutcome:
    normalized_type = file_type.lower()
    backend = str(scanner_cfg.get("office_parser_backend", RUST_OFFICE_BACKEND))
    fallback_order = _fallback_order(scanner_cfg)

    if backend != RUST_OFFICE_BACKEND:
        context = _run_configured_python_backend(
            file_path,
            normalized_type,
            limits,
            backend,
        )
        return OfficeParseOutcome(
            context=context,
            audit=OfficeParseAudit(attempted_backend=context.parser_backend or ""),
        )

    runner = rust_runner or RustOfficeParserRunner(
        scanner_cfg.get("rust_office_parser_bin", DEFAULT_RUST_OFFICE_PARSER_BIN)
    )
    rust_context, rust_duration_ms = runner.parse(
        file_path,
        normalized_type,
        limits,
        timeout_seconds,
    )
    if rust_context.error is None:
        attempted_backend = rust_context.parser_backend or RUST_OFFICE_BACKEND
        return OfficeParseOutcome(
            context=rust_context,
            audit=OfficeParseAudit(
                attempted_backend=attempted_backend,
                rust_duration_ms=rust_duration_ms,
            ),
        )

    fallback_reason = rust_context.error or ""
    attempted_backend = rust_context.parser_backend or RUST_OFFICE_BACKEND
    decision = classify_office_failure(
        file_type=normalized_type,
        rust_backend=attempted_backend,
        rust_error=fallback_reason,
        scanner_cfg=scanner_cfg,
    )

    if not decision.allow_fallback:
        return OfficeParseOutcome(
            context=rust_context,
            audit=OfficeParseAudit(
                attempted_backend=attempted_backend,
                fallback_reason=fallback_reason,
                rust_duration_ms=rust_duration_ms,
                failure_class=decision.failure_class,
            ),
        )

    fallback_started_at = perf_counter()
    fallback_context = _run_python_fallback(
        file_path,
        normalized_type,
        limits,
        python_fallback=python_fallback,
        fallback_order=fallback_order,
    )
    fallback_duration_ms = _elapsed_ms(fallback_started_at)

    if fallback_context.error is None:
        return OfficeParseOutcome(
            context=fallback_context,
            audit=OfficeParseAudit(
                attempted_backend=RUST_OFFICE_BACKEND,
                fallback_backend=fallback_context.parser_backend or "",
                fallback_reason=fallback_reason,
                rust_duration_ms=rust_duration_ms,
                fallback_duration_ms=fallback_duration_ms,
                failure_class=decision.failure_class,
            ),
        )

    return OfficeParseOutcome(
        context=FileContext(
            file_path=str(file_path),
            file_type=normalized_type,
            content="",
            error=(
                "OFFICE_PARSE_FAILED: "
                f"rust={rust_context.error}; python={fallback_context.error}"
            ),
            parser_backend=RUST_OFFICE_BACKEND,
            truncated=False,
        ),
        audit=OfficeParseAudit(
            attempted_backend=RUST_OFFICE_BACKEND,
            fallback_backend=fallback_context.parser_backend or "",
            fallback_reason=fallback_reason,
            rust_duration_ms=rust_duration_ms,
            fallback_duration_ms=fallback_duration_ms,
            failure_class=decision.failure_class,
        ),
    )


def parse_with_sharepoint_text(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
    *,
    import_module: Callable[[str], Any] = importlib.import_module,
) -> FileContext:
    from ..workers.contracts import parse_sharepoint_text_payload

    payload = parse_sharepoint_text_payload(
        file_path,
        file_type.lower(),
        limits,
        import_module=import_module,
    )
    return FileContext(
        file_path=payload.file_path,
        file_type=payload.file_type,
        content=payload.content,
        error=payload.error,
        parser_backend=payload.parser_backend,
        truncated=payload.truncated,
    )


def _run_python_fallback(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
    python_fallback: PythonFallback | None = None,
    fallback_order: Sequence[str] = _DEFAULT_FALLBACK_ORDER,
) -> FileContext:
    if python_fallback is not None:
        return python_fallback(file_path, file_type, limits)

    last_context: FileContext | None = None
    for backend in fallback_order:
        if backend == PYTHON_OFFICE_BACKEND and file_type in _MODERN_OFFICE_FILE_TYPES:
            context = _run_python_office(file_path, file_type, limits)
            if context.error is None:
                return context
            last_context = context
            continue

        if (
            backend == PYTHON_SHAREPOINT_TEXT_BACKEND
            and file_type in _LEGACY_OFFICE_FILE_TYPES
        ):
            context = parse_with_sharepoint_text(file_path, file_type, limits)
            if context.error is None:
                return context
            last_context = context

    if last_context is not None:
        return last_context

    return FileContext(
        file_path=str(file_path),
        file_type=file_type,
        content="",
        error=f"PYTHON_FALLBACK_UNAVAILABLE: {file_type}",
        parser_backend=NOT_PARSED_BACKEND,
        truncated=False,
    )


def _run_configured_python_backend(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
    backend: str,
) -> FileContext:
    if backend == PYTHON_OFFICE_BACKEND:
        return _run_python_office(file_path, file_type, limits)
    if backend == PYTHON_SHAREPOINT_TEXT_BACKEND:
        return parse_with_sharepoint_text(file_path, file_type, limits)
    return FileContext(
        file_path=str(file_path),
        file_type=file_type,
        content="",
        error=f"OFFICE_UNKNOWN_BACKEND: {backend}",
        parser_backend=NOT_PARSED_BACKEND,
        truncated=False,
    )


def _run_python_office(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
) -> FileContext:
    from .document_parser import DocumentParserOptions, parse_document_file

    return parse_document_file(
        file_path,
        file_type,
        limits,
        DocumentParserOptions(office_parser_backend=PYTHON_OFFICE_BACKEND),
    )


def _validate_rust_payload_context(
    context: FileContext,
    *,
    expected_file_path: str,
    expected_file_type: str,
) -> None:
    if context.file_path != expected_file_path:
        raise ValueError(
            f"file_path mismatch: expected {expected_file_path!r}, "
            f"got {context.file_path!r}"
        )
    if context.file_type != expected_file_type:
        raise ValueError(
            f"file_type mismatch: expected {expected_file_type!r}, "
            f"got {context.file_type!r}"
        )
    allowed_backends = {RUST_OFFICE_BACKEND}
    if expected_file_type == ".xlsx":
        allowed_backends.add(RUST_XLSX_BOUNDED_BACKEND)
    if context.parser_backend not in allowed_backends:
        raise ValueError(
            f"parser_backend mismatch: expected one of {sorted(allowed_backends)!r}, "
            f"got {context.parser_backend!r}"
        )


def _error_context(file_path: Path, file_type: str, error: str) -> FileContext:
    return FileContext(
        file_path=str(file_path),
        file_type=file_type,
        content="",
        error=error,
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )


def _elapsed_ms(started_at: float) -> int:
    return max(0, int((perf_counter() - started_at) * 1000))


def _positive_limit(limits: Mapping[str, Any], key: str, default: int) -> int:
    try:
        value = int(limits.get(key, default))
    except (TypeError, ValueError):
        return default
    return value if value > 0 else default


def _fallback_order(scanner_cfg: Mapping[str, Any]) -> Sequence[str]:
    order = scanner_cfg.get("office_parser_fallback_order", _DEFAULT_FALLBACK_ORDER)
    if isinstance(order, str):
        return (order,)
    if isinstance(order, Sequence):
        return tuple(str(item) for item in order)
    return _DEFAULT_FALLBACK_ORDER
