"""扫描规划边界服务。"""

from __future__ import annotations

from pathlib import Path
from typing import Iterable


class ScanPlanner:
    """负责解析预算和候选文件分流规划。"""

    def __init__(self, scanner_cfg: dict):
        self.scanner_cfg = scanner_cfg

    def build_parser_profile(self, summary_mode: bool = False) -> dict:
        """根据扫描模式生成解析预算。"""
        if summary_mode:
            profile = {
                "excel_max_rows": self.scanner_cfg.get("summary_excel_max_rows", 10),
                "pdf_max_pages": self.scanner_cfg.get("summary_pdf_max_pages", 2),
                "text_max_chars": self.scanner_cfg.get("summary_text_max_chars", 2000),
            }
        else:
            profile = {
                "excel_max_rows": self.scanner_cfg["excel_max_rows"],
                "pdf_max_pages": self.scanner_cfg["pdf_max_pages"],
                "text_max_chars": self.scanner_cfg["text_max_chars"],
            }

        profile["total_max_chars"] = self.scanner_cfg.get("total_max_chars", 50000)
        profile["summary_mode"] = summary_mode
        profile["parser_profile_version"] = self.scanner_cfg.get(
            "parser_profile_version",
            "v1",
        )
        return profile

    def plan_candidates(
        self,
        candidates: Iterable[Path],
        cached_file_paths: set[str] | None = None,
    ) -> dict:
        """把候选文件拆分为缓存命中和未命中两组。"""
        cached_file_paths = cached_file_paths or set()
        cached: list[Path] = []
        uncached: list[Path] = []

        for file_path in candidates:
            if str(file_path) in cached_file_paths:
                cached.append(file_path)
            else:
                uncached.append(file_path)

        return {
            "cached": cached,
            "uncached": uncached,
            "total_candidates": len(cached) + len(uncached),
        }
