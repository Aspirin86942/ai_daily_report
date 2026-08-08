"""`ai-daily-scanner` 的私有 Python 进程适配器。

分层：adapter —— Rust scanner 子进程适配。
"""

from __future__ import annotations

import hashlib
import os
import shutil
import sys
from functools import lru_cache
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable
from uuid import UUID, uuid4

from src.models.scanner_contract import (
    BuildContextRequest,
    ContextEnvelope,
    ContextSummary,
    Diagnostic,
    DoctorRequest,
    DoctorResponse,
    InspectRunRequest,
    InspectRunResponse,
    InspectRunResponseV2,
    UpgradeDatabaseRequestV1,
    UpgradeDatabaseResponseV1,
    VersionResponse,
    VersionResponseV2,
    build_rust_core_crashed_envelope,
)

from .json_process_client import JsonProcessResult, run_json_process

if TYPE_CHECKING:
    from .context_scheduler import ContextScheduleRequest


DEFAULT_SCANNER_BINARY = "rust/target/release/ai-daily-scanner"
DEFAULT_OFFICE_WORKER_BINARY = "rust/target/release/ai-daily-office-parser"
DEFAULT_SCAN_DB = "data/db/scan_index_v2.sqlite3"


class RustContextProbeError(RuntimeError):
    """A strict scanner probe failed without exposing subprocess output."""

    def __init__(self, operation: str, kind: str) -> None:
        self.operation = operation
        self.kind = kind
        super().__init__(f"Rust scanner {operation} probe failed ({kind})")


@lru_cache(maxsize=1)
def _default_python_worker_executable() -> Path:
    """Use the venv with a direct CPython image on Windows when safe."""
    configured = Path(sys.executable).resolve()
    if os.name != "nt" or sys.prefix == sys.base_prefix:
        return configured
    base_value = getattr(sys, "_base_executable", "")
    if not base_value:
        return configured
    return _materialize_windows_python_worker_executable(
        configured=configured,
        base=Path(base_value).resolve(),
        prefix=Path(sys.prefix).resolve(),
        version_tag=f"{sys.version_info.major}{sys.version_info.minor}",
    )


def _materialize_windows_python_worker_executable(
    *,
    configured: Path,
    base: Path,
    prefix: Path,
    version_tag: str,
) -> Path:
    """Create a content-addressed CPython copy without replacing venv files."""
    scripts_dir = configured.parent
    if (
        not base.is_file()
        or base.suffix.lower() != ".exe"
        or scripts_dir.parent != prefix
        or not (prefix / "pyvenv.cfg").is_file()
    ):
        return configured
    try:
        digest = _file_sha256(base)
        target = scripts_dir / (
            f"ai-daily-python-worker-{version_tag}-{digest[:16]}.exe"
        )
        if target.exists():
            return target if _same_executable_bytes(base, target, digest) else configured
        temporary = target.with_name(
            f".{target.name}.{os.getpid()}.{uuid4().hex}.tmp"
        )
        try:
            shutil.copyfile(base, temporary)
            if _file_sha256(temporary) != digest:
                return configured
            try:
                temporary.rename(target)
            except FileExistsError:
                pass
        finally:
            temporary.unlink(missing_ok=True)
        return target if _same_executable_bytes(base, target, digest) else configured
    except OSError:
        return configured


