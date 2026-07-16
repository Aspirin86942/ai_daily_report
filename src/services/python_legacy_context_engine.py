"""Task 9 期间冻结的 Python legacy context engine adapter。"""

from __future__ import annotations

from collections.abc import Mapping
import json
from dataclasses import replace
from pathlib import Path
from time import perf_counter
from typing import TYPE_CHECKING, Any, Callable
from uuid import UUID, uuid4

from ..models.scanner_contract import (
    ContextEnvelope,
    ContextSummary,
    Diagnostic,
)
from ..models.schemas import FileContext, ScanResult
from .context_compressor import (
    ACTION_COMPRESS,
    ACTION_ERROR,
    ACTION_KEEP,
    ACTION_METADATA_ONLY,
    CompressedContext,
    ContextCompressor,
    ContextDecision,
    ContextProfile,
)
from .file_scanner import FileScanner
from .scan_metrics import is_timeout_error

if TYPE_CHECKING:
    from .context_scheduler import ContextScheduleRequest


_SUMMARY_REPORT_MODES = {"weekly", "monthly"}
_OFFICE_OR_PDF_EXTENSIONS = {
    ".doc",
    ".docx",
    ".pdf",
    ".ppt",
    ".pptx",
    ".xls",
    ".xlsm",
    ".xlsx",
}
_TEXT_KEEP_EXTENSIONS = {".md", ".txt"}
_ENGINE_VERSION = "python-legacy-v1"
_ENGINE_BUILD = "python-legacy-frozen-task9"
_AUDIT_ERROR_CODE = "PYTHON_LEGACY_CONTEXT_FAILED"


