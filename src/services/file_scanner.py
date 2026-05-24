"""文件扫描服务"""

import multiprocessing as mp
from pathlib import Path
from datetime import date, datetime, timedelta
from typing import List, Optional
from concurrent.futures import ThreadPoolExecutor, as_completed
from time import perf_counter
import pandas as pd
import pdfplumber
from pptx import Presentation

from ..models.schemas import FileContext, ScanResult
from ..core.config import config
from ..core.logger import setup_logger
from ..utils.text_tools import truncate_text
from .scan_aggregator import ScanAggregator
from .scan_discovery import DiscoveredFile, FileDiscoveryService
from .scan_index_store import InventoryItem, ScanIndexStore
from .light_text_parser import (
    DEFAULT_TEXT_MAX_CHARS,
    LIGHT_TEXT_PARSER_BACKEND,
    LightTextParserOptions,
    build_light_text_budget,
    parse_text_like_file,
)
from .scan_metrics import ReparseDetail, ScanMetricsCollector
from .scan_planner import ScanPlanner
from .scan_worker_pool import ParserSupervisor

logger = setup_logger()

TEXT_FILE_TYPES = {".txt", ".md", ".csv", ".json", ".log"}


def _extract_content_worker(
    file_path_str: str,
    limits: dict,
    scanner_cfg: dict,
    result_queue: mp.Queue,
) -> None:
    """子进程解析单个文件并返回可序列化结果。"""
    scanner = object.__new__(FileScanner)
    scanner.scanner_cfg = scanner_cfg
    scanner.work_dir = Path(".")
    context = scanner._extract_content(Path(file_path_str), limits)
    result_queue.put(context.model_dump())


