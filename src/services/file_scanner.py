"""文件扫描服务"""

import os
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

logger = setup_logger()


class FileScanner:
    """文件扫描器"""

    def __init__(self):
        """初始化扫描器"""
        self.scanner_cfg = config.scanner_config
        self.work_dir = config.work_dir

    def scan_today_files(self) -> ScanResult:
        """扫描今日修改的文件 (向后兼容封装)

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

        # 获取日期范围内修改的文件
        matched_files = self._get_files_in_range(start_date, end_date)
        logger.info(f"发现 {len(matched_files)} 个文件")

        if not matched_files:
            return ScanResult(
                total_files=0, success_count=0, error_count=0, contexts=[]
            )

        # 确定解析限制
        if summary_mode:
            limits = {
                "excel_max_rows": self.scanner_cfg.get("summary_excel_max_rows", 10),
                "pdf_max_pages": self.scanner_cfg.get("summary_pdf_max_pages", 2),
                "text_max_chars": self.scanner_cfg.get("summary_text_max_chars", 2000),
            }
        else:
            limits = {
                "excel_max_rows": self.scanner_cfg["excel_max_rows"],
                "pdf_max_pages": self.scanner_cfg["pdf_max_pages"],
                "text_max_chars": self.scanner_cfg["text_max_chars"],
            }

        total_max_chars = self.scanner_cfg.get("total_max_chars", 50000)

        # 并行处理文件
        contexts: list[FileContext] = []
        success_count = 0
        error_count = 0
        total_chars = 0
        truncated_by_global_limit = False

        with ThreadPoolExecutor(
            max_workers=self.scanner_cfg["max_workers"]
        ) as executor:
            future_to_file = {
                executor.submit(self._extract_content, f, limits): f
                for f in matched_files
            }

            for future in as_completed(future_to_file):
                file_path = future_to_file[future]
                try:
                    context = future.result()
                    if context.error:
                        error_count += 1
                    else:
                        success_count += 1

                    # 全局字符上限检查
                    total_chars += len(context.content)
                    if total_chars > total_max_chars and not truncated_by_global_limit:
                        truncated_by_global_limit = True
                        logger.warning(
                            f"已达全局字符上限 {total_max_chars}，后续文件内容将被省略"
                        )

                    if truncated_by_global_limit and not context.error:
                        context = FileContext(
                            file_path=context.file_path,
                            file_type=context.file_type,
                            content="(已达全局字符上限，内容省略)",
                            error=None,
                        )

                    contexts.append(context)
                except Exception as e:
                    logger.error(f"处理文件失败 {file_path}: {e}")
                    error_count += 1
                    contexts.append(
                        FileContext(
                            file_path=str(file_path),
                            file_type=file_path.suffix,
                            content="",
                            error=str(e),
                        )
                    )

        # 数据完整性校验
        assert success_count + error_count == len(matched_files), "文件处理数量不匹配"

        logger.info(f"扫描完成: 成功 {success_count}, 失败 {error_count}")

        return ScanResult(
            total_files=len(matched_files),
            success_count=success_count,
            error_count=error_count,
            contexts=contexts,
        )

    def _get_files_in_range(self, start_date: date, end_date: date) -> List[Path]:
        """获取日期范围内修改的文件列表

        Args:
            start_date: 起始日期
            end_date: 结束日期

        Returns:
            文件路径列表
        """
        start_dt = datetime.combine(start_date, datetime.min.time())
        end_dt = datetime.combine(end_date, datetime.max.time())

        files = []
        excluded_dirs = self.scanner_cfg.get("excluded_dirs", [])
        excluded_paths = [Path(d).resolve() for d in excluded_dirs]

        for root, _, filenames in os.walk(self.work_dir):
            root_path = Path(root).resolve()

            # 检查是否在排除目录中
            skip_dir = False
            for excluded in excluded_paths:
                try:
                    root_path.relative_to(excluded)
                    skip_dir = True
                    break
                except ValueError:
                    continue

            if skip_dir:
                continue

            for filename in filenames:
                # 检查扩展名
                if not any(
                    filename.endswith(ext)
                    for ext in self.scanner_cfg["allowed_extensions"]
                ):
                    continue

                # 检查忽略模式
                if any(
                    filename.startswith(pattern.replace("*", ""))
                    for pattern in self.scanner_cfg["ignored_patterns"]
                ):
                    continue

                file_path = Path(root) / filename
                try:
                    mtime = datetime.fromtimestamp(file_path.stat().st_mtime)
                    if start_dt <= mtime <= end_dt:
                        files.append(file_path)
                except Exception as e:
                    logger.warning(f"无法读取文件时间 {file_path}: {e}")

        return files

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
            if file_type in [".xlsx", ".xls"]:
                content = self._parse_excel(file_path, limits["excel_max_rows"])
            elif file_type == ".pdf":
                content = self._parse_pdf(file_path, limits["pdf_max_pages"])
            elif file_type == ".pptx":
                content = self._parse_pptx(file_path)
            elif file_type in [".txt", ".md"]:
                content = self._parse_text(file_path)
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
            df = pd.read_excel(file_path, sheet_name=sheet_name)

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

    def _parse_text(self, file_path: Path) -> str:
        """解析纯文本文件 (.txt, .md)

        Args:
            file_path: 文件路径

        Returns:
            文件文本内容
        """
        return file_path.read_text(encoding="utf-8")

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