def _same_executable_bytes(base: Path, target: Path, base_digest: str) -> bool:
    """Accept only the managed byte-for-byte CPython copy; never overwrite drift."""
    if target.is_symlink() or not target.is_file():
        return False
    return (
        target.stat().st_size == base.stat().st_size
        and _file_sha256(target) == base_digest
    )


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(128 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class RustContextClient:
    """组装无敏感信息的 wire request，并执行一次深层 context 调用。"""

    def __init__(
        self,
        *,
        config: Any,
        project_root: Path | None = None,
        scanner_binary: str | Path = DEFAULT_SCANNER_BINARY,
        scan_db_path: str | Path = DEFAULT_SCAN_DB,
        office_worker_path: str | Path = DEFAULT_OFFICE_WORKER_BINARY,
        python_executable: str | Path | None = None,
        python_module_root: str | Path | None = None,
        python_document_worker_module: str = (
            "src.workers.document_parser_worker"
        ),
        timeout_seconds: float = 900,
        request_id_factory: Callable[[], UUID | str] = uuid4,
    ) -> None:
        self._config = config
        self._project_root = (
            project_root or Path(__file__).resolve().parents[2]
        ).resolve()
        self._scanner_binary = self._resolve_executable(scanner_binary)
        self._scan_db_path = self._resolve_path(scan_db_path)
        self._office_worker_path = self._resolve_executable(office_worker_path)
        self._python_executable = self._resolve_path(
            python_executable or _default_python_worker_executable()
        )
        self._python_module_root = self._resolve_path(
            python_module_root or self._project_root
        )
        self._python_document_worker_module = python_document_worker_module
        self._timeout_seconds = float(timeout_seconds)
        if self._timeout_seconds <= 0:
            raise ValueError("timeout_seconds must be positive")
        self._request_id_factory = request_id_factory

    def _adapter_payload(self) -> dict[str, str]:
        """Return the single adapter contract shared by doctor and runs."""
        return {
            "office_worker_path": str(self._office_worker_path),
            "python_executable": str(self._python_executable),
            "python_module_root": str(self._python_module_root),
            "python_document_worker_module": (
                self._python_document_worker_module
            ),
        }

    def version(self) -> VersionResponse:
        """Validate the requestless scanner identity contract (v1 projection)."""
        result = run_json_process(
            command=[str(self._scanner_binary), "version"],
            request_payload=None,
            response_model=VersionResponse,
            timeout_seconds=self._timeout_seconds,
            cwd=self._project_root,
        )
        if result.response is None:
            raise self._probe_error("version", result)
        return result.response

    def version_v2(self) -> VersionResponseV2:
        """`version --response-version 2`: strict v2 capabilities (spec Part 5.3)."""
        result = run_json_process(
            command=[str(self._scanner_binary), "version", "--response-version", "2"],
            request_payload=None,
            response_model=VersionResponseV2,
            timeout_seconds=self._timeout_seconds,
            cwd=self._project_root,
        )
        if result.response is None:
            raise self._probe_error("version --response-version 2", result)
        return result.response

    def doctor(self) -> DoctorResponse:
        """Validate the scan DB and the configured crash-isolated workers."""
        request_id = str(self._request_id_factory())
        request = DoctorRequest(
            contract="ai_daily_context",
            protocol_version=1,
            request_id=request_id,
            scan_db_path=str(self._scan_db_path),
            adapters=self._adapter_payload(),
        )
        result = run_json_process(
            command=[str(self._scanner_binary), "doctor"],
            request_payload=request.model_dump(mode="json"),
            response_model=DoctorResponse,
            timeout_seconds=self._timeout_seconds,
            expected_request_id=request_id,
            cwd=self._project_root,
        )
        if result.response is None:
            raise self._probe_error("doctor", result)
        return result.response

    def build_context(
        self,
        request: ContextScheduleRequest,
    ) -> ContextEnvelope:
        request_id = str(self._request_id_factory())
        wire_request = BuildContextRequest(
            contract="ai_daily_context",
            protocol_version=1,
            request_id=request_id,
            work_dir=str(self._resolve_path(self._config.work_dir)),
            start_date=request.start_date.isoformat(),
            end_date=request.end_date.isoformat(),
            report_mode=request.report_mode,
            compression_profile=request.compression_profile,
            scan_db_path=str(self._scan_db_path),
            scanner_profile=self._config.scanner_contract_profile(),
            adapters=self._adapter_payload(),
        )
        result = run_json_process(
            command=[str(self._scanner_binary), "build-context"],
            request_payload=wire_request.model_dump(
                mode="json",
                exclude_unset=True,
            ),
            response_model=ContextEnvelope,
            timeout_seconds=self._timeout_seconds,
            expected_request_id=request_id,
            cwd=self._project_root,
        )
        if result.response is not None:
            return result.response
        return self._crashed_envelope(request_id, result)

    def inspect_run(
        self,
        scan_run_id: int,
        *,
        include_content: bool = False,
    ) -> InspectRunResponse:
        """通过稳定 DTO 读取 Rust-owned run；不暴露或查询表结构。"""
        request_id = str(self._request_id_factory())
        request = InspectRunRequest(
            contract="ai_daily_context",
            protocol_version=1,
            request_id=request_id,
            scan_db_path=str(self._scan_db_path),
            scan_run_id=scan_run_id,
            include_content=include_content,
        )
        result = run_json_process(
            command=[str(self._scanner_binary), "inspect-run"],
            request_payload=request.model_dump(mode="json"),
            response_model=InspectRunResponse,
            timeout_seconds=self._timeout_seconds,
            expected_request_id=request_id,
            cwd=self._project_root,
        )
        if result.response is None:
            raise RuntimeError("Rust inspect-run did not return a trusted response")
        return result.response

    def inspect_run_v2(
        self,
        scan_run_id: int,
        *,
        include_content: bool = False,
    ) -> InspectRunResponseV2:
        """`inspect-run --response-version 2`: strict full-provenance v2 audit."""
        request_id = str(self._request_id_factory())
        request = InspectRunRequest(
            contract="ai_daily_context",
            protocol_version=1,
            request_id=request_id,
            scan_db_path=str(self._scan_db_path),
            scan_run_id=scan_run_id,
            include_content=include_content,
        )
        result = run_json_process(
            command=[
                str(self._scanner_binary),
                "inspect-run",
                "--response-version",
                "2",
            ],
            request_payload=request.model_dump(mode="json"),
            response_model=InspectRunResponseV2,
            timeout_seconds=self._timeout_seconds,
            expected_request_id=request_id,
            cwd=self._project_root,
        )
        if result.response is None:
            raise RuntimeError(
                "Rust inspect-run v2 did not return a trusted response"
            )
        return result.response

    def upgrade_database(
        self,
        request: UpgradeDatabaseRequestV1,
    ) -> UpgradeDatabaseResponseV1:
        """执行 `upgrade-db`（audit/apply）；不内置备份，回滚由运维承担。"""
        result = run_json_process(
            command=[str(self._scanner_binary), "upgrade-db"],
            request_payload=request.model_dump(mode="json"),
            response_model=UpgradeDatabaseResponseV1,
            timeout_seconds=self._timeout_seconds,
            expected_request_id=request.request_id,
            cwd=self._project_root,
        )
        if result.response is None:
            raise self._probe_error("upgrade-db", result)
        return result.response

    def _resolve_path(self, value: str | Path) -> Path:
        path = Path(value)
        if not path.is_absolute():
            path = self._project_root / path
        return path.resolve()

    def _resolve_executable(self, value: str | Path) -> Path:
        path = self._resolve_path(value)
        if os.name == "nt" and path.suffix.lower() != ".exe":
            path = Path(f"{path}.exe")
        return path

    @staticmethod
    def _probe_error(
        operation: str,
        result: JsonProcessResult[Any],
    ) -> RustContextProbeError:
        if result.failure is not None:
            kind = result.failure.kind
        elif result.transport_error is not None:
            kind = "transport_error"
        else:
            kind = "missing_response"
        return RustContextProbeError(operation, kind)

    @staticmethod
    def _crashed_envelope(
        request_id: str,
        result: JsonProcessResult[Any],
    ) -> ContextEnvelope:
        if result.transport_error is None:
            return build_rust_core_crashed_envelope(
                request_id=request_id,
                duration_ms=result.duration_ms,
            )
        diagnostic = result.transport_error.error
        return ContextEnvelope(
            contract="ai_daily_context",
            protocol_version=1,
            request_id=request_id,
            engine_version="unknown",
            engine_build="unknown",
            status="error",
            file_context="",
            summary=ContextSummary(
                source_file_count=0,
                success_count=0,
                timeout_count=0,
                included_file_count=0,
                omitted_file_count=0,
                error_file_count=0,
                input_chars=0,
                output_chars=0,
                total_duration_ms=result.duration_ms,
                discovery_duration_ms=0,
                parse_duration_ms=0,
                compression_duration_ms=0,
            ),
            scan_run_id=None,
            context_run_id=None,
            warnings=[],
            error=diagnostic,
        )