class FileScanner:
    """文件扫描器"""

    def __init__(self):
        """初始化扫描器"""
        self.scanner_cfg = config.scanner_config
        self.work_dir = config.work_dir
        self.discovery_service = FileDiscoveryService(self.work_dir, self.scanner_cfg)
        self.scan_planner = ScanPlanner(self.scanner_cfg)
        self.scan_index_store = ScanIndexStore(
            self._resolve_project_path(self.scanner_cfg["index_db_path"])
        )
        self.parser_supervisor = ParserSupervisor(
            file_timeout_seconds=self.scanner_cfg.get("file_timeout_seconds", 30.0),
            file_timeout_by_extension=(
                self.scanner_cfg.get("file_timeout_by_extension", {}) or {}
            ),
        )
        self.last_reparse_details: list[ReparseDetail] = []

    @staticmethod
    def _resolve_project_path(path_value: str | Path) -> Path:
        """把相对配置路径解析到项目根目录，避免随运行目录漂移。"""
        path = Path(path_value)
        if path.is_absolute():
            return path
        project_root = Path(__file__).resolve().parent.parent.parent
        return project_root / path

    def scan_today_files(self) -> ScanResult:
        """扫描今日修改的文件（默认日期范围封装）

        Returns:
            扫描汇总结果
        """
        today = date.today()
        yesterday = today - timedelta(days=1)
        return self.scan_files(start_date=yesterday, end_date=today)

    def scan_files(
        self,
        start_date: Optional[date] = None,
        end_date: Optional[date] = None,
        summary_mode: bool = False,
    ) -> ScanResult:
        """通用文件扫描方法

        Args:
            start_date: 起始日期 (默认昨日)
            end_date: 结束日期 (默认今日)
            summary_mode: 摘要模式 — 使用缩减的解析限制

        Returns:
            扫描汇总结果
        """
        if start_date is None:
            start_date = date.today() - timedelta(days=1)
        if end_date is None:
            end_date = date.today()

        logger.info(
            f"开始扫描工作目录: {self.work_dir} ({start_date} ~ {end_date}, summary={summary_mode})"
        )

        metrics = ScanMetricsCollector.start()
        self.last_reparse_details = []

        # 发现边界只负责找候选文件，不承担解析与汇总逻辑。
        with metrics.measure_stage("discovery"):
            discovered_files = self._normalize_discovered_files(
                self.discovery_service.bootstrap_full_scan(start_date, end_date)
            )
        logger.info(f"发现 {len(discovered_files)} 个文件")
        metrics.set_discovered_count(len(discovered_files))

        if not discovered_files:
            with metrics.measure_stage("inventory_cache"):
                # 空扫描同样要覆盖 inventory 快照，避免后续规划继续读取旧发现结果。
                self.scan_index_store.replace_inventory([])
                metrics.set_plan_counts(reused_count=0, reparsed_count=0)
            result = ScanResult(
                total_files=0, success_count=0, error_count=0, contexts=[]
            )
            metrics.set_result_counts(
                success_count=result.success_count,
                error_count=result.error_count,
            )
            with metrics.measure_stage("aggregation"):
                pass
            run_metrics = metrics.finish()
            self.scan_index_store.save_scan_run_metrics(run_metrics=run_metrics)
            logger.info(run_metrics.to_summary_line())
            return result

        with metrics.measure_stage("inventory_cache"):
            parser_profile = self.scan_planner.build_parser_profile(
                summary_mode=summary_mode
            )
            parser_profile_key = self.scan_planner.serialize_parser_profile(
                parser_profile
            )
            # 先写入 bootstrap inventory，后续计划才能稳定基于统一快照做 freshness 判断。
            self.scan_index_store.replace_inventory(
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
            inventory_items = self.scan_index_store.query_inventory(start_date, end_date)
            cache_probes = {
                item.file_identity: self.scan_index_store.probe_parse_cache(
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
            planned_candidates = self.scan_planner.plan_candidates(
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
                "excel_max_rows": parser_profile["excel_max_rows"],
                "pdf_max_pages": parser_profile["pdf_max_pages"],
                "text_max_chars": parser_profile["text_max_chars"],
            }
            cached_contexts = self._get_cached_contexts(
                planned_candidates["cached"],
                parser_profile_key,
            )
            cached_contexts_by_path = {
                Path(context.file_path): context for context in cached_contexts
            }

        aggregator = ScanAggregator(parser_profile["total_max_chars"])

        for cached_file in planned_candidates["cached"]:
            cached_path = self._item_path(cached_file)
            cached_context = cached_contexts_by_path.get(cached_path)
            if cached_context is None:
                aggregator.add_context(
                    FileContext(
                        file_path=str(cached_path),
                        file_type=self._item_extension(cached_file),
                        content="",
                        error="cache hit missing context",
                    )
                )
                continue
            aggregator.add_cached_context(cached_context)

        with metrics.measure_stage("parse"):
            # 并行处理文件
            with ThreadPoolExecutor(
                max_workers=self.scanner_cfg["max_workers"]
            ) as executor:
                future_to_file = {
                    executor.submit(
                        self._extract_uncached_content_with_duration,
                        item,
                        limits,
                    ): item
                    for item in planned_candidates["uncached"]
                }

                for future in as_completed(future_to_file):
                    inventory_item = future_to_file[future]
                    file_path = self._item_path(inventory_item)
                    try:
                        context, duration_ms = future.result()
                        metrics.record_extension_result(
                            self._item_extension(inventory_item),
                            duration_ms,
                            context.error,
                        )
                        self._record_reparse_detail(
                            inventory_item,
                            cache_probes[self._item_identity(inventory_item)],
                            duration_ms,
                            context,
                        )
                        self._write_parse_cache(
                            inventory_item,
                            parser_profile_key,
                            context,
                        )
                        previous_truncated = aggregator.truncated_by_global_limit
                        aggregator.add_context(context)
                        if (
                            aggregator.truncated_by_global_limit
                            and not previous_truncated
                        ):
                            logger.warning(
                                "已达全局字符上限 %s，后续文件内容将被省略",
                                aggregator.total_max_chars,
                            )
                    except Exception as e:
                        logger.error(f"处理文件失败 {file_path}: {e}")
                        metrics.record_extension_result(
                            self._item_extension(inventory_item),
                            0,
                            str(e),
                        )
                        self.scan_index_store.upsert_parse_cache(
                            file_identity=self._item_identity(inventory_item),
                            parser_profile=parser_profile_key,
                            content_excerpt="",
                            parse_status="error",
                            parse_error=str(e),
                            source_version=self._item_source_version(inventory_item),
                        )
                        self._record_reparse_exception(
                            inventory_item,
                            cache_probes[self._item_identity(inventory_item)],
                            str(e),
                        )
                        aggregator.add_exception(file_path, e)

        with metrics.measure_stage("aggregation"):
            # 数据完整性校验
            assert (
                aggregator.success_count + aggregator.error_count
                == planned_candidates["total_candidates"]
            ), "文件处理数量不匹配"
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
        run_metrics = metrics.finish()
        self.scan_index_store.save_scan_run_metrics(run_metrics=run_metrics)
        logger.info(run_metrics.to_summary_line())

        return result

    def _normalize_discovered_files(
        self,
        discovered_files: list[Path | DiscoveredFile],
    ) -> list[DiscoveredFile]:
        """兼容旧 Path monkeypatch，同时统一生成 inventory 所需元数据。"""
        normalized: list[DiscoveredFile] = []
        for item in discovered_files:
            if isinstance(item, DiscoveredFile):
                normalized.append(item)
                continue

            file_path = Path(item)
            stat_result = file_path.stat()
            resolved_path = file_path.resolve()
            normalized.append(
                DiscoveredFile(
                    file_identity=f"bootstrap:{str(resolved_path).lower()}",
                    path=file_path,
                    extension=file_path.suffix.lower(),
                    modified_at=datetime.fromtimestamp(stat_result.st_mtime),
                    size_bytes=stat_result.st_size,
                    source_version=(
                        f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
                    ),
                )
            )
        return normalized

    def _get_cached_contexts(
        self,
        cached_files: list[Path | InventoryItem],
        parser_profile: str,
    ) -> list[FileContext]:
        """从 parse_cache 恢复 fresh cache 命中的上下文。"""
        contexts: list[FileContext] = []
        for item in cached_files:
            cached = self.scan_index_store.load_parse_cache(
                self._item_identity(item),
                parser_profile,
                source_version=self._item_source_version(item),
            )
            parse_status = cached["parse_status"]
            parse_error = cached["parse_error"] or None
            contexts.append(
                FileContext(
                    file_path=str(self._item_path(item)),
                    file_type=self._item_extension(item),
                    content=cached["content_excerpt"],
                    error=parse_error if parse_status != "success" else None,
                    parser_backend=cached["parser_backend"] or None,
                    truncated=bool(cached["truncated"]),
                )
            )
        return contexts

    def _get_files_in_range(self, start_date: date, end_date: date) -> List[Path]:
        """获取日期范围内修改的文件列表

        Args:
            start_date: 起始日期
            end_date: 结束日期

        Returns:
            文件路径列表
        """
        discovered_files = self.discovery_service.bootstrap_full_scan(start_date, end_date)
        return [
            item.path if isinstance(item, DiscoveredFile) else Path(item)
            for item in discovered_files
        ]

    def _item_path(self, item: Path | InventoryItem) -> Path:
        """统一读取候选路径。"""
        return item if isinstance(item, Path) else Path(item.path)

    def _item_identity(self, item: Path | InventoryItem) -> str:
        """统一读取缓存身份。"""
        if isinstance(item, Path):
            return f"bootstrap:{str(item.resolve()).lower()}"
        return item.file_identity

    def _item_extension(self, item: Path | InventoryItem) -> str:
        """统一读取扩展名。"""
        return item.suffix.lower() if isinstance(item, Path) else item.extension

    def _item_source_version(self, item: Path | InventoryItem) -> str:
        """统一读取 discovery 版本指纹。"""
        if isinstance(item, Path):
            stat_result = item.stat()
            return f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
        return item.source_version

    def _write_parse_cache(
        self,
        item: Path | InventoryItem,
        parser_profile: str,
        context: FileContext,
    ) -> None:
        """把本轮解析结果写回 parse_cache。"""
        is_success = context.error is None
        self.scan_index_store.upsert_parse_cache(
            file_identity=self._item_identity(item),
            parser_profile=parser_profile,
            content_excerpt=context.content if is_success else "",
            parse_status="success" if is_success else "error",
            parse_error=context.error or "",
            source_version=self._item_source_version(item),
            parser_backend=context.parser_backend or "",
            truncated=context.truncated,
        )

    def _record_reparse_detail(
        self,
        item: Path | InventoryItem,
        cache_probe,
        duration_ms: int,
        context: FileContext,
    ) -> None:
        """记录单个重解析文件的 cache miss 原因和解析结果。"""
        self.last_reparse_details.append(
            ReparseDetail(
                path=str(self._item_path(item)),
                extension=self._item_extension(item),
                file_identity=self._item_identity(item),
                source_version=self._item_source_version(item),
                cache_status=cache_probe.cache_status,
                cache_miss_reason=cache_probe.cache_miss_reason,
                previous_source_version=cache_probe.previous_source_version,
                parse_duration_ms=duration_ms,
                parse_status="error" if context.error else "success",
                parse_error=context.error or "",
                parser_backend=context.parser_backend or "subprocess",
                truncated=context.truncated,
            )
        )

    def _record_reparse_exception(
        self,
        item: Path | InventoryItem,
        cache_probe,
        parse_error: str,
    ) -> None:
        """解析入口抛异常时，也要留下 benchmark 可见的重解析明细。"""
        self.last_reparse_details.append(
            ReparseDetail(
                path=str(self._item_path(item)),
                extension=self._item_extension(item),
                file_identity=self._item_identity(item),
                source_version=self._item_source_version(item),
                cache_status=cache_probe.cache_status,
                cache_miss_reason=cache_probe.cache_miss_reason,
                previous_source_version=cache_probe.previous_source_version,
                parse_duration_ms=0,
                parse_status="error",
                parse_error=parse_error,
                parser_backend="",
                truncated=False,
            )
        )

    def _extract_content_with_timeout(
        self, file_path: Path, limits: Optional[dict] = None
    ) -> FileContext:
        """带单文件时间预算的内容提取入口。"""
        file_type = file_path.suffix.lower()
        timeout_seconds = self.parser_supervisor.resolve_timeout(file_type)
        context, timed_out = self._run_extract_subprocess(
            file_path,
            limits,
            timeout_seconds,
        )
        if timed_out:
            logger.warning(
                "解析文件超时: %s (%ss)",
                file_path,
                f"{timeout_seconds:g}",
            )
            return self.parser_supervisor.handle_worker_timeout(file_path, file_type)
        if context is None:
            return self.parser_supervisor.handle_missing_result(file_path, file_type)
        return context

    def _extract_content_with_duration(
        self,
        file_path: Path,
        limits: Optional[dict] = None,
    ) -> tuple[FileContext, int]:
        """解析单文件并返回本 worker 内部 wall clock 耗时。"""
        started_at = perf_counter()
        context = self._extract_content_with_timeout(file_path, limits)
        duration_ms = int(round((perf_counter() - started_at) * 1000))
        return context, max(0, duration_ms)

    def _extract_uncached_content_with_duration(
        self,
        item: Path | InventoryItem,
        limits: Optional[dict] = None,
    ) -> tuple[FileContext, int]:
        """解析未缓存文件，并返回本 worker 内部 wall clock 耗时。"""
        started_at = perf_counter()
        context = self._extract_uncached_content(
            self._item_path(item),
            self._item_extension(item),
            limits,
        )
        duration_ms = int(round((perf_counter() - started_at) * 1000))
        return context, max(0, duration_ms)

    def _extract_uncached_content(
        self,
        file_path: Path,
        file_type: str,
        limits: Optional[dict] = None,
    ) -> FileContext:
        """根据文件类型选择 light text parser 或 subprocess timeout lane。"""
        effective_limits = limits or {}
        if self._should_parse_direct(file_type):
            too_large_context = self._build_file_too_large_context(
                file_path,
                file_type,
            )
            if too_large_context is not None:
                return too_large_context
            return parse_text_like_file(
                file_path=file_path,
                file_type=file_type,
                limits=effective_limits,
                options=self._build_light_text_options(effective_limits),
            )
        return self._extract_content_with_timeout(file_path, effective_limits)

    def _should_parse_direct(self, file_type: str) -> bool:
        """text-like 文件使用 bounded direct parser，避免 Windows spawn 固定开销。"""
        if str(self.scanner_cfg.get("worker_lane_mode", "direct")).lower() != "direct":
            return False
        return file_type.lower() in TEXT_FILE_TYPES

    def _build_light_text_options(self, limits: dict) -> LightTextParserOptions:
        """把 scanner 配置转换为 light parser 的有界读取选项。"""
        light_text_budget = build_light_text_budget(
            self.scanner_cfg,
            text_max_chars=limits.get(
                "text_max_chars",
                self.scanner_cfg.get("text_max_chars", DEFAULT_TEXT_MAX_CHARS),
            ),
            default_text_max_chars=DEFAULT_TEXT_MAX_CHARS,
            on_invalid=self._warn_invalid_light_text_budget,
        )
        return LightTextParserOptions(
            read_head_bytes=light_text_budget.direct_text_read_bytes,
            read_tail_bytes=light_text_budget.log_tail_read_bytes,
            max_output_chars=light_text_budget.text_excerpt_max_chars,
            parser_backend_version=LIGHT_TEXT_PARSER_BACKEND,
        )

    def _warn_invalid_light_text_budget(
        self,
        key: str,
        raw_value: object,
        default: int,
        reason: str,
    ) -> None:
        """运行时记录无效预算配置；planner 只负责静默归一化 cache key。"""
        if reason == "invalid":
            logger.warning(
                "%s 配置无效，使用默认值 %s: %r",
                key,
                default,
                raw_value,
            )
            return
        logger.warning(
            "%s 配置必须为正整数，使用默认值 %s: %r",
            key,
            default,
            raw_value,
        )

    def _build_file_too_large_context(
        self,
        file_path: Path,
        file_type: str,
    ) -> FileContext | None:
        """复用 scanner 既有 max_file_size_mb 策略，避免 direct lane 绕过门禁。"""
        max_file_size_mb = self.scanner_cfg.get("max_file_size_mb")
        if max_file_size_mb is None:
            return None

        max_bytes = float(max_file_size_mb) * 1024 * 1024
        file_size = file_path.stat().st_size
        if file_size <= max_bytes:
            return None

        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error=(
                f"file too large: {file_size} bytes exceeds "
                f"{max_file_size_mb} MB limit"
            ),
        )

    def _run_extract_subprocess(
        self,
        file_path: Path,
        limits: Optional[dict],
        timeout_seconds: float,
    ) -> tuple[Optional[FileContext], bool]:
        """在独立子进程中解析文件，返回结果和是否超时。"""
        ctx = mp.get_context("spawn")
        result_queue: mp.Queue = ctx.Queue(maxsize=1)
        process = ctx.Process(
            target=_extract_content_worker,
            args=(str(file_path), limits or {}, dict(self.scanner_cfg), result_queue),
        )
        process.start()
        process.join(timeout_seconds)

        if process.is_alive():
            process.terminate()
            process.join()
            return None, True

        try:
            payload = result_queue.get_nowait()
        except Exception:
            return None, False

        try:
            return FileContext(**payload), False
        except Exception as exc:
            logger.warning("子进程返回无效结果 %s: %s", file_path, exc)
            return (
                self.parser_supervisor.handle_invalid_payload(
                    file_path,
                    file_path.suffix.lower(),
                ),
                False,
            )

    def _resolve_file_timeout(self, file_type: str) -> float:
        """兼容旧调用点，内部转发给 supervisor。"""
        return self.parser_supervisor.resolve_timeout(file_type)

    def load_discovery_checkpoint(self, discovery_key: str) -> str | None:
        """读取 discovery checkpoint 占位值，供后续增量发现接线。"""
        return self.scan_index_store.load_checkpoint(discovery_key)

    def save_discovery_checkpoint(
        self,
        discovery_key: str,
        checkpoint_value: str,
    ) -> None:
        """写入 discovery checkpoint 占位值，但当前扫描流程仍不依赖它。"""
        self.scan_index_store.save_checkpoint(discovery_key, checkpoint_value)

    def _extract_content(
        self, file_path: Path, limits: Optional[dict] = None
    ) -> FileContext:
        """提取文件内容

        Args:
            file_path: 文件路径
            limits: 解析限制参数 (excel_max_rows, pdf_max_pages, text_max_chars)

        Returns:
            文件上下文
        """
        if limits is None:
            limits = {
                "excel_max_rows": self.scanner_cfg["excel_max_rows"],
                "pdf_max_pages": self.scanner_cfg["pdf_max_pages"],
                "text_max_chars": self.scanner_cfg["text_max_chars"],
            }

        file_type = file_path.suffix.lower()

        try:
            too_large_context = self._build_file_too_large_context(
                file_path,
                file_type,
            )
            if too_large_context is not None:
                return too_large_context

            if file_type in [".xlsx", ".xls"]:
                content = self._parse_excel(file_path, limits["excel_max_rows"])
            elif file_type == ".pdf":
                content = self._parse_pdf(file_path, limits["pdf_max_pages"])
            elif file_type == ".pptx":
                content = self._parse_pptx(file_path)
            elif file_type in TEXT_FILE_TYPES:
                content = self._parse_text(file_path, limits["text_max_chars"])
            elif file_type == ".docx":
                content = self._parse_docx(file_path)
            else:
                content = ""

            # 截断过长文本
            content = truncate_text(content, limits["text_max_chars"])

            return FileContext(
                file_path=str(file_path),
                file_type=file_type,
                content=content,
                error=None,
            )

        except Exception as e:
            logger.warning(f"解析文件失败 {file_path}: {e}")
            return FileContext(
                file_path=str(file_path), file_type=file_type, content="", error=str(e)
            )

    def _parse_excel(self, file_path: Path, max_rows: Optional[int] = None) -> str:
        """解析 Excel 文件

        Args:
            file_path: 文件路径
            max_rows: 最大行数限制

        Returns:
            Markdown 格式的表格内容
        """
        if max_rows is None:
            max_rows = self.scanner_cfg["excel_max_rows"]

        content_parts = []

        # 读取所有 Sheet
        excel_file = pd.ExcelFile(file_path)

        for sheet_name in excel_file.sheet_names:
            df = pd.read_excel(file_path, sheet_name=sheet_name, nrows=max_rows)

            # 矢量化过滤空行
            df = df.dropna(how="all")

            # 限制行数
            if len(df) > max_rows:
                df = df.head(max_rows)
                content_parts.append(f"## {sheet_name} (仅显示前 {max_rows} 行)")
            else:
                content_parts.append(f"## {sheet_name}")

            # 转换为 Markdown 表格
            if not df.empty:
                content_parts.append(df.to_markdown(index=False))

        return "\n\n".join(content_parts)

    def _parse_pdf(self, file_path: Path, max_pages: Optional[int] = None) -> str:
        """解析 PDF 文件

        Args:
            file_path: 文件路径
            max_pages: 最大页数限制

        Returns:
            提取的文本内容
        """
        if max_pages is None:
            max_pages = self.scanner_cfg["pdf_max_pages"]

        content_parts = []

        with pdfplumber.open(file_path) as pdf:
            for i, page in enumerate(pdf.pages[:max_pages]):
                text = page.extract_text()
                if text:
                    content_parts.append(f"## 第 {i + 1} 页\n{text}")

            if len(pdf.pages) > max_pages:
                content_parts.append(
                    f"\n(PDF 共 {len(pdf.pages)} 页，仅显示前 {max_pages} 页)"
                )

        return "\n\n".join(content_parts)

    def _parse_pptx(self, file_path: Path) -> str:
        """解析 PPTX 文件

        Args:
            file_path: 文件路径

        Returns:
            提取的文本内容
        """
        content_parts = []
        prs = Presentation(file_path)

        for i, slide in enumerate(prs.slides):
            slide_text = []
            for shape in slide.shapes:
                if hasattr(shape, "text") and shape.text:
                    slide_text.append(shape.text)

            if slide_text:
                content_parts.append(f"## 幻灯片 {i + 1}\n" + "\n".join(slide_text))

        return "\n\n".join(content_parts)

    def _parse_text(self, file_path: Path, max_chars: Optional[int] = None) -> str:
        """解析纯文本文件 (.txt, .md)

        Args:
            file_path: 文件路径
            max_chars: 最大读取字符数

        Returns:
            文件文本内容
        """
        if max_chars is None:
            max_chars = self.scanner_cfg["text_max_chars"]
        with open(file_path, "r", encoding="utf-8") as file:
            content = file.read(max_chars + 1)
        return content

    def _parse_docx(self, file_path: Path) -> str:
        """解析 Word 文档

        Args:
            file_path: 文件路径

        Returns:
            提取的文本内容
        """
        from docx import Document

        doc = Document(file_path)
        return "\n\n".join(p.text for p in doc.paragraphs if p.text.strip())
