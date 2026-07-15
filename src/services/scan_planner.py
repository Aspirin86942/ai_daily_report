"""扫描规划边界服务。"""

from __future__ import annotations

import json
from collections.abc import Mapping
from pathlib import Path
from typing import Iterable

from .light_text_parser import (
    DEFAULT_SUMMARY_TEXT_MAX_CHARS,
    DEFAULT_TEXT_MAX_CHARS,
    LIGHT_TEXT_PARSER_BACKEND,
    build_light_text_budget,
    normalize_positive_int,
)
from .document_parser import (
    DEFAULT_DOCX_MAX_PARAGRAPHS,
    DEFAULT_DOCX_MAX_TABLES,
    DEFAULT_DOCX_TABLE_MAX_COLS,
    DEFAULT_DOCX_TABLE_MAX_ROWS,
    DEFAULT_EXCEL_MAX_COLUMNS,
    DEFAULT_EXCEL_MAX_ROWS,
    DEFAULT_EXCEL_MAX_SHEETS,
    DEFAULT_PPTX_MAX_SLIDES,
    PDF_TEXT_PARSER_BACKEND,
)
from .scan_timeouts import (
    DEFAULT_FILE_TIMEOUT_SECONDS,
    normalize_file_timeout,
)

DEFAULT_OFFICE_PARSER_BACKEND = "rust_office_oxide_v1"
DEFAULT_RUST_OFFICE_PARSER_BIN = (
    "rust/office_parser/target/release/ai-daily-office-parser"
)
DEFAULT_OFFICE_FALLBACK_ORDER = [
    "python_office_v1",
    "python_sharepoint_text_v1",
]
DEFAULT_OFFICE_FALLBACK_POLICY_VERSION = "hybrid_v1"
DEFAULT_SUMMARY_EXCEL_MAX_SHEETS = 2
DEFAULT_SUMMARY_EXCEL_MAX_COLUMNS = 12
DEFAULT_SUMMARY_DOCX_MAX_PARAGRAPHS = 80
DEFAULT_SUMMARY_DOCX_MAX_TABLES = 8
DEFAULT_SUMMARY_DOCX_TABLE_MAX_ROWS = 20
DEFAULT_SUMMARY_DOCX_TABLE_MAX_COLS = 8
DEFAULT_SUMMARY_PPTX_MAX_SLIDES = 15


