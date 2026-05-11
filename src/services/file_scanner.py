"""文件扫描服务"""

import multiprocessing as mp
from pathlib import Path
from datetime import date, datetime, timedelta
from typing import List, Optional
from concurrent.futures import ThreadPoolExecutor, as_completed
import pandas as pd
import pdfplumber
from pptx import Presentation

from ..models.schemas import FileContext, ScanResult
from ..core.config import config
from ..core.logger import setup_logger
from ..utils.text_tools import truncate_text
from .scan_aggregator import ScanAggregator
from .scan_discovery import FileDiscoveryService
from .scan_index_store import ScanIndexStore
from .scan_planner import ScanPlanner

logger = setup_logger()

TEXT_FILE_TYPES = {".txt", ".md", ".csv", ".json", ".log"}
DEFAULT_FILE_TIMEOUT_SECONDS = 30.0


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

        # 发现边界只负责找候选文件，不承担解析与汇总逻辑。
        matched_files = self.discovery_service.bootstrap_full_scan(start_date, end_date)
        logger.info(f"发现 {len(matched_files)} 个文件")

        if not matched_files:
            return ScanResult(
                total_files=0, success_count=0, error_count=0, contexts=[]
            )

        parser_profile = self.scan_planner.build_parser_profile(
            summary_mode=summary_mode
        )
        planned_candidates = self.scan_planner.plan_candidates(matched_files)
        limits = {
            "excel_max_rows": parser_profile["excel_max_rows"],
            "pdf_max_pages": parser_profile["pdf_max_pages"],
            "text_max_chars": parser_profile["text_max_chars"],
        }
        aggregator = ScanAggregator(parser_profile["total_max_chars"])
        cached_contexts = self._get_cached_contexts(planned_candidates["cached"])
        cached_contexts_by_path = {
            Path(context.file_path): context for context in cached_contexts
        }

        for cached_file in planned_candidates["cached"]:
            cached_context = cached_contexts_by_path.get(cached_file)
            if cached_context is None:
                aggregator.add_context(
                    FileContext(
                        file_path=str(cached_file),
                        file_type=cached_file.suffix.lower(),
                        content="",
                        error="cache hit missing context",
                    )
                )
                continue
            aggregator.add_cached_context(cached_context)

        # 并行处理文件
        with ThreadPoolExecutor(
            max_workers=self.scanner_cfg["max_workers"]
        ) as executor:
            future_to_file = {
                executor.submit(self._extract_content_with_timeout, f, limits): f
                for f in planned_candidates["uncached"]
            }

            for future in as_completed(future_to_file):
                file_path = future_to_file[future]
                try:
                    context = future.result()
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
                    aggregator.add_exception(file_path, e)

        # 数据完整性校验
        assert (
            aggregator.success_count + aggregator.error_count
            == planned_candidates["total_candidates"]
        ), "文件处理数量不匹配"

        logger.info(
            "扫描完成: 成功 %s, 失败 %s",
            aggregator.success_count,
            aggregator.error_count,
        )

        return aggregator.build_result(planned_candidates["total_candidates"])

    def _get_cached_contexts(self, cached_files: list[Path]) -> list[FileContext]:
        """缓存上下文加载钩子，默认由后续任务接入外部缓存实现。"""
        return []

    def _get_files_in_range(self, start_date: date, end_date: date) -> List[Path]:
        """获取日期范围内修改的文件列表

        Args:
            start_date: 起始日期
            end_date: 结束日期

        Returns:
            文件路径列表
        """
        return self.discovery_service.bootstrap_full_scan(start_date, end_date)

    def _extract_content_with_timeout(
        self, file_path: Path, limits: Optional[dict] = None
    ) -> FileContext:
        """带单文件时间预算的内容提取入口。"""
        file_type = file_path.suffix.lower()
        timeout_seconds = self._resolve_file_timeout(file_type)
        context, timed_out = self._run_extract_subprocess(
            file_path,
            limits,
            timeout_seconds,
        )
        if timed_out:
            timeout_label = f"{timeout_seconds:g}"
            logger.warning(
                "解析文件超时: %s (%ss)",
                file_path,
                timeout_label,
            )
            return FileContext(
                file_path=str(file_path),
                file_type=file_type,
                content="",
                error=f"timeout: file parse exceeded {timeout_label}s",
            )
        if context is None:
            return FileContext(
                file_path=str(file_path),
                file_type=file_type,
                content="",
                error="subprocess exited without result",
            )
        return context

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
                FileContext(
                    file_path=str(file_path),
                    file_type=file_path.suffix.lower(),
                    content="",
                    error="subprocess returned invalid payload",
                ),
                False,
            )

    def _resolve_file_timeout(self, file_type: str) -> float:
        """解析单文件超时秒数，优先使用扩展名覆盖。"""
        normalized_type = file_type.lower()
        overrides = self.scanner_cfg.get("file_timeout_by_extension", {}) or {}
        timeout_value = overrides.get(
            normalized_type,
            self.scanner_cfg.get("file_timeout_seconds", DEFAULT_FILE_TIMEOUT_SECONDS),
        )
        try:
            timeout = float(timeout_value)
        except (TypeError, ValueError):
            logger.warning("非法单文件超时配置 %s，回退默认值 30s", timeout_value)
            return DEFAULT_FILE_TIMEOUT_SECONDS

        if timeout <= 0:
            logger.warning("非法单文件超时配置 %s，回退默认值 30s", timeout_value)
            return DEFAULT_FILE_TIMEOUT_SECONDS
        return timeout

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
            max_file_size_mb = self.scanner_cfg.get("max_file_size_mb")
            if max_file_size_mb is not None:
                max_bytes = float(max_file_size_mb) * 1024 * 1024
                file_size = file_path.stat().st_size
                if file_size > max_bytes:
                    return FileContext(
                        file_path=str(file_path),
                        file_type=file_type,
                        content="",
                        error=(
                            f"file too large: {file_size} bytes exceeds "
                            f"{max_file_size_mb} MB limit"
                        ),
                    )

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