class PythonLegacyContextEngine:
    """把原 ContextScheduler 的完整 Python pipeline 封装成临时 adapter。"""

    def __init__(
        self,
        *,
        scanner_factory: Callable[[], FileScanner] | None = None,
        compressor: ContextCompressor | None = None,
        request_id_factory: Callable[[], UUID | str] = uuid4,
    ) -> None:
        self._scanner_factory = scanner_factory or FileScanner
        self._compressor = compressor or ContextCompressor()
        self._request_id_factory = request_id_factory

    def build_context(self, request: ContextScheduleRequest) -> ContextEnvelope:
        """运行一次冻结 legacy pipeline，并收敛成严格 ContextEnvelope。"""
        request_id = str(self._request_id_factory())
        started_at = perf_counter()
        profile = self._build_profile(request)
        profile_key = self._serialize_profile(profile)
        scan_result: ScanResult | None = None
        decisions: list[ContextDecision] = []
        compressed: CompressedContext | None = None
        context_run_id: int | None = None
        store: Any | None = None
        scan_metrics: dict[str, int] | None = None

        try:
            scanner = self._scanner_factory()
            store = scanner.scan_index_store
            scan_result = scanner.scan_files(
                start_date=request.start_date,
                end_date=request.end_date,
                summary_mode=profile.report_mode in _SUMMARY_REPORT_MODES,
            )
            if scan_result.scan_run_id is None:
                raise RuntimeError("legacy scanner did not return scan_run_id")
            scan_metrics = self._load_scan_metrics(
                store,
                scan_result.scan_run_id,
            )
            decisions = self._build_decisions(scan_result, profile)
            compressed = self._compressor.compress(
                scan_result=scan_result,
                decisions=decisions,
                profile=profile,
            )
            duration_ms = self._duration_ms(started_at)
            context_run_id = store.save_context_run_with_decisions(
                report_mode=profile.report_mode,
                start_date=request.start_date,
                end_date=request.end_date,
                compression_profile=profile.compression_profile,
                context_profile_key=profile_key,
                scan_run_id=scan_result.scan_run_id,
                source_file_count=compressed.source_file_count,
                included_file_count=compressed.included_file_count,
                omitted_file_count=compressed.omitted_file_count,
                metadata_only_count=compressed.metadata_only_count,
                compressed_file_count=compressed.compressed_file_count,
                error_file_count=compressed.error_file_count,
                truncated_file_count=compressed.truncated_file_count,
                input_chars=compressed.input_chars,
                output_chars=compressed.output_chars,
                duration_ms=duration_ms,
                status="success",
                error="",
                decisions=compressed.decisions,
            )
            warnings = self._build_warnings(scan_result, compressed)
            has_incomplete_file = scan_result.error_count > 0 or any(
                context.error for context in scan_result.contexts
            )
            status = "partial" if has_incomplete_file else "ok"
            return ContextEnvelope(
                contract="ai_daily_context",
                protocol_version=1,
                request_id=request_id,
                engine_version=_ENGINE_VERSION,
                engine_build=_ENGINE_BUILD,
                status=status,
                file_context=compressed.content,
                summary=self._summary(
                    scan_result=scan_result,
                    compressed=compressed,
                    duration_ms=duration_ms,
                    scan_metrics=scan_metrics,
                ),
                scan_run_id=scan_result.scan_run_id,
                context_run_id=context_run_id,
                warnings=warnings,
                error=None,
            )
        except Exception:
            failure_message = "文件上下文构建失败"
            if compressed is None:
                fallback = CompressedContext.empty(error=failure_message)
                audit_decisions = decisions
            else:
                fallback = self._compressed_error_fallback(
                    compressed,
                    failure_message,
                )
                audit_decisions = fallback.decisions
            duration_ms = self._duration_ms(started_at)
            audit_failed = False
            if store is not None:
                context_run_id, audit_failed = self._try_save_error_run(
                    store=store,
                    request=request,
                    profile=profile,
                    profile_key=profile_key,
                    scan_result=scan_result,
                    decisions=audit_decisions,
                    fallback=fallback,
                    duration_ms=duration_ms,
                )
            scan_run_id = None if scan_result is None else scan_result.scan_run_id
            return ContextEnvelope(
                contract="ai_daily_context",
                protocol_version=1,
                request_id=request_id,
                engine_version=_ENGINE_VERSION,
                engine_build=_ENGINE_BUILD,
                status="error",
                file_context="",
                summary=self._summary(
                    scan_result=scan_result,
                    compressed=fallback,
                    duration_ms=duration_ms,
                    scan_metrics=scan_metrics,
                ),
                scan_run_id=scan_run_id,
                context_run_id=context_run_id,
                warnings=(
                    [
                        Diagnostic(
                            error_code="INTERNAL_ERROR",
                            message="Python legacy audit persistence failed",
                            retryable=False,
                            stage="context",
                            file_path=None,
                            backend=None,
                        )
                    ]
                    if audit_failed
                    else []
                ),
                error=Diagnostic(
                    error_code="INTERNAL_ERROR",
                    message="Python legacy context engine failed",
                    retryable=False,
                    stage="context",
                    file_path=None,
                    backend=None,
                ),
            )

    @staticmethod
    def _build_profile(request: ContextScheduleRequest) -> ContextProfile:
        profile = ContextProfile.for_report_mode(request.report_mode)
        if request.compression_profile and request.compression_profile.strip():
            return replace(
                profile,
                compression_profile=request.compression_profile.strip(),
            )
        return profile

    @staticmethod
    def _serialize_profile(profile: ContextProfile) -> str:
        return json.dumps(
            profile.to_profile_dict(),
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        )

    def _build_decisions(
        self,
        scan_result: ScanResult,
        profile: ContextProfile,
    ) -> list[ContextDecision]:
        decisions = [
            self._build_decision(context, profile)
            for context in scan_result.contexts
        ]
        return sorted(
            decisions,
            key=lambda item: (item.priority, item.file_path.lower()),
        )

    def _build_decision(
        self,
        context: FileContext,
        profile: ContextProfile,
    ) -> ContextDecision:
        extension = (context.file_type or Path(context.file_path).suffix).lower()
        size_bytes = self._file_size(context.file_path)
        input_chars = len(context.content or "")
        priority = self._priority_for_context(
            file_path=context.file_path,
            extension=extension,
            has_error=bool(context.error),
        )
        if context.error:
            action, reason = ACTION_ERROR, "parse_error"
        elif size_bytes > profile.large_file_max_bytes:
            action, reason = ACTION_METADATA_ONLY, "file_size_policy"
        elif input_chars <= profile.per_file_max_chars and not context.truncated:
            action, reason = ACTION_KEEP, "small_file_keep"
        else:
            action, reason = ACTION_COMPRESS, self._compression_reason(extension)
        return ContextDecision(
            file_path=context.file_path,
            extension=extension,
            size_bytes=size_bytes,
            parser_backend=context.parser_backend,
            worker_lane="unknown",
            cache_status="unknown",
            action=action,
            reason=reason,
            priority=priority,
            input_chars=input_chars,
            output_chars=0,
            truncated=context.truncated,
            error=context.error,
        )

    @staticmethod
    def _priority_for_context(
        *,
        file_path: str,
        extension: str,
        has_error: bool,
    ) -> int:
        path_key = "\\" + file_path.lower().replace("/", "\\").strip("\\")
        if has_error:
            return 80
        if "\\.pytest_cache\\" in path_key or "\\data\\benchmarks\\" in path_key:
            return 70
        if "\\logs\\" in path_key:
            return 60
        if extension in _OFFICE_OR_PDF_EXTENSIONS:
            return 20
        if extension in _TEXT_KEEP_EXTENSIONS:
            return 30
        return 50

    @staticmethod
    def _compression_reason(extension: str) -> str:
        if extension == ".log":
            return "large_log_tail"
        if extension in _OFFICE_OR_PDF_EXTENSIONS:
            return "large_document_summary"
        return "medium_text_compress"

    @staticmethod
    def _file_size(file_path: str) -> int:
        try:
            return Path(file_path).stat().st_size
        except OSError:
            return 0

    def _try_save_error_run(
        self,
        *,
        store: Any,
        request: ContextScheduleRequest,
        profile: ContextProfile,
        profile_key: str,
        scan_result: ScanResult | None,
        decisions: list[ContextDecision],
        fallback: CompressedContext,
        duration_ms: int,
    ) -> tuple[int | None, bool]:
        fallback = self._fallback_with_scan_stats(fallback, scan_result)
        run_payload = {
            "report_mode": profile.report_mode,
            "start_date": request.start_date,
            "end_date": request.end_date,
            "compression_profile": profile.compression_profile,
            "context_profile_key": profile_key,
            "scan_run_id": (
                None if scan_result is None else scan_result.scan_run_id
            ),
            "source_file_count": fallback.source_file_count,
            "included_file_count": fallback.included_file_count,
            "omitted_file_count": fallback.omitted_file_count,
            "metadata_only_count": fallback.metadata_only_count,
            "compressed_file_count": fallback.compressed_file_count,
            "error_file_count": fallback.error_file_count,
            "truncated_file_count": fallback.truncated_file_count,
            "input_chars": fallback.input_chars,
            "output_chars": fallback.output_chars,
            "duration_ms": duration_ms,
            "status": "error",
            "error": _AUDIT_ERROR_CODE,
        }
        try:
            return (
                store.save_context_run_with_decisions(
                    **run_payload,
                    decisions=decisions,
                ),
                False,
            )
        except Exception:
            try:
                context_run_id = store.save_context_run(**run_payload)
            except Exception:
                return None, True
        if decisions:
            try:
                store.save_context_decisions(context_run_id, decisions)
            except Exception:
                return None, True
        return context_run_id, False

    @staticmethod
    def _compressed_error_fallback(
        compressed: CompressedContext,
        failure_message: str,
    ) -> CompressedContext:
        warning_lines = list(compressed.warnings)
        if failure_message not in warning_lines:
            warning_lines.append(failure_message)
        failure_section = "\n".join(
            [
                "## 上下文构建异常",
                f"- {failure_message}",
                "- 已保留压缩统计和逐文件决策用于本地审计。",
            ]
        )
        content = compressed.content.rstrip() + "\n\n" + failure_section + "\n"
        return replace(
            compressed,
            content=content,
            output_chars=len(content),
            warnings=warning_lines,
            decisions=compressed.decisions,
        )

    @staticmethod
    def _fallback_with_scan_stats(
        fallback: CompressedContext,
        scan_result: ScanResult | None,
    ) -> CompressedContext:
        if scan_result is None:
            return replace(fallback, error_file_count=0)
        return replace(
            fallback,
            source_file_count=scan_result.total_files,
            error_file_count=max(fallback.error_file_count, scan_result.error_count),
            input_chars=sum(len(context.content or "") for context in scan_result.contexts),
        )

    @staticmethod
    def _build_warnings(
        scan_result: ScanResult,
        compressed: CompressedContext,
    ) -> list[Diagnostic]:
        warnings: list[Diagnostic] = []
        error_context_count = sum(bool(context.error) for context in scan_result.contexts)
        if scan_result.error_count > 0:
            warnings.append(
                Diagnostic(
                    error_code="PARSER_FAILED",
                    message=(
                        "Python legacy scanner reported "
                        f"{scan_result.error_count} incomplete file(s)"
                    ),
                    retryable=False,
                    stage="parse",
                    file_path=None,
                    backend=None,
                )
            )
        if compressed.warnings:
            warnings.append(
                Diagnostic(
                    error_code="INTERNAL_ERROR",
                    message=(
                        "Python legacy compressor reported "
                        f"{len(compressed.warnings)} operational warning(s)"
                    ),
                    retryable=False,
                    stage="context",
                    file_path=None,
                    backend=None,
                )
            )
        if error_context_count > scan_result.error_count and not warnings:
            warnings.append(
                Diagnostic(
                    error_code="PARSER_FAILED",
                    message="Python legacy scanner returned incomplete file evidence",
                    retryable=False,
                    stage="parse",
                    file_path=None,
                    backend=None,
                )
            )
        return warnings

    @staticmethod
    def _summary(
        *,
        scan_result: ScanResult | None,
        compressed: CompressedContext,
        duration_ms: int,
        scan_metrics: dict[str, int] | None,
    ) -> ContextSummary:
        if scan_result is None:
            source_count = 0
            success_count = 0
            timeout_count = 0
            error_count = 0
        else:
            observed_error_count = sum(
                bool(context.error) for context in scan_result.contexts
            )
            observed_timeout_count = sum(
                is_timeout_error(context.error)
                for context in scan_result.contexts
            )
            observed_success_count = len(scan_result.contexts) - observed_error_count
            source_count = max(
                0,
                scan_result.total_files,
                len(scan_result.contexts),
                compressed.source_file_count,
            )
            classified_error_count = max(
                0,
                min(
                    max(
                        scan_result.error_count,
                        observed_error_count,
                        compressed.error_file_count,
                    ),
                    source_count,
                ),
            )
            timeout_count = min(
                classified_error_count,
                max(
                    observed_timeout_count,
                    0
                    if scan_metrics is None
                    else int(scan_metrics.get("timeout_count", 0)),
                ),
            )
            error_count = classified_error_count - timeout_count
            success_count = max(
                0,
                min(
                    max(scan_result.success_count, observed_success_count),
                    source_count - error_count - timeout_count,
                ),
            )
        discovery_duration_ms = (
            0
            if scan_metrics is None
            else max(0, int(scan_metrics.get("discovery_duration_ms", 0)))
        )
        parse_duration_ms = (
            0
            if scan_metrics is None
            else max(0, int(scan_metrics.get("parse_duration_ms", 0)))
        )
        scan_duration_ms = (
            0
            if scan_metrics is None
            else max(0, int(scan_metrics.get("total_duration_ms", 0)))
        )
        total_duration_ms = max(duration_ms, scan_duration_ms)
        return ContextSummary(
            source_file_count=source_count,
            success_count=success_count,
            timeout_count=timeout_count,
            included_file_count=max(0, compressed.included_file_count),
            omitted_file_count=max(0, compressed.omitted_file_count),
            error_file_count=error_count,
            input_chars=max(0, compressed.input_chars),
            output_chars=max(0, compressed.output_chars),
            total_duration_ms=total_duration_ms,
            discovery_duration_ms=discovery_duration_ms,
            parse_duration_ms=parse_duration_ms,
            compression_duration_ms=max(0, total_duration_ms - scan_duration_ms),
        )

    @staticmethod
    def _load_scan_metrics(store: Any, scan_run_id: int) -> dict[str, int] | None:
        loader = getattr(store, "get_scan_run_detail", None)
        if not callable(loader):
            return None
        raw = loader(scan_run_id)
        if raw is None:
            return None
        if not isinstance(raw, Mapping):
            raise TypeError("legacy scan metrics must be a mapping")
        return {str(key): int(value) for key, value in raw.items()}

    @staticmethod
    def _duration_ms(started_at: float) -> int:
        return max(0, int(round((perf_counter() - started_at) * 1000)))


__all__ = ["PythonLegacyContextEngine"]
