"""扫描规划边界服务。"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Iterable

from .light_text_parser import (
    DEFAULT_SUMMARY_TEXT_MAX_CHARS,
    DEFAULT_TEXT_MAX_CHARS,
    LIGHT_TEXT_PARSER_BACKEND,
    build_light_text_budget,
)


class ScanPlanner:
    """负责解析预算和候选文件分流规划。"""

    def __init__(self, scanner_cfg: dict):
        self.scanner_cfg = scanner_cfg

    def build_parser_profile(self, summary_mode: bool = False) -> dict:
        """根据扫描模式生成解析预算。"""
        if summary_mode:
            text_max_chars = self.scanner_cfg.get(
                "summary_text_max_chars",
                DEFAULT_SUMMARY_TEXT_MAX_CHARS,
            )
            light_text_budget = build_light_text_budget(
                self.scanner_cfg,
                text_max_chars=text_max_chars,
                default_text_max_chars=DEFAULT_SUMMARY_TEXT_MAX_CHARS,
            )
            profile = {
                "excel_max_rows": self.scanner_cfg.get("summary_excel_max_rows", 10),
                "pdf_max_pages": self.scanner_cfg.get("summary_pdf_max_pages", 2),
                "text_max_chars": light_text_budget.text_max_chars,
            }
        else:
            text_max_chars = self.scanner_cfg.get(
                "text_max_chars",
                DEFAULT_TEXT_MAX_CHARS,
            )
            light_text_budget = build_light_text_budget(
                self.scanner_cfg,
                text_max_chars=text_max_chars,
                default_text_max_chars=DEFAULT_TEXT_MAX_CHARS,
            )
            profile = {
                "excel_max_rows": self.scanner_cfg["excel_max_rows"],
                "pdf_max_pages": self.scanner_cfg["pdf_max_pages"],
                "text_max_chars": light_text_budget.text_max_chars,
            }

        profile["total_max_chars"] = self.scanner_cfg.get("total_max_chars", 50000)
        profile["summary_mode"] = summary_mode
        profile["parser_profile_version"] = self.scanner_cfg.get(
            "parser_profile_version",
            "v1",
        )
        profile["text_parser_backend"] = LIGHT_TEXT_PARSER_BACKEND
        profile["direct_text_read_bytes"] = light_text_budget.direct_text_read_bytes
        profile["log_tail_read_bytes"] = light_text_budget.log_tail_read_bytes
        profile["text_excerpt_max_chars"] = light_text_budget.text_excerpt_max_chars
        return profile

    def serialize_parser_profile(self, profile: dict) -> str:
        """稳定序列化 parser profile，避免 cache key 因键顺序漂移。"""
        return json.dumps(profile, ensure_ascii=False, sort_keys=True, separators=(",", ":"))

    def plan_candidates(
        self,
        candidates: Iterable[object],
        cached_file_paths: set[str] | None = None,
        start_date: object | None = None,
        end_date: object | None = None,
        cache_lookup: dict[str, bool] | None = None,
    ) -> dict:
        """把候选文件拆分为缓存命中和未命中两组。"""
        cached_file_paths = cached_file_paths or set()
        cache_lookup = cache_lookup or {}
        cached: list[Path] = []
        uncached: list[Path] = []
        cached_inventory: list[object] = []
        uncached_inventory: list[object] = []

        for candidate in candidates:
            if isinstance(candidate, Path):
                if str(candidate) in cached_file_paths:
                    cached.append(candidate)
                else:
                    uncached.append(candidate)
                continue

            candidate_path = getattr(candidate, "path", None)
            file_identity = getattr(candidate, "file_identity", None)
            if candidate_path is None or file_identity is None:
                raise TypeError("candidate must be Path or inventory-like object")

            if cache_lookup.get(str(file_identity), False):
                cached_inventory.append(candidate)
            else:
                uncached_inventory.append(candidate)

        if cached_inventory or uncached_inventory:
            return {
                "cached": cached_inventory,
                "uncached": uncached_inventory,
                "total_candidates": len(cached_inventory) + len(uncached_inventory),
            }

        return {
            "cached": cached,
            "uncached": uncached,
            "total_candidates": len(cached) + len(uncached),
        }
