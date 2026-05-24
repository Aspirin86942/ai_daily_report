"""扫描规划边界服务。"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Iterable


LIGHT_TEXT_PARSER_BACKEND = "light_text_v1"
DEFAULT_DIRECT_TEXT_READ_BYTES = 256 * 1024
DEFAULT_LOG_TAIL_READ_BYTES = 256 * 1024


class ScanPlanner:
    """负责解析预算和候选文件分流规划。"""

    def __init__(self, scanner_cfg: dict):
        self.scanner_cfg = scanner_cfg

    def _positive_int_config(self, key: str, default: int) -> int:
        """读取正整数配置；非法值回退默认值，避免 cache key 写入脏值。"""
        raw_value = self.scanner_cfg.get(key, default)
        try:
            value = int(raw_value)
        except (TypeError, ValueError):
            return default
        return value if value > 0 else default

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
        direct_default = self._positive_int_config(
            "direct_text_max_bytes",
            DEFAULT_DIRECT_TEXT_READ_BYTES,
        )
        direct_text_read_bytes = self._positive_int_config(
            "direct_text_read_bytes",
            direct_default,
        )
        log_tail_read_bytes = self._positive_int_config(
            "log_tail_read_bytes",
            DEFAULT_LOG_TAIL_READ_BYTES,
        )
        text_excerpt_max_chars = self._positive_int_config(
            "text_excerpt_max_chars",
            int(profile["text_max_chars"]),
        )
        profile["text_parser_backend"] = LIGHT_TEXT_PARSER_BACKEND
        profile["direct_text_read_bytes"] = direct_text_read_bytes
        profile["log_tail_read_bytes"] = log_tail_read_bytes
        profile["text_excerpt_max_chars"] = text_excerpt_max_chars
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
