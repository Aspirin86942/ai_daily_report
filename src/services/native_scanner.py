"""CPython 原生 scanner 的唯一 Python interface。"""

from __future__ import annotations

import importlib
import importlib.machinery
import importlib.util
import sys
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Any, Literal, Protocol

from ..models.scanner_contract import (
    ContextEnvelope,
    DoctorResponse,
    InspectRunResponseV2,
)

ReportMode = Literal["daily", "weekly", "monthly"]


@dataclass(frozen=True, slots=True)
class ScanRequest:
    report_mode: ReportMode
    start_date: date
    end_date: date
    compression_profile: str | None = None

    def to_native(self) -> dict[str, object]:
        if self.start_date > self.end_date:
            raise ValueError("start_date must be earlier than or equal to end_date")
        return {
            "report_mode": self.report_mode,
            "start_date": self.start_date.isoformat(),
            "end_date": self.end_date.isoformat(),
            "compression_profile": self.compression_profile,
        }


@dataclass(frozen=True, slots=True)
class ScanResult:
    envelope: ContextEnvelope
    evidence: InspectRunResponseV2 | None

    @classmethod
    def from_native(cls, value: object) -> "ScanResult":
        if not isinstance(value, dict):
            raise NativeScannerError(
                "NATIVE_RESULT_INVALID",
                "native scanner result must be a mapping",
                False,
            )
        envelope = ContextEnvelope.model_validate(value.get("envelope"))
        raw_evidence = value.get("evidence")
        evidence = (
            None
            if raw_evidence is None
            else InspectRunResponseV2.model_validate(raw_evidence)
        )
        if envelope.scan_run_id is not None:
            if evidence is None or evidence.scan_run_id != envelope.scan_run_id:
                raise NativeScannerError(
                    "NATIVE_EVIDENCE_INVALID",
                    "native result does not contain matching run evidence",
                    False,
                )
        return cls(envelope=envelope, evidence=evidence)


class NativeScannerError(RuntimeError):
    def __init__(self, error_code: str, message: str, retryable: bool) -> None:
        self.error_code = error_code
        self.message = message
        self.retryable = retryable
        super().__init__(f"{error_code}: {message}")


class _NativeScannerObject(Protocol):
    def build_context(self, request: dict[str, object]) -> object: ...

    def doctor(self) -> object: ...


class NativeScanner:
    """延迟加载 `.pyd`，隐藏 Rust 配置和跨语言类型转换。"""

    def __init__(
        self,
        runtime_config: Any,
        *,
        project_root: Path | None = None,
        index_db_path: str | Path | None = None,
        native: _NativeScannerObject | None = None,
    ) -> None:
        root = (project_root or Path(__file__).resolve().parents[2]).resolve()
        self._native = native
        self._native_config = {
            "work_dir": str(_resolve_path(root, runtime_config.work_dir)),
            "scan_db_path": str(
                _resolve_path(
                    root,
                    index_db_path or runtime_config.rust_index_db_path,
                )
            ),
            "scanner_profile": runtime_config.scanner_contract_profile(),
            "office_worker_path": str(
                _resolve_executable(
                    root,
                    runtime_config.rust_office_parser_bin,
                )
            ),
            "python_executable": str(Path(sys.executable).resolve()),
            "python_module_root": str(root),
            "python_document_worker_module": "src.workers.document_parser_worker",
        }

    def build_context(self, request: ScanRequest) -> ScanResult:
        try:
            value = self._native_object().build_context(request.to_native())
            return ScanResult.from_native(value)
        except NativeScannerError:
            raise
        except (TypeError, ValueError) as exc:
            raise ValueError(str(exc)) from exc
        except Exception as exc:
            raise _map_native_error(exc) from exc

    def doctor(self) -> DoctorResponse:
        try:
            return DoctorResponse.model_validate(self._native_object().doctor())
        except NativeScannerError:
            raise
        except Exception as exc:
            raise _map_native_error(exc) from exc

    @staticmethod
    def _load_native(config: dict[str, object]) -> _NativeScannerObject:
        try:
            module = importlib.import_module("ai_daily_scanner_native")
        except ModuleNotFoundError as exc:
            if exc.name != "ai_daily_scanner_native":
                raise
            module = _load_source_checkout_extension()
        return module.Scanner(config)

    def _native_object(self) -> _NativeScannerObject:
        if self._native is None:
            self._native = self._load_native(self._native_config)
        return self._native


def _load_source_checkout_extension() -> Any:
    """从 release build 加载开发态扩展；已安装 release 始终使用 wheel。"""
    extension_path = (
        Path(__file__).resolve().parents[2]
        / "rust"
        / "target"
        / "release"
        / "ai_daily_scanner_native.dll"
    )
    if sys.platform != "win32" or not extension_path.is_file():
        raise ModuleNotFoundError(
            "ai_daily_scanner_native is not installed and no source-checkout "
            "release extension is available"
        )
    loader = importlib.machinery.ExtensionFileLoader(
        "ai_daily_scanner_native",
        str(extension_path),
    )
    spec = importlib.util.spec_from_file_location(
        "ai_daily_scanner_native",
        extension_path,
        loader=loader,
    )
    if spec is None:
        raise ImportError("cannot create ai_daily_scanner_native module spec")
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    sys.modules["ai_daily_scanner_native"] = module
    return module


def _resolve_path(root: Path, value: str | Path) -> Path:
    path = Path(value)
    return (path if path.is_absolute() else root / path).resolve()


def _resolve_executable(root: Path, value: str | Path) -> Path:
    path = _resolve_path(root, value)
    if sys.platform == "win32" and path.suffix.lower() != ".exe":
        path = Path(f"{path}.exe")
    return path


def _map_native_error(exc: Exception) -> NativeScannerError:
    args = exc.args
    if (
        len(args) == 3
        and isinstance(args[0], str)
        and isinstance(args[1], str)
        and isinstance(args[2], bool)
    ):
        return NativeScannerError(args[0], args[1], args[2])
    return NativeScannerError("NATIVE_SCANNER_FAILED", str(exc), False)


__all__ = [
    "NativeScanner",
    "NativeScannerError",
    "ScanRequest",
    "ScanResult",
]
