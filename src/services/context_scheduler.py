"""CLI 生命周期内的文件上下文调度服务。"""

from __future__ import annotations

import json
from dataclasses import dataclass, replace
from datetime import date
from pathlib import Path
from time import perf_counter
from typing import Callable, Protocol

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

_SUMMARY_REPORT_MODES = {"weekly", "monthly"}
_SUPPORTED_REPORT_MODES = {"daily", "weekly", "monthly"}
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


class _ContextStore(Protocol):
    def latest_scan_run_detail(self) -> dict[str, int]:
        ...

    def save_context_run(
        self,
        *,
        report_mode: str,
        start_date: date,
        end_date: date,
        compression_profile: str,
        context_profile_key: str,
        scan_run_id: int | None,
        source_file_count: int,
        included_file_count: int,
        omitted_file_count: int,
        metadata_only_count: int,
        compressed_file_count: int,
        error_file_count: int,
        truncated_file_count: int,
        input_chars: int,
        output_chars: int,
        duration_ms: int,
        status: str = "success",
        error: str = "",
    ) -> int:
        ...

    def save_context_decisions(
        self,
        context_run_id: int,
        decisions: list[ContextDecision],
    ) -> None:
        ...


@dataclass(frozen=True, slots=True)
class ContextScheduleRequest:
    report_mode: str
    source: str
    start_date: date
    end_date: date
    compression_profile: str | None = None
    user_input: str | None = None


@dataclass(slots=True)
class ContextScheduleResult:
    file_context: str
    compressed_context: CompressedContext
    scan_result: ScanResult | None
    context_run_id: int | None
    decisions: list[ContextDecision]
    error: str | None = None


