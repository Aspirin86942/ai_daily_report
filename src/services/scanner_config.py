"""Scanner settings extraction with Rust-owned normalization defaults."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any


SCANNER_SETTINGS_FIELDS = (
    "allowed_extensions",
    "ignored_patterns",
    "excluded_dirs",
    "max_workers",
    "max_file_size_mb",
    "discovery_timeout_seconds",
    "file_timeout_seconds",
    "file_timeout_by_extension",
    "total_max_chars",
    "fallback_after_timeout",
    "legacy_office_enabled",
    "pptx_include_notes",
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
    "max_candidate_files",
    "max_pdf_text_extractions",
    "max_total_pdf_classification_pages",
    "pdf_classification_timeout_ms",
    "total_deadline_ms",
    "worker_max_requests",
    "worker_idle_ttl_ms",
    "worker_rss_limit_bytes",
)

SCANNER_PATH_FIELDS = frozenset({"index_db_path", "office_worker_path"})


class UnknownScannerSettingsError(ValueError):
    """Scanner configuration contains removed or unknown keys."""

    def __init__(self, fields: Sequence[str]) -> None:
        self.fields = tuple(sorted(set(fields)))
        super().__init__("unknown scanner settings: " + ", ".join(self.fields))


def extract_scanner_settings(scanner_config: Any) -> dict[str, Any]:
    """Return only caller-controlled leaves; Rust owns all defaults and policy."""
    if isinstance(scanner_config, Mapping):
        raw_items = scanner_config.items()
    elif hasattr(scanner_config, "__dict__"):
        raw_items = vars(scanner_config).items()
    else:
        raise ValueError("scanner configuration must expose explicit leaves")

    present = {
        str(key).strip().lower(): _to_builtin_value(value)
        for key, value in raw_items
    }
    known = set(SCANNER_SETTINGS_FIELDS) | SCANNER_PATH_FIELDS
    unknown = sorted(set(present) - known)
    if unknown:
        raise UnknownScannerSettingsError(unknown)
    return {
        key: present[key]
        for key in SCANNER_SETTINGS_FIELDS
        if key in present
    }


def _to_builtin_value(value: Any) -> Any:
    if isinstance(value, Mapping):
        return {
            str(key): _to_builtin_value(item) for key, item in value.items()
        }
    if isinstance(value, Sequence) and not isinstance(
        value, (str, bytes, bytearray)
    ):
        return [_to_builtin_value(item) for item in value]
    return value


__all__ = [
    "SCANNER_PATH_FIELDS",
    "SCANNER_SETTINGS_FIELDS",
    "UnknownScannerSettingsError",
    "extract_scanner_settings",
]
