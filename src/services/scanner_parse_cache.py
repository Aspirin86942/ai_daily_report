"""Scanner parse-cache and reparse audit helpers."""

from __future__ import annotations

from collections.abc import Callable, Mapping
from typing import Any

from ..models.schemas import FileContext
from .office_parser import OfficeParseAudit
from .scan_index_store import CacheProbe
from .scan_metrics import ReparseDetail
from .scanner_items import (
    ScannerItem,
    item_extension,
    item_identity,
    item_path,
    item_source_version,
)

InferWorkerLane = Callable[[str, FileContext], str]


def get_cached_contexts(
    scan_index_store: Any,
    cached_files: list[ScannerItem],
    parser_profile: str,
) -> list[FileContext]:
    """从 parse_cache 恢复 fresh cache 命中的上下文。"""
    contexts: list[FileContext] = []
    for item in cached_files:
        cached = scan_index_store.load_parse_cache(
            item_identity(item),
            parser_profile,
            source_version=item_source_version(item),
        )
        parse_status = cached["parse_status"]
        parse_error = cached["parse_error"] or None
        contexts.append(
            FileContext(
                file_path=str(item_path(item)),
                file_type=item_extension(item),
                content=cached["content_excerpt"],
                error=parse_error if parse_status != "success" else None,
                parser_backend=cached["parser_backend"] or None,
                truncated=bool(cached["truncated"]),
            )
        )
    return contexts


def write_parse_cache(
    scan_index_store: Any,
    item: ScannerItem,
    parser_profile: str,
    context: FileContext,
) -> None:
    """把本轮解析结果写回 parse_cache。"""
    is_success = context.error is None
    scan_index_store.upsert_parse_cache(
        file_identity=item_identity(item),
        parser_profile=parser_profile,
        content_excerpt=context.content if is_success else "",
        parse_status="success" if is_success else "error",
        parse_error=context.error or "",
        source_version=item_source_version(item),
        parser_backend=context.parser_backend or "",
        truncated=context.truncated,
    )


def build_reparse_detail(
    *,
    item: ScannerItem,
    cache_probe: CacheProbe,
    duration_ms: int,
    context: FileContext,
    office_parse_audits: Mapping[str, OfficeParseAudit],
    infer_worker_lane: InferWorkerLane,
) -> ReparseDetail:
    """构建单个重解析文件的 cache miss 原因和解析结果。"""
    path = item_path(item)
    extension = item_extension(item)
    office_audit = office_parse_audits.get(str(path))
    return ReparseDetail(
        path=str(path),
        extension=extension,
        file_identity=item_identity(item),
        source_version=item_source_version(item),
        cache_status=cache_probe.cache_status,
        cache_miss_reason=cache_probe.cache_miss_reason,
        previous_source_version=cache_probe.previous_source_version,
        parse_duration_ms=duration_ms,
        parse_status="error" if context.error else "success",
        parse_error=context.error or "",
        parser_backend=context.parser_backend or "subprocess",
        worker_lane=infer_worker_lane(extension, context),
        truncated=context.truncated,
        attempted_backend=office_audit.attempted_backend
        if office_audit is not None
        else "",
        fallback_backend=office_audit.fallback_backend
        if office_audit is not None
        else "",
        fallback_reason=office_audit.fallback_reason
        if office_audit is not None
        else "",
        rust_duration_ms=office_audit.rust_duration_ms
        if office_audit is not None
        else 0,
        fallback_duration_ms=office_audit.fallback_duration_ms
        if office_audit is not None
        else 0,
        failure_class=office_audit.failure_class if office_audit is not None else "",
    )


def build_reparse_exception_detail(
    *,
    item: ScannerItem,
    cache_probe: CacheProbe,
    parse_error: str,
    not_parsed_backend: str,
) -> ReparseDetail:
    """解析入口抛异常时，也要留下 benchmark 可见的重解析明细。"""
    path = item_path(item)
    return ReparseDetail(
        path=str(path),
        extension=item_extension(item),
        file_identity=item_identity(item),
        source_version=item_source_version(item),
        cache_status=cache_probe.cache_status,
        cache_miss_reason=cache_probe.cache_miss_reason,
        previous_source_version=cache_probe.previous_source_version,
        parse_duration_ms=0,
        parse_status="error",
        parse_error=parse_error,
        parser_backend=not_parsed_backend,
        worker_lane="not_parsed",
        truncated=False,
    )
