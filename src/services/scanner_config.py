"""scanner 配置纯函数与 wire contract 校验（从 config.py 迁出）。

职责：把调用方显式配置的 scanner v1 wire 叶子提取为 versioned profile，
拒绝未知字段，并把基础设施字段排除在 wire 之外。默认值与归一化的唯一
所有者仍是 Rust core。
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any


SCANNER_CONTRACT_FIELDS = (
    "allowed_extensions",
    "ignored_patterns",
    "excluded_dirs",
    "max_workers",
    "max_file_size_mb",
    "discovery_timeout_seconds",
    "file_timeout_seconds",
    "file_timeout_by_extension",
    "total_max_chars",
    "parser_profile_version",
    "office_parser_backend",
    "pdf_parser_backend",
    "office_fallback_policy_version",
    "office_parser_fallback_enabled",
    "office_fallback_after_timeout",
    "office_legacy_extensions_enabled",
    "pptx_include_notes",
    "office_parser_fallback_order",
    "direct_text_max_bytes",
    "direct_text_read_bytes",
    "log_tail_read_bytes",
    "text_excerpt_max_chars",
    "excel_max_rows",
    "pdf_max_pages",
    "text_max_chars",
    "excel_max_sheets",
    "excel_max_columns",
    "docx_max_paragraphs",
    "docx_max_tables",
    "docx_table_max_rows",
    "docx_table_max_cols",
    "pptx_max_slides",
    "document_excerpt_max_chars",
    "summary_excel_max_rows",
    "summary_pdf_max_pages",
    "summary_text_max_chars",
    "summary_excel_max_sheets",
    "summary_excel_max_columns",
    "summary_docx_max_paragraphs",
    "summary_docx_max_tables",
    "summary_docx_table_max_rows",
    "summary_docx_table_max_cols",
    "summary_pptx_max_slides",
    "summary_document_excerpt_max_chars",
)

SCANNER_INFRASTRUCTURE_FIELDS = frozenset(
    {
        "rust_office_parser_bin",
        "engine",
        "rust_scanner_bin",
        "rust_index_db_path",
        "rust_process_timeout_seconds",
    }
)


class UnknownScannerContractFieldsError(ValueError):
    """表示 scanner 配置包含不能进入版本化 wire contract 的字段。"""

    def __init__(self, fields: Sequence[str]) -> None:
        self.fields = tuple(sorted(set(fields)))
        super().__init__(
            "unknown scanner contract fields: " + ", ".join(self.fields)
        )


def _to_builtin_value(value: Any) -> Any:
    """递归转成原生容器，避免 Dynaconf 容器在 Windows spawn 下无法 pickle。"""
    if isinstance(value, Mapping):
        return {
            str(key): _to_builtin_value(item) for key, item in value.items()
        }
    if isinstance(value, Sequence) and not isinstance(
        value, (str, bytes, bytearray)
    ):
        return [_to_builtin_value(item) for item in value]
    return value


def extract_scanner_profile(scanner_settings: Any) -> dict[str, Any]:
    """提取调用方显式配置的 scanner v1 wire 叶子。

    Rust 是默认值和归一化的唯一所有者，因此这里不补默认值，也不携带
    worker、数据库或进程路径。
    """
    if isinstance(scanner_settings, Mapping):
        raw_items = scanner_settings.items()
    elif hasattr(scanner_settings, "__dict__"):
        raw_items = vars(scanner_settings).items()
    else:
        raise ValueError("scanner settings must expose explicit leaves")

    present = {
        str(key).strip().lower(): _to_builtin_value(value)
        for key, value in raw_items
    }
    unknown = sorted(
        set(present)
        - set(SCANNER_CONTRACT_FIELDS)
        - SCANNER_INFRASTRUCTURE_FIELDS
    )
    if unknown:
        raise UnknownScannerContractFieldsError(unknown)

    profile: dict[str, Any] = {"schema_version": "scanner_profile_v1"}
    for key in SCANNER_CONTRACT_FIELDS:
        if key in present:
            profile[key] = present[key]
    return profile
