"""Office parser backend orchestration: Rust primary plus Python fallback."""

from __future__ import annotations

import importlib
import json
import subprocess
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from time import perf_counter
from typing import Any

from ..models.schemas import FileContext

RUST_OFFICE_BACKEND = "rust_office_oxide_v1"
RUST_XLSX_BOUNDED_BACKEND = "rust_xlsx_bounded_v1"
PYTHON_OFFICE_BACKEND = "python_office_v1"
PYTHON_SHAREPOINT_TEXT_BACKEND = "python_sharepoint_text_v1"
NOT_PARSED_BACKEND = "not_parsed"
OFFICE_RUST_FILE_TYPES = {".docx", ".xlsx", ".pptx", ".doc", ".xls", ".ppt"}
DEFAULT_RUST_OFFICE_PARSER_BIN = (
    "rust/office_parser/target/release/ai-daily-office-parser"
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


@dataclass(frozen=True, slots=True)
class OfficeParseOutcome:
    context: FileContext
    audit: OfficeParseAudit


class RustOfficeParserRunner:
    """Run the Rust Office parser CLI and validate its FileContext payload."""

    def __init__(self, binary_path: str | Path):
        self.binary_path = Path(binary_path)

    def _resolve_binary_path(self) -> Path:
        """Resolve relative Rust binary paths from the project root."""
        if self.binary_path.is_absolute():
            return self.binary_path
        project_root = Path(__file__).resolve().parents[2]
        return project_root / self.binary_path

    def parse(
        self,
        file_path: Path,
        file_type: str,
        limits: Mapping[str, Any],
        timeout_seconds: float,
    ) -> tuple[FileContext, int]:
        started_at = perf_counter()
        normalized_type = file_type.lower()
        request = {
            "path": str(file_path),
            "file_path": str(file_path),
            "file_type": normalized_type,
            "limits": dict(limits),
            "parser_backend": RUST_OFFICE_BACKEND,
        }

        try:
            completed = subprocess.run(
                [str(self._resolve_binary_path())],
                input=json.dumps(request, ensure_ascii=False, indent=2),
                text=True,
                encoding="utf-8",
                errors="strict",
                capture_output=True,
                timeout=float(timeout_seconds),
                check=False,
            )
        except (subprocess.TimeoutExpired, TimeoutError):
            return (
                _error_context(
                    file_path,
                    normalized_type,
                    f"RUST_OFFICE_TIMEOUT: file parse exceeded {timeout_seconds:g}s",
                ),
                _elapsed_ms(started_at),
            )
        except OSError as exc:
            return (
                _error_context(
                    file_path,
                    normalized_type,
                    f"RUST_OFFICE_START_FAILED: {exc}",
                ),
                _elapsed_ms(started_at),
            )

        if completed.returncode != 0:
            message = completed.stderr.strip() or f"exit code {completed.returncode}"
            return (
                _error_context(
                    file_path,
                    normalized_type,
                    f"RUST_OFFICE_PARSE_FAILED: {message}",
                ),
                _elapsed_ms(started_at),
            )

        try:
            payload = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            return (
                _error_context(
                    file_path,
                    normalized_type,
                    f"RUST_OFFICE_INVALID_JSON: {exc}",
                ),
                _elapsed_ms(started_at),
            )

        try:
            context = FileContext(**payload)
            _validate_rust_payload_context(
                context,
                expected_file_path=str(file_path),
                expected_file_type=normalized_type,
            )
            return context, _elapsed_ms(started_at)
        except Exception as exc:
            return (
                _error_context(
                    file_path,
                    normalized_type,
                    f"RUST_OFFICE_INVALID_PAYLOAD: {exc}",
                ),
                _elapsed_ms(started_at),
            )


PythonFallback = Callable[[Path, str, Mapping[str, Any]], FileContext]


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

    fallback_enabled = bool(scanner_cfg.get("office_parser_fallback_enabled", True))
    fallback_after_timeout = bool(scanner_cfg.get("office_fallback_after_timeout", False))
    fallback_reason = rust_context.error or ""
    is_timeout = fallback_reason.startswith("RUST_OFFICE_TIMEOUT:")
    attempted_backend = rust_context.parser_backend or RUST_OFFICE_BACKEND

    if (
        not fallback_enabled
        or (is_timeout and not fallback_after_timeout)
        or _should_skip_python_fallback(normalized_type, rust_context)
    ):
        return OfficeParseOutcome(
            context=rust_context,
            audit=OfficeParseAudit(
                attempted_backend=attempted_backend,
                fallback_reason=fallback_reason,
                rust_duration_ms=rust_duration_ms,
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
        ),
    )


def parse_with_sharepoint_text(
    file_path: Path,
    file_type: str,
    limits: Mapping[str, Any],
    *,
    import_module: Callable[[str], Any] = importlib.import_module,
) -> FileContext:
    normalized_type = file_type.lower()
    try:
        sharepoint2text = import_module("sharepoint2text")
    except ModuleNotFoundError:
        return FileContext(
            file_path=str(file_path),
            file_type=normalized_type,
            content="",
            error="PYTHON_SHAREPOINT_TEXT_UNAVAILABLE: sharepoint2text",
            parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
            truncated=False,
        )

    try:
        result = next(sharepoint2text.read_file(str(file_path)))
        raw_text = result.get_full_text()
    except Exception as exc:
        return FileContext(
            file_path=str(file_path),
            file_type=normalized_type,
            content="",
            error=f"PYTHON_SHAREPOINT_TEXT_FAILED: {exc}",
            parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
            truncated=False,
        )

    max_chars = _positive_limit(limits, "document_excerpt_max_chars", 6000)
    content, truncated = _truncate_text(raw_text or "No Office text extracted", max_chars)
    return FileContext(
        file_path=str(file_path),
        file_type=normalized_type,
        content=content,
        error=None,
        parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
        truncated=truncated,
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


def _should_skip_python_fallback(
    normalized_type: str,
    rust_context: FileContext,
) -> bool:
    """确定性坏 xlsx 不再进入 Python fallback，避免 warm scan 反复慢失败。"""
    if normalized_type != ".xlsx":
        return False
    if rust_context.parser_backend != RUST_XLSX_BOUNDED_BACKEND:
        return False
    error = rust_context.error or ""
    return error.startswith("RUST_XLSX_BOUNDED_PARSE_FAILED: ZIP error:")


def _positive_limit(limits: Mapping[str, Any], key: str, default: int) -> int:
    try:
        value = int(limits.get(key, default))
    except (TypeError, ValueError):
        return default
    return value if value > 0 else default


def _truncate_text(text: str, max_chars: int) -> tuple[str, bool]:
    if len(text) <= max_chars:
        return text, False
    return text[:max_chars], True


def _fallback_order(scanner_cfg: Mapping[str, Any]) -> Sequence[str]:
    order = scanner_cfg.get("office_parser_fallback_order", _DEFAULT_FALLBACK_ORDER)
    if isinstance(order, str):
        return (order,)
    if isinstance(order, Sequence):
        return tuple(str(item) for item in order)
    return _DEFAULT_FALLBACK_ORDER
