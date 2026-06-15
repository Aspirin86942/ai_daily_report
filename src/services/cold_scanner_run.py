"""Cold scanner run lifecycle orchestration."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import date, timedelta
from pathlib import Path
from typing import Any

from ..core.logger import setup_logger
from ..models.schemas import FileContext, ScanResult
from .scan_aggregator import ScanAggregator
from .scan_metrics import ScanMetricsCollector

logger = setup_logger()


class ColdScannerRun:
    """Run-level Module that owns scanner lifecycle ordering."""

    def __init__(self, scanner: Any) -> None:
        self.scanner = scanner

    def scan_files(
        self,
        start_date: date | None = None,
        end_date: date | None = None,
        summary_mode: bool = False,
    ) -> ScanResult:
        """Execute one scanner run while preserving FileScanner's parser behavior."""
        if start_date is None:
            start_date = date.today() - timedelta(days=1)
        if end_date is None:
            end_date = date.today()

        logger.info(
            "开始扫描工作目录: %s (%s ~ %s, summary=%s)",
            self.scanner.work_dir,
            start_date,
            end_date,
            summary_mode,
        )

        metrics = ScanMetricsCollector.start()
        self.scanner.last_reparse_details = []
        self.scanner._office_parse_audits = {}

        with metrics.measure_stage("discovery"):
            discovered_files = self.scanner._normalize_discovered_files(
                self.scanner.discovery_service.bootstrap_full_scan(
                    start_date,
                    end_date,
                )
            )
        logger.info("发现 %s 个文件", len(discovered_files))
        metrics.set_discovered_count(len(discovered_files))

        if not discovered_files:
            return self._finish_empty_run(metrics)

        with metrics.measure_stage("inventory_cache"):
            parser_profile = self.scanner.scan_planner.build_parser_profile(
                summary_mode=summary_mode
            )
            parser_profile_key = self.scanner.scan_planner.serialize_parser_profile(
                parser_profile
            )
            self.scanner.scan_index_store.replace_inventory(
                [
                    {
                        "file_identity": item.file_identity,
                        "path": str(item.path),
                        "extension": item.extension,
                        "modified_date": item.modified_at.date().isoformat(),
                        "size_bytes": item.size_bytes,
                        "source_version": item.source_version,
                    }
                    for item in discovered_files
                ]
            )
            inventory_items = self.scanner.scan_index_store.query_inventory(
                start_date,
                end_date,
            )
            cache_probes = {
                item.file_identity: self.scanner.scan_index_store.probe_parse_cache(
                    item.file_identity,
                    parser_profile_key,
                    source_version=item.source_version,
                )
                for item in inventory_items
            }
            cache_lookup = {
                file_identity: probe.cache_status == "fresh"
                for file_identity, probe in cache_probes.items()
            }
            planned_candidates = self.scanner.scan_planner.plan_candidates(
                candidates=inventory_items,
                start_date=start_date,
                end_date=end_date,
                cache_lookup=cache_lookup,
            )
            metrics.set_plan_counts(
                reused_count=len(planned_candidates["cached"]),
                reparsed_count=len(planned_candidates["uncached"]),
            )
            limits = {
                key: value
                for key, value in parser_profile.items()
                if key not in {"total_max_chars", "summary_mode"}
            }
            cached_contexts = self.scanner._get_cached_contexts(
                planned_candidates["cached"],
                parser_profile_key,
            )
            cached_contexts_by_path = {
                Path(context.file_path): context for context in cached_contexts
            }

        aggregator = ScanAggregator(parser_profile["total_max_chars"])
        self._add_cached_contexts(
            aggregator,
            planned_candidates["cached"],
            cached_contexts_by_path,
        )

        with metrics.measure_stage("parse"):
            self._parse_uncached_candidates(
                aggregator=aggregator,
                metrics=metrics,
                planned_candidates=planned_candidates,
                cache_probes=cache_probes,
                parser_profile_key=parser_profile_key,
                limits=limits,
            )

        with metrics.measure_stage("aggregation"):
            processed_count = aggregator.success_count + aggregator.error_count
            expected_count = planned_candidates["total_candidates"]
            if processed_count != expected_count:
                raise RuntimeError(
                    "文件处理数量不匹配: "
                    f"processed={processed_count}, expected={expected_count}"
                )
            result = aggregator.build_result(planned_candidates["total_candidates"])

        logger.info(
            "扫描完成: 成功 %s, 失败 %s",
            aggregator.success_count,
            aggregator.error_count,
        )
        metrics.set_result_counts(
            success_count=result.success_count,
            error_count=result.error_count,
        )
        return self._persist_run_result(metrics, result)

    def _finish_empty_run(self, metrics: ScanMetricsCollector) -> ScanResult:
        with metrics.measure_stage("inventory_cache"):
            # 空扫描也要覆盖 inventory 快照，避免后续计划读取上一轮发现结果。
            self.scanner.scan_index_store.replace_inventory([])
            metrics.set_plan_counts(reused_count=0, reparsed_count=0)

        result = ScanResult(total_files=0, success_count=0, error_count=0, contexts=[])
        metrics.set_result_counts(
            success_count=result.success_count,
            error_count=result.error_count,
        )
        with metrics.measure_stage("aggregation"):
            pass
        return self._persist_run_result(metrics, result)

    def _add_cached_contexts(
        self,
        aggregator: ScanAggregator,
        cached_files: list[Any],
        cached_contexts_by_path: dict[Path, FileContext],
    ) -> None:
        for cached_file in cached_files:
            cached_path = self.scanner._item_path(cached_file)
            cached_context = cached_contexts_by_path.get(cached_path)
            if cached_context is None:
                aggregator.add_context(
                    FileContext(
                        file_path=str(cached_path),
                        file_type=self.scanner._item_extension(cached_file),
                        content="",
                        error="cache hit missing context",
                    )
                )
                continue
            aggregator.add_cached_context(cached_context)

    def _parse_uncached_candidates(
        self,
        *,
        aggregator: ScanAggregator,
        metrics: ScanMetricsCollector,
        planned_candidates: dict[str, Any],
        cache_probes: dict[str, Any],
        parser_profile_key: str,
        limits: dict[str, Any],
    ) -> None:
        with ThreadPoolExecutor(max_workers=self.scanner.scanner_cfg["max_workers"]) as executor:
            future_to_file = {
                executor.submit(
                    self.scanner._extract_uncached_content_with_duration,
                    item,
                    limits,
                ): item
                for item in planned_candidates["uncached"]
            }

            for future in as_completed(future_to_file):
                inventory_item = future_to_file[future]
                file_path = self.scanner._item_path(inventory_item)
                try:
                    context, duration_ms = future.result()
                    metrics.record_extension_result(
                        self.scanner._item_extension(inventory_item),
                        duration_ms,
                        context.error,
                    )
                    self.scanner._record_reparse_detail(
                        inventory_item,
                        cache_probes[self.scanner._item_identity(inventory_item)],
                        duration_ms,
                        context,
                    )
                    self.scanner._write_parse_cache(
                        inventory_item,
                        parser_profile_key,
                        context,
                    )
                    previous_truncated = aggregator.truncated_by_global_limit
                    aggregator.add_context(context)
                    if aggregator.truncated_by_global_limit and not previous_truncated:
                        logger.warning(
                            "已达全局字符上限 %s，后续文件内容将被省略",
                            aggregator.total_max_chars,
                        )
                except Exception as exc:
                    self._record_uncached_exception(
                        aggregator,
                        metrics,
                        inventory_item,
                        file_path,
                        cache_probes,
                        parser_profile_key,
                        exc,
                    )

    def _record_uncached_exception(
        self,
        aggregator: ScanAggregator,
        metrics: ScanMetricsCollector,
        inventory_item: Any,
        file_path: Path,
        cache_probes: dict[str, Any],
        parser_profile_key: str,
        exc: Exception,
    ) -> None:
        logger.error("处理文件失败 %s: %s", file_path, exc)
        metrics.record_extension_result(
            self.scanner._item_extension(inventory_item),
            0,
            str(exc),
        )
        self.scanner.scan_index_store.upsert_parse_cache(
            file_identity=self.scanner._item_identity(inventory_item),
            parser_profile=parser_profile_key,
            content_excerpt="",
            parse_status="error",
            parse_error=str(exc),
            source_version=self.scanner._item_source_version(inventory_item),
        )
        self.scanner._record_reparse_exception(
            inventory_item,
            cache_probes[self.scanner._item_identity(inventory_item)],
            str(exc),
        )
        aggregator.add_exception(file_path, exc)

    def _persist_run_result(
        self,
        metrics: ScanMetricsCollector,
        result: ScanResult,
    ) -> ScanResult:
        run_metrics = metrics.finish()
        run_id = self.scanner.scan_index_store.save_scan_run_metrics(
            run_metrics=run_metrics
        )
        result = result.model_copy(update={"scan_run_id": run_id})
        logger.info(run_metrics.to_summary_line())
        return result
