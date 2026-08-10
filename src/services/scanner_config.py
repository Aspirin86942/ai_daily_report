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

# v2-only leaves（spec Part 8.1 表）。显式配置出现任一叶子即输出
# schema_version=scanner_profile_v2；否则继续输出 v1。
SCANNER_PROFILE_V2_ONLY_FIELDS = frozenset(
    {
        "max_candidate_files",
        "max_pdf_text_extractions",
        "max_total_pdf_classification_pages",
        "admission_policy_version",
        "classifier_policy_version",
        "pdf_classification_timeout_ms",
        "total_deadline_ms",
        "session_concurrency",
        "max_requests_per_session",
        "session_idle_ttl_ms",
        "session_rss_limit_bytes",
    }
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
    """提取调用方显式配置的 scanner wire 叶子。

    显式配置出现任一 v2-only 叶子时输出 `schema_version=scanner_profile_v2`
    （v1 叶子是 v2 的严格子集），否则继续输出 v1。Rust 是默认值和归一化的
    唯一所有者，因此这里不补默认值，也不携带 worker、数据库或进程路径。
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
    known_fields = set(SCANNER_CONTRACT_FIELDS) | SCANNER_PROFILE_V2_ONLY_FIELDS
    unknown = sorted(set(present) - known_fields - SCANNER_INFRASTRUCTURE_FIELDS)
    if unknown:
        raise UnknownScannerContractFieldsError(unknown)

    has_v2_only = bool(set(present) & SCANNER_PROFILE_V2_ONLY_FIELDS)
    schema_version = "scanner_profile_v2" if has_v2_only else "scanner_profile_v1"

    profile: dict[str, Any] = {"schema_version": schema_version}
    ordered_fields = tuple(SCANNER_CONTRACT_FIELDS) + tuple(
        sorted(SCANNER_PROFILE_V2_ONLY_FIELDS)
    )
    for key in ordered_fields:
        if key in present:
            profile[key] = present[key]
    return profile


# report-mode 冻结默认表（spec Part 8.1）：(max_candidate_files,
# max_total_pdf_classification_pages, max_pdf_text_extractions, total_deadline_ms)。
# 分类页预算 = pdf_max_pages(100) × max_pdf_text_extractions（文件配额），
# 保证默认 profile 下每个 PDF 都能先分类后解析。
SCANNER_PROFILE_V2_QUOTA_DEFAULTS = {
    "daily": (96, 800, 8, 10_000),
    "weekly": (192, 1200, 12, 15_000),
    "monthly": (384, 1600, 16, 25_000),
}


def normalize_scanner_profile_v2(
    raw_profile: Mapping[str, Any],
    report_mode: str,
) -> dict[str, Any]:
    """v1→v2 归一化镜像（spec Part 8.1）：为 v2-only 叶子填冻结默认值。

    接受 `scanner_profile_v1` 或 `scanner_profile_v2` 输入；v1 输入（或 v2
    省略的叶子）使用 report-mode 冻结默认表。PDF 页数默认与其余 v1 叶子仍
    由 Rust 唯一归一化，生产默认值的唯一所有者是 Rust core；此函数只用于
    测试/审计的等价性断言。
    """
    schema_version = raw_profile.get("schema_version")
    if schema_version not in {"scanner_profile_v1", "scanner_profile_v2"}:
        raise ValueError("unknown scanner profile schema_version")
    if report_mode not in SCANNER_PROFILE_V2_QUOTA_DEFAULTS:
        raise ValueError("unknown report mode")
    (
        default_max_candidate_files,
        default_max_classification_pages,
        default_max_extractions,
        default_total_deadline_ms,
    ) = SCANNER_PROFILE_V2_QUOTA_DEFAULTS[report_mode]
    max_workers = raw_profile.get("max_workers")
    if max_workers is None:
        max_workers = 4
    return {
        "max_candidate_files": _leaf_or_default(
            raw_profile, "max_candidate_files", default_max_candidate_files
        ),
        "max_total_pdf_classification_pages": _leaf_or_default(
            raw_profile,
            "max_total_pdf_classification_pages",
            default_max_classification_pages,
        ),
        "max_pdf_text_extractions": _leaf_or_default(
            raw_profile, "max_pdf_text_extractions", default_max_extractions
        ),
        "total_deadline_ms": _leaf_or_default(
            raw_profile, "total_deadline_ms", default_total_deadline_ms
        ),
        "pdf_classification_timeout_ms": _leaf_or_default(
            raw_profile, "pdf_classification_timeout_ms", 2_000
        ),
        "session_concurrency": _leaf_or_default(
            raw_profile, "session_concurrency", min(max_workers, 4)
        ),
        "max_requests_per_session": _leaf_or_default(
            raw_profile, "max_requests_per_session", 128
        ),
        "session_idle_ttl_ms": _leaf_or_default(
            raw_profile, "session_idle_ttl_ms", 30_000
        ),
        "session_rss_limit_bytes": _leaf_or_default(
            raw_profile, "session_rss_limit_bytes", 512 * 1024 * 1024
        ),
    }


def _leaf_or_default(raw_profile: Mapping[str, Any], key: str, default: Any) -> Any:
    """raw 叶子缺省或显式为 None 时返回默认值。

    配置提取器不输出 None，但 pydantic model_dump 会把未设置的 Optional
    叶子显式序列化为 None；两者都视为「未配置」。
    """
    value = raw_profile.get(key)
    return default if value is None else value