class ScanPlanner:
    """负责解析预算和候选文件分流规划。"""

    def __init__(
        self,
        scanner_cfg: dict,
        *,
        rust_office_parser_bin_size_bytes: int | None = None,
        rust_office_parser_bin_mtime_ns: int | None = None,
    ):
        self.scanner_cfg = scanner_cfg
        self.rust_office_parser_bin_size_bytes = (
            rust_office_parser_bin_size_bytes
        )
        self.rust_office_parser_bin_mtime_ns = rust_office_parser_bin_mtime_ns

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
                "excel_max_rows": self._positive_profile_value(
                    "summary_excel_max_rows",
                    10,
                ),
                "pdf_max_pages": self._positive_profile_value(
                    "summary_pdf_max_pages",
                    2,
                ),
                "text_max_chars": light_text_budget.text_max_chars,
            }
            self._add_document_profile(
                profile,
                summary_mode=True,
                document_excerpt_default=light_text_budget.text_max_chars,
            )
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
                "excel_max_rows": self._positive_profile_value(
                    "excel_max_rows",
                    DEFAULT_EXCEL_MAX_ROWS,
                ),
                "pdf_max_pages": self._positive_profile_value("pdf_max_pages", 5),
                "text_max_chars": light_text_budget.text_max_chars,
            }
            self._add_document_profile(
                profile,
                summary_mode=False,
                document_excerpt_default=light_text_budget.text_max_chars,
            )

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

    def _add_document_profile(
        self,
        profile: dict,
        *,
        summary_mode: bool,
        document_excerpt_default: int,
    ) -> None:
        """补齐 Office/PDF 解析预算，确保 cache key 能反映 parser profile 变化。"""
        profile["office_parser_backend"] = self.scanner_cfg.get(
            "office_parser_backend",
            DEFAULT_OFFICE_PARSER_BACKEND,
        )
        profile["pdf_parser_backend"] = self.scanner_cfg.get(
            "pdf_parser_backend",
            PDF_TEXT_PARSER_BACKEND,
        )
        profile["rust_office_parser_bin"] = self.scanner_cfg.get(
            "rust_office_parser_bin",
            DEFAULT_RUST_OFFICE_PARSER_BIN,
        )
        profile["rust_office_parser_bin_size_bytes"] = (
            self.rust_office_parser_bin_size_bytes
        )
        profile["rust_office_parser_bin_mtime_ns"] = (
            self.rust_office_parser_bin_mtime_ns
        )
        profile["file_timeout_seconds"] = self._positive_timeout_value(
            self.scanner_cfg.get(
                "file_timeout_seconds",
                DEFAULT_FILE_TIMEOUT_SECONDS,
            )
        )
        timeout_overrides = (
            self.scanner_cfg.get("file_timeout_by_extension", {}) or {}
        )
        profile["file_timeout_by_extension"] = (
            {
                str(extension): self._positive_timeout_value(timeout)
                for extension, timeout in timeout_overrides.items()
            }
            if isinstance(timeout_overrides, Mapping)
            else {}
        )
        profile["office_parser_fallback_enabled"] = bool(
            self.scanner_cfg.get("office_parser_fallback_enabled", True)
        )
        profile["office_parser_fallback_order"] = list(
            self.scanner_cfg.get(
                "office_parser_fallback_order",
                DEFAULT_OFFICE_FALLBACK_ORDER,
            )
        )
        profile["office_fallback_after_timeout"] = bool(
            self.scanner_cfg.get("office_fallback_after_timeout", False)
        )
        profile["office_external_fallback"] = str(
            self.scanner_cfg.get("office_external_fallback", "disabled")
        ).strip().lower()
        profile["office_legacy_extensions_enabled"] = bool(
            self.scanner_cfg.get("office_legacy_extensions_enabled", False)
        )
        profile["office_fallback_policy_version"] = self._string_profile_value(
            "office_fallback_policy_version",
            DEFAULT_OFFICE_FALLBACK_POLICY_VERSION,
        )
        if summary_mode:
            profile["excel_max_sheets"] = self._positive_profile_value(
                "summary_excel_max_sheets",
                DEFAULT_SUMMARY_EXCEL_MAX_SHEETS,
            )
            profile["excel_max_columns"] = self._positive_profile_value(
                "summary_excel_max_columns",
                DEFAULT_SUMMARY_EXCEL_MAX_COLUMNS,
            )
            profile["docx_max_paragraphs"] = self._positive_profile_value(
                "summary_docx_max_paragraphs",
                DEFAULT_SUMMARY_DOCX_MAX_PARAGRAPHS,
            )
            profile["docx_max_tables"] = self._positive_profile_value(
                "summary_docx_max_tables",
                DEFAULT_SUMMARY_DOCX_MAX_TABLES,
            )
            profile["docx_table_max_rows"] = self._positive_profile_value(
                "summary_docx_table_max_rows",
                DEFAULT_SUMMARY_DOCX_TABLE_MAX_ROWS,
            )
            profile["docx_table_max_cols"] = self._positive_profile_value(
                "summary_docx_table_max_cols",
                DEFAULT_SUMMARY_DOCX_TABLE_MAX_COLS,
            )
            profile["pptx_max_slides"] = self._positive_profile_value(
                "summary_pptx_max_slides",
                DEFAULT_SUMMARY_PPTX_MAX_SLIDES,
            )
            profile["document_excerpt_max_chars"] = self._positive_profile_value(
                "summary_document_excerpt_max_chars",
                document_excerpt_default,
            )
        else:
            profile["excel_max_sheets"] = self._positive_profile_value(
                "excel_max_sheets",
                DEFAULT_EXCEL_MAX_SHEETS,
            )
            profile["excel_max_columns"] = self._positive_profile_value(
                "excel_max_columns",
                DEFAULT_EXCEL_MAX_COLUMNS,
            )
            profile["docx_max_paragraphs"] = self._positive_profile_value(
                "docx_max_paragraphs",
                DEFAULT_DOCX_MAX_PARAGRAPHS,
            )
            profile["docx_max_tables"] = self._positive_profile_value(
                "docx_max_tables",
                DEFAULT_DOCX_MAX_TABLES,
            )
            profile["docx_table_max_rows"] = self._positive_profile_value(
                "docx_table_max_rows",
                DEFAULT_DOCX_TABLE_MAX_ROWS,
            )
            profile["docx_table_max_cols"] = self._positive_profile_value(
                "docx_table_max_cols",
                DEFAULT_DOCX_TABLE_MAX_COLS,
            )
            profile["pptx_max_slides"] = self._positive_profile_value(
                "pptx_max_slides",
                DEFAULT_PPTX_MAX_SLIDES,
            )
            profile["document_excerpt_max_chars"] = self._positive_profile_value(
                "document_excerpt_max_chars",
                document_excerpt_default,
            )

        profile["pptx_include_notes"] = bool(
            self.scanner_cfg.get("pptx_include_notes", True)
        )

    def _positive_profile_value(self, key: str, default: int) -> int:
        """profile 使用归一化正整数，避免运行时预算与 cache key 漂移。"""
        return normalize_positive_int(self.scanner_cfg.get(key, default), default)

    def _string_profile_value(self, key: str, default: str) -> str:
        """profile 字符串值不允许为空，避免 cache key 出现非契约版本。"""
        value = self.scanner_cfg.get(key, default)
        if value is None:
            return default
        text = str(value).strip()
        return text or default

    @staticmethod
    def _positive_timeout_value(value: object) -> float:
        """timeout cache 语义与运行时一致：无效值回到统一默认值。"""
        timeout, _ = normalize_file_timeout(value)
        return timeout

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