class ContextScheduler:
    """编排一次 CLI run 内的文件上下文构建与审计落库。"""

    def __init__(
        self,
        scanner_factory: Callable[[], FileScanner] | None = None,
        compressor: ContextCompressor | None = None,
    ) -> None:
        # Scheduler 不是后台 daemon，只负责本次 CLI 生命周期内的策略编排；
        # scanner/compressor 通过依赖注入接入，便于测试真实调度边界。
        self._scanner_factory = scanner_factory or FileScanner
        self._compressor = compressor or ContextCompressor()

    def build_context(self, request: ContextScheduleRequest) -> ContextScheduleResult:
        """构建文件上下文，并把 run 级与逐文件决策审计写入 scanner store。"""
        self._validate_request(request)

        started_at = perf_counter()
        profile = self._build_profile(request)
        profile_key = self._serialize_profile(profile)
        scan_result: ScanResult | None = None
        decisions: list[ContextDecision] = []
        context_run_id: int | None = None
        store: _ContextStore | None = None

        try:
            scanner = self._scanner_factory()
            store = scanner.scan_index_store
            scan_result = scanner.scan_files(
                start_date=request.start_date,
                end_date=request.end_date,
                summary_mode=profile.report_mode in _SUMMARY_REPORT_MODES,
            )
            decisions = self._build_decisions(scan_result, profile)
            compressed = self._compressor.compress(
                scan_result=scan_result,
                decisions=decisions,
                profile=profile,
            )
            duration_ms = self._duration_ms(started_at)
            context_run_id = self._save_context_run(
                store=store,
                request=request,
                profile=profile,
                profile_key=profile_key,
                scan_run_id=self._latest_scan_run_id(store),
                compressed=compressed,
                duration_ms=duration_ms,
                status="success",
                error="",
            )
            store.save_context_decisions(context_run_id, compressed.decisions)
            return ContextScheduleResult(
                file_context=compressed.content,
                compressed_context=compressed,
                scan_result=scan_result,
                context_run_id=context_run_id,
                decisions=compressed.decisions,
            )
        except Exception as exc:
            error_text = str(exc) or exc.__class__.__name__
            fallback = CompressedContext.empty(
                error=f"文件上下文构建失败: {error_text}"
            )
            duration_ms = self._duration_ms(started_at)
            # 异常 fallback 仍尽量落 run 级审计，因为用户需要知道本次报告为什么缺少文件上下文。
            if store is not None:
                context_run_id = self._try_save_error_run(
                    store=store,
                    request=request,
                    profile=profile,
                    profile_key=profile_key,
                    scan_result=scan_result,
                    decisions=decisions,
                    fallback=fallback,
                    duration_ms=duration_ms,
                    error_text=error_text,
                )

            return ContextScheduleResult(
                file_context=fallback.content,
                compressed_context=fallback,
                scan_result=scan_result,
                context_run_id=context_run_id,
                decisions=decisions,
                error=error_text,
            )

    def _validate_request(self, request: ContextScheduleRequest) -> None:
        source = request.source.strip().lower()
        report_mode = request.report_mode.strip().lower()
        if source != "scan":
            raise ValueError(f"unsupported context source: {request.source!r}")
        if report_mode not in _SUPPORTED_REPORT_MODES:
            raise ValueError(f"unsupported report_mode: {request.report_mode!r}")
        if request.start_date > request.end_date:
            raise ValueError("start_date must be earlier than or equal to end_date")

    def _build_profile(self, request: ContextScheduleRequest) -> ContextProfile:
        profile = ContextProfile.for_report_mode(request.report_mode)
        if request.compression_profile and request.compression_profile.strip():
            return replace(
                profile,
                compression_profile=request.compression_profile.strip(),
            )
        return profile

    def _serialize_profile(self, profile: ContextProfile) -> str:
        # profile key 使用排序序列化，后续 benchmark/cache 比较时不会受 dict 顺序影响。
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
            action = ACTION_ERROR
            reason = "parse_error"
        elif size_bytes > profile.large_file_max_bytes:
            action = ACTION_METADATA_ONLY
            reason = "file_size_policy"
        elif input_chars <= profile.per_file_max_chars and not context.truncated:
            action = ACTION_KEEP
            reason = "small_file_keep"
        else:
            action = ACTION_COMPRESS
            reason = self._compression_reason(extension)

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

    def _priority_for_context(
        self,
        *,
        file_path: str,
        extension: str,
        has_error: bool,
    ) -> int:
        path_key = _normalized_path_key(file_path)
        if has_error:
            return 80
        if "\\.pytest_cache\\" in path_key or "\\data\\benchmarks\\" in path_key:
            return 70
        if "\\logs\\" in path_key or path_key.startswith("logs\\"):
            return 60
        if extension in _OFFICE_OR_PDF_EXTENSIONS:
            return 20
        if extension in _TEXT_KEEP_EXTENSIONS:
            return 30
        return 50

    def _compression_reason(self, extension: str) -> str:
        if extension == ".log":
            return "large_log_tail"
        if extension in _OFFICE_OR_PDF_EXTENSIONS:
            return "large_document_summary"
        return "medium_text_compress"

    def _file_size(self, file_path: str) -> int:
        try:
            return Path(file_path).stat().st_size
        except OSError:
            return 0

    def _latest_scan_run_id(self, store: _ContextStore) -> int | None:
        try:
            detail = store.latest_scan_run_detail()
            run_id = detail.get("run_id")
        except Exception:
            return None
        return None if run_id is None else int(run_id)

    def _save_context_run(
        self,
        *,
        store: _ContextStore,
        request: ContextScheduleRequest,
        profile: ContextProfile,
        profile_key: str,
        scan_run_id: int | None,
        compressed: CompressedContext,
        duration_ms: int,
        status: str,
        error: str,
    ) -> int:
        return store.save_context_run(
            report_mode=profile.report_mode,
            start_date=request.start_date,
            end_date=request.end_date,
            compression_profile=profile.compression_profile,
            context_profile_key=profile_key,
            scan_run_id=scan_run_id,
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
            status=status,
            error=error,
        )

    def _try_save_error_run(
        self,
        *,
        store: _ContextStore,
        request: ContextScheduleRequest,
        profile: ContextProfile,
        profile_key: str,
        scan_result: ScanResult | None,
        decisions: list[ContextDecision],
        fallback: CompressedContext,
        duration_ms: int,
        error_text: str,
    ) -> int | None:
        fallback_for_audit = self._fallback_with_scan_stats(
            fallback=fallback,
            scan_result=scan_result,
        )
        try:
            context_run_id = self._save_context_run(
                store=store,
                request=request,
                profile=profile,
                profile_key=profile_key,
                scan_run_id=self._latest_scan_run_id(store),
                compressed=fallback_for_audit,
                duration_ms=duration_ms,
                status="error",
                error=error_text,
            )
        except Exception:
            return None

        if decisions:
            try:
                store.save_context_decisions(context_run_id, decisions)
            except Exception:
                return context_run_id
        return context_run_id

    def _fallback_with_scan_stats(
        self,
        *,
        fallback: CompressedContext,
        scan_result: ScanResult | None,
    ) -> CompressedContext:
        if scan_result is None:
            return fallback
        return replace(
            fallback,
            source_file_count=scan_result.total_files,
            error_file_count=max(fallback.error_file_count, scan_result.error_count),
            input_chars=sum(
                len(context.content or "")
                for context in scan_result.contexts
            ),
        )

    def _duration_ms(self, started_at: float) -> int:
        return max(0, int(round((perf_counter() - started_at) * 1000)))


def _normalized_path_key(file_path: str) -> str:
    return "\\" + file_path.lower().replace("/", "\\").strip("\\")


__all__ = [
    "ContextScheduleRequest",
    "ContextScheduleResult",
    "ContextScheduler",
]
