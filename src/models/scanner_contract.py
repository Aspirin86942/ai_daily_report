"""Windows-first Rust scanner core 的严格 v1 进程合同。"""

from __future__ import annotations

import re
from datetime import date
from enum import Enum
from typing import Annotated, Any, Literal
from uuid import UUID

from pydantic import (
    AfterValidator,
    BaseModel,
    ConfigDict,
    Field,
    model_validator,
)


def _absolute_path(value: str) -> str:
    if "\x00" in value or not re.match(r"^(?:[A-Za-z]:[\\/]|\\\\|/)", value):
        raise ValueError("path must be absolute")
    return value


def _request_id(value: str) -> str:
    pattern = (
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-5][0-9a-fA-F]{3}-"
        r"[89abAB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}"
    )
    if not re.fullmatch(pattern, value):
        raise ValueError("request_id must use canonical RFC 4122 text")
    try:
        parsed = UUID(value)
    except ValueError as exc:
        raise ValueError("request_id must be a UUID") from exc
    if parsed.version not in {1, 2, 3, 4, 5}:
        raise ValueError("request_id UUID version must be 1..5")
    if parsed.variant != "specified in RFC 4122":
        raise ValueError("request_id must use the RFC 4122 variant")
    return value


def _date_string(value: str) -> str:
    if not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", value):
        raise ValueError("date must use YYYY-MM-DD")
    try:
        date.fromisoformat(value)
    except ValueError as exc:
        raise ValueError("date must be a valid calendar date") from exc
    return value


def _extension(value: str) -> str:
    if not re.fullmatch(r"\.[^A-Z\\/:\x00]{1,31}", value):
        raise ValueError("invalid lowercase file extension")
    return value


def _source_version(value: str) -> str:
    if not re.fullmatch(r"mtime_ns=[0-9]+:size=[0-9]+", value):
        raise ValueError("invalid source version")
    return value


def _relative_path(value: str) -> str:
    if not value or len(value) > 32767:
        raise ValueError("relative path length is invalid")
    if re.match(r"^(?:[A-Za-z]:|[\\/])", value):
        raise ValueError("audit path must be relative")
    if any(part == ".." for part in re.split(r"[\\/]", value)):
        raise ValueError("audit path must not escape its root")
    return value


AbsolutePath = Annotated[
    str,
    Field(min_length=1, max_length=32767),
    AfterValidator(_absolute_path),
]
RequestId = Annotated[str, AfterValidator(_request_id)]
DateString = Annotated[str, AfterValidator(_date_string)]
Extension = Annotated[
    str,
    Field(min_length=2, max_length=32),
    AfterValidator(_extension),
]
SourceVersion = Annotated[str, AfterValidator(_source_version)]
RelativePath = Annotated[str, AfterValidator(_relative_path)]
NonEmpty1024 = Annotated[str, Field(min_length=1, max_length=1024)]
NonEmpty4096 = Annotated[str, Field(min_length=1, max_length=4096)]
TimeoutSeconds = Annotated[int, Field(ge=1, le=3600)]
TimeoutMilliseconds = Annotated[int, Field(ge=1, le=3_600_000)]
ReadBudget = Annotated[int, Field(ge=1, le=67_108_864)]
CharBudget = Annotated[int, Field(ge=1, le=10_000_000)]
PdfPageBudget = Annotated[int, Field(ge=1, le=10_000)]
ExcelSheetBudget = Annotated[int, Field(ge=1, le=1024)]
RowBudget = Annotated[int, Field(ge=1, le=1_048_576)]
ColumnBudget = Annotated[int, Field(ge=1, le=16_384)]
ParagraphBudget = Annotated[int, Field(ge=1, le=1_000_000)]
CollectionBudget = Annotated[int, Field(ge=1, le=100_000)]
NonNegativeInt = Annotated[int, Field(ge=0)]
PositiveInt = Annotated[int, Field(ge=1)]
ReportMode = Literal["daily", "weekly", "monthly"]


class ContractModel(BaseModel):
    """共享的严格 wire 配置。"""

    model_config = ConfigDict(extra="forbid", strict=True)


class AdapterPaths(ContractModel):
    office_worker_path: AbsolutePath
    python_executable: AbsolutePath
    python_module_root: AbsolutePath
    python_document_worker_module: Annotated[
        str,
        Field(
            min_length=1,
            max_length=1024,
            pattern=(
                r"^[A-Za-z_][A-Za-z0-9_]*"
                r"(?:\.[A-Za-z_][A-Za-z0-9_]*)*$"
            ),
        ),
    ]


class RawScannerProfileV1(ContractModel):
    """只包含调用方显式提供的 scanner 叶子。"""

    schema_version: Literal["scanner_profile_v1"]
    allowed_extensions: Annotated[list[Extension], Field(max_length=256)] | None = None
    ignored_patterns: Annotated[list[NonEmpty1024], Field(max_length=256)] | None = None
    excluded_dirs: Annotated[list[NonEmpty1024], Field(max_length=256)] | None = None
    max_workers: Annotated[int, Field(ge=1, le=64)] | None = None
    max_file_size_mb: Annotated[int, Field(ge=1, le=4096)] | None = None
    discovery_timeout_seconds: TimeoutSeconds | None = None
    file_timeout_seconds: TimeoutSeconds | None = None
    file_timeout_by_extension: Annotated[
        dict[Extension, TimeoutSeconds],
        Field(max_length=256),
    ] | None = None
    total_max_chars: CharBudget | None = None
    parser_profile_version: NonEmpty1024 | None = None
    office_parser_backend: NonEmpty1024 | None = None
    pdf_parser_backend: NonEmpty1024 | None = None
    office_fallback_policy_version: NonEmpty1024 | None = None
    office_parser_fallback_enabled: bool | None = None
    office_fallback_after_timeout: bool | None = None
    office_legacy_extensions_enabled: bool | None = None
    pptx_include_notes: bool | None = None
    office_parser_fallback_order: Annotated[
        list[Literal["python_office_v1", "python_sharepoint_text_v1"]],
        Field(max_length=2),
    ] | None = None
    direct_text_max_bytes: ReadBudget | None = None
    direct_text_read_bytes: ReadBudget | None = None
    log_tail_read_bytes: ReadBudget | None = None
    text_excerpt_max_chars: CharBudget | None = None
    excel_max_rows: RowBudget | None = None
    pdf_max_pages: PdfPageBudget | None = None
    text_max_chars: CharBudget | None = None
    excel_max_sheets: ExcelSheetBudget | None = None
    excel_max_columns: ColumnBudget | None = None
    docx_max_paragraphs: ParagraphBudget | None = None
    docx_max_tables: CollectionBudget | None = None
    docx_table_max_rows: RowBudget | None = None
    docx_table_max_cols: ColumnBudget | None = None
    pptx_max_slides: CollectionBudget | None = None
    document_excerpt_max_chars: CharBudget | None = None
    summary_excel_max_rows: RowBudget | None = None
    summary_pdf_max_pages: PdfPageBudget | None = None
    summary_text_max_chars: CharBudget | None = None
    summary_excel_max_sheets: ExcelSheetBudget | None = None
    summary_excel_max_columns: ColumnBudget | None = None
    summary_docx_max_paragraphs: ParagraphBudget | None = None
    summary_docx_max_tables: CollectionBudget | None = None
    summary_docx_table_max_rows: RowBudget | None = None
    summary_docx_table_max_cols: ColumnBudget | None = None
    summary_pptx_max_slides: CollectionBudget | None = None
    summary_document_excerpt_max_chars: CharBudget | None = None

    @model_validator(mode="before")
    @classmethod
    def reject_explicit_nulls(cls, value: Any) -> Any:
        if isinstance(value, dict):
            null_fields = [key for key, item in value.items() if item is None]
            if null_fields:
                raise ValueError(
                    "raw scanner profile fields cannot be null: "
                    + ", ".join(sorted(null_fields))
                )
        return value

    @model_validator(mode="after")
    def reject_duplicate_fallbacks(self) -> "RawScannerProfileV1":
        order = self.office_parser_fallback_order
        if order is not None and len(order) != len(set(order)):
            raise ValueError("fallback order must not contain duplicates")
        return self


class RawScannerProfileV2(ContractModel):
    """只包含调用方显式提供的 scanner 叶子；v1 叶子的严格超集。

    v1 的每个叶子在 v2 里都继续可解析，另加 v2-only 叶子。全部叶子仍为
    可选，默认值的唯一所有者是 Rust 归一化。
    """

    schema_version: Literal["scanner_profile_v2"]
    allowed_extensions: Annotated[list[Extension], Field(max_length=256)] | None = None
    ignored_patterns: Annotated[list[NonEmpty1024], Field(max_length=256)] | None = None
    excluded_dirs: Annotated[list[NonEmpty1024], Field(max_length=256)] | None = None
    max_workers: Annotated[int, Field(ge=1, le=64)] | None = None
    max_file_size_mb: Annotated[int, Field(ge=1, le=4096)] | None = None
    discovery_timeout_seconds: TimeoutSeconds | None = None
    file_timeout_seconds: TimeoutSeconds | None = None
    file_timeout_by_extension: Annotated[
        dict[Extension, TimeoutSeconds],
        Field(max_length=256),
    ] | None = None
    total_max_chars: CharBudget | None = None
    parser_profile_version: NonEmpty1024 | None = None
    office_parser_backend: NonEmpty1024 | None = None
    pdf_parser_backend: NonEmpty1024 | None = None
    office_fallback_policy_version: NonEmpty1024 | None = None
    office_parser_fallback_enabled: bool | None = None
    office_fallback_after_timeout: bool | None = None
    office_legacy_extensions_enabled: bool | None = None
    pptx_include_notes: bool | None = None
    office_parser_fallback_order: Annotated[
        list[Literal["python_office_v1", "python_sharepoint_text_v1"]],
        Field(max_length=2),
    ] | None = None
    direct_text_max_bytes: ReadBudget | None = None
    direct_text_read_bytes: ReadBudget | None = None
    log_tail_read_bytes: ReadBudget | None = None
    text_excerpt_max_chars: CharBudget | None = None
    excel_max_rows: RowBudget | None = None
    pdf_max_pages: PdfPageBudget | None = None
    text_max_chars: CharBudget | None = None
    excel_max_sheets: ExcelSheetBudget | None = None
    excel_max_columns: ColumnBudget | None = None
    docx_max_paragraphs: ParagraphBudget | None = None
    docx_max_tables: CollectionBudget | None = None
    docx_table_max_rows: RowBudget | None = None
    docx_table_max_cols: ColumnBudget | None = None
    pptx_max_slides: CollectionBudget | None = None
    document_excerpt_max_chars: CharBudget | None = None
    summary_excel_max_rows: RowBudget | None = None
    summary_pdf_max_pages: PdfPageBudget | None = None
    summary_text_max_chars: CharBudget | None = None
    summary_excel_max_sheets: ExcelSheetBudget | None = None
    summary_excel_max_columns: ColumnBudget | None = None
    summary_docx_max_paragraphs: ParagraphBudget | None = None
    summary_docx_max_tables: CollectionBudget | None = None
    summary_docx_table_max_rows: RowBudget | None = None
    summary_docx_table_max_cols: ColumnBudget | None = None
    summary_pptx_max_slides: CollectionBudget | None = None
    summary_document_excerpt_max_chars: CharBudget | None = None
    # v2-only leaves（spec Part 8.1 表）
    max_candidate_files: Annotated[int, Field(ge=1, le=1_000_000)] | None = None
    max_pdf_text_extractions: Annotated[int, Field(ge=0, le=100_000)] | None = None
    max_total_pdf_classification_pages: Annotated[int, Field(ge=0, le=10_000_000)] | None = None
    admission_policy_version: Literal["budget_admission_v2"] | None = None
    classifier_policy_version: Literal["pdf_text_presence_v1"] | None = None
    pdf_classification_timeout_ms: Annotated[int, Field(ge=100, le=60_000)] | None = None
    total_deadline_ms: Annotated[int, Field(ge=5_000, le=3_600_000)] | None = None
    session_concurrency: Annotated[int, Field(ge=1, le=8)] | None = None
    max_requests_per_session: Annotated[int, Field(ge=1, le=10_000)] | None = None
    session_idle_ttl_ms: Annotated[int, Field(ge=1_000, le=600_000)] | None = None
    session_rss_limit_bytes: Annotated[int, Field(ge=67_108_864, le=8_589_934_592)] | None = None

    @model_validator(mode="before")
    @classmethod
    def reject_explicit_nulls(cls, value: Any) -> Any:
        if isinstance(value, dict):
            null_fields = [key for key, item in value.items() if item is None]
            if null_fields:
                raise ValueError(
                    "raw scanner profile fields cannot be null: "
                    + ", ".join(sorted(null_fields))
                )
        return value

    @model_validator(mode="after")
    def reject_duplicate_fallbacks(self) -> "RawScannerProfileV2":
        order = self.office_parser_fallback_order
        if order is not None and len(order) != len(set(order)):
            raise ValueError("fallback order must not contain duplicates")
        return self


class DiscoveryProfile(ContractModel):
    allowed_extensions: Annotated[list[Extension], Field(max_length=256)]
    ignored_patterns: Annotated[list[NonEmpty1024], Field(max_length=256)]
    excluded_dirs: Annotated[list[NonEmpty1024], Field(max_length=256)]

    @model_validator(mode="after")
    def require_canonical_sets(self) -> "DiscoveryProfile":
        for name in ("allowed_extensions", "ignored_patterns", "excluded_dirs"):
            values = getattr(self, name)
            if values != sorted(set(values)):
                raise ValueError(f"{name} must be sorted and unique")
        return self


class ExecutionProfile(ContractModel):
    max_workers: Annotated[int, Field(ge=1, le=64)]
    max_file_size_bytes: Annotated[int, Field(ge=1, le=4_294_967_296)]
    discovery_timeout_ms: Annotated[int, Field(ge=1000, le=3_600_000)]
    file_timeout_ms: Annotated[int, Field(ge=1000, le=3_600_000)]
    file_timeout_by_extension_ms: Annotated[
        dict[Extension, Annotated[int, Field(ge=1000, le=3_600_000)]],
        Field(max_length=256),
    ]

    @model_validator(mode="after")
    def require_sorted_timeout_keys(self) -> "ExecutionProfile":
        keys = list(self.file_timeout_by_extension_ms)
        if keys != sorted(keys):
            raise ValueError("timeout extension keys must be sorted")
        return self


class TextParseProfile(ContractModel):
    backend: Literal["light_text_v1"]
    read_head_bytes: ReadBudget
    read_tail_bytes: ReadBudget
    max_chars: CharBudget
    excerpt_max_chars: CharBudget


class OfficeParseProfile(ContractModel):
    primary_backend: NonEmpty1024
    fallback_enabled: bool
    fallback_order: Annotated[
        list[Literal["python_office_v1", "python_sharepoint_text_v1"]],
        Field(max_length=2),
    ]
    fallback_after_timeout: bool
    fallback_policy_version: NonEmpty1024
    legacy_extensions_enabled: bool
    excel_max_sheets: ExcelSheetBudget
    excel_max_rows: RowBudget
    excel_max_columns: ColumnBudget
    docx_max_paragraphs: ParagraphBudget
    docx_max_tables: CollectionBudget
    docx_table_max_rows: RowBudget
    docx_table_max_cols: ColumnBudget
    pptx_max_slides: CollectionBudget
    pptx_include_notes: bool
    document_excerpt_max_chars: CharBudget

    @model_validator(mode="after")
    def reject_duplicate_fallbacks(self) -> "OfficeParseProfile":
        if len(self.fallback_order) != len(set(self.fallback_order)):
            raise ValueError("fallback order must not contain duplicates")
        return self


class PdfParseProfile(ContractModel):
    backend: NonEmpty1024
    max_pages: PdfPageBudget
    excerpt_max_chars: CharBudget


class ParseProfile(ContractModel):
    aggregate_max_chars: CharBudget
    text: TextParseProfile
    office: OfficeParseProfile
    pdf: PdfParseProfile


class ContextProfile(ContractModel):
    profile_name: Literal[
        "daily_balanced_v1",
        "weekly_balanced_v1",
        "monthly_balanced_v1",
    ]
    global_max_chars: CharBudget
    per_file_max_chars: CharBudget
    small_file_max_bytes: Literal[65536]
    medium_file_max_bytes: Literal[1048576]
    large_file_max_bytes: Literal[10485760]
    priority_policy_version: Literal["default_v1"]
    compression_policy_version: Literal["markdown_context_v1"]

    @model_validator(mode="after")
    def validate_budget_order(self) -> "ContextProfile":
        if not (
            self.small_file_max_bytes
            < self.medium_file_max_bytes
            < self.large_file_max_bytes
        ):
            raise ValueError("context size thresholds must be increasing")
        if self.global_max_chars < self.per_file_max_chars:
            raise ValueError("global context budget must cover a file budget")
        return self


class NormalizedScannerProfileV1(ContractModel):
    schema_version: Literal["normalized_scanner_profile_v1"]
    parser_profile_version: NonEmpty1024
    report_mode: ReportMode
    discovery: DiscoveryProfile
    execution: ExecutionProfile
    parse: ParseProfile
    context: ContextProfile

    @model_validator(mode="after")
    def validate_mode_context(self) -> "NormalizedScannerProfileV1":
        expected = {
            "daily": ("daily_balanced_v1", 50_000, 8_000),
            "weekly": ("weekly_balanced_v1", 50_000, 5_000),
            "monthly": ("monthly_balanced_v1", 60_000, 4_000),
        }[self.report_mode]
        actual = (
            self.context.profile_name,
            self.context.global_max_chars,
            self.context.per_file_max_chars,
        )
        if actual != expected:
            raise ValueError("report mode and context profile do not match")
        return self


class ContextProfileV2(ContractModel):
    profile_name: Literal[
        "daily_balanced_v1",
        "weekly_balanced_v1",
        "monthly_balanced_v1",
    ]
    global_max_chars: CharBudget
    per_file_max_chars: CharBudget
    small_file_max_bytes: Literal[65536]
    medium_file_max_bytes: Literal[1048576]
    large_file_max_bytes: Literal[10485760]
    priority_policy_version: Literal["budget_nominal_v2"]
    compression_policy_version: Literal["markdown_context_v2"]

    @model_validator(mode="after")
    def validate_budget_order(self) -> "ContextProfileV2":
        if not (
            self.small_file_max_bytes
            < self.medium_file_max_bytes
            < self.large_file_max_bytes
        ):
            raise ValueError("context size thresholds must be increasing")
        if self.global_max_chars < self.per_file_max_chars:
            raise ValueError("global context budget must cover a file budget")
        return self


class NormalizedScannerProfileV2(ContractModel):
    schema_version: Literal["normalized_scanner_profile_v2"]
    parser_profile_version: NonEmpty1024
    report_mode: ReportMode
    discovery: DiscoveryProfile
    execution: ExecutionProfile
    parse: ParseProfile
    context: ContextProfileV2
    # v2-only leaves（spec Part 8.1 表），归一化后全部必填
    admission_policy_version: Literal["budget_admission_v2"]
    classifier_policy_version: Literal["pdf_text_presence_v1"]
    max_candidate_files: Annotated[int, Field(ge=1, le=1_000_000)]
    max_pdf_text_extractions: Annotated[int, Field(ge=0, le=100_000)]
    max_total_pdf_classification_pages: Annotated[int, Field(ge=0, le=10_000_000)]
    pdf_classification_timeout_ms: Annotated[int, Field(ge=100, le=60_000)]
    total_deadline_ms: Annotated[int, Field(ge=5_000, le=3_600_000)]
    session_concurrency: Annotated[int, Field(ge=1, le=8)]
    max_requests_per_session: Annotated[int, Field(ge=1, le=10_000)]
    session_idle_ttl_ms: Annotated[int, Field(ge=1_000, le=600_000)]
    session_rss_limit_bytes: Annotated[int, Field(ge=67_108_864, le=8_589_934_592)]

    @model_validator(mode="after")
    def validate_mode_context(self) -> "NormalizedScannerProfileV2":
        expected = {
            "daily": ("daily_balanced_v1", 50_000, 8_000),
            "weekly": ("weekly_balanced_v1", 50_000, 5_000),
            "monthly": ("monthly_balanced_v1", 60_000, 4_000),
        }[self.report_mode]
        actual = (
            self.context.profile_name,
            self.context.global_max_chars,
            self.context.per_file_max_chars,
        )
        if actual != expected:
            raise ValueError("report mode and context profile do not match")
        return self


ScannerProfile = Annotated[
    RawScannerProfileV1 | RawScannerProfileV2,
    Field(discriminator="schema_version"),
]


class BuildContextRequest(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    request_id: RequestId
    work_dir: AbsolutePath
    start_date: DateString
    end_date: DateString
    report_mode: ReportMode
    compression_profile: Literal[
        "daily_balanced_v1",
        "weekly_balanced_v1",
        "monthly_balanced_v1",
    ] | None
    scan_db_path: AbsolutePath
    scanner_profile: ScannerProfile
    adapters: AdapterPaths

    @model_validator(mode="after")
    def validate_dates_and_profile(self) -> "BuildContextRequest":
        if self.start_date > self.end_date:
            raise ValueError("start_date must not be after end_date")
        if self.compression_profile is not None:
            expected = f"{self.report_mode}_balanced_v1"
            if self.compression_profile != expected:
                raise ValueError("compression profile does not match report mode")
        return self


class Diagnostic(ContractModel):
    error_code: Literal[
        "INVALID_REQUEST",
        "CONTRACT_VERSION_MISMATCH",
        "WORK_DIR_NOT_FOUND",
        "WORK_DIR_NOT_DIRECTORY",
        "DISCOVERY_ENTRY_UNREADABLE",
        "FILE_TOO_LARGE",
        "PARSER_START_FAILED",
        "PARSER_TIMEOUT",
        "PARSER_INVALID_PAYLOAD",
        "PARSER_FAILED",
        "WORKER_HANDSHAKE_FAILED",
        "WORKER_VERSION_MISMATCH",
        "WORKER_BUILD_CHANGED",
        "SOURCE_VERSION_CHANGED",
        "CACHE_OPEN_FAILED",
        "CACHE_WRITE_FAILED",
        "SCAN_ALREADY_RUNNING",
        "REQUEST_IN_PROGRESS",
        "REQUEST_ID_CONFLICT",
        "RUN_NOT_FOUND",
        "RUN_CORRUPT",
        "CONTEXT_BUDGET_INVALID",
        "NOT_IMPLEMENTED",
        "RUST_CORE_CRASHED",
        "STAGE_DEADLINE_EXHAUSTED",
        "BUDGET_MODEL_MISMATCH",
        "CONTEXT_FIXED_SECTIONS_OVER_BUDGET",
        "PROFILE_ROUTE_INVARIANT",
        "SOURCE_FILE_LIMIT_EXCEEDED",
        "SOURCE_GUARD_UNAVAILABLE",
        "MAINTENANCE_MODE_UNAVAILABLE",
        "SCHEMA_UPGRADE_REQUIRED",
        "SCHEMA_MIGRATION_FAILED",
        "DIAGNOSTICS_AGGREGATED",
        "SNAPSHOT_REUSE_PROJECTED_AS_FRESH",
        "PARSE_CACHE_NOT_APPLICABLE_PROJECTED_AS_MISS",
        "CACHE_MISS_REASON_PROJECTED_AS_NEW_FILE",
        "SOURCE_GUARD_NOT_PROJECTED",
        "INSPECT_V2_PROVENANCE_UNAVAILABLE",
        "INTERNAL_ERROR",
    ]
    message: NonEmpty4096
    retryable: bool
    stage: Literal[
        "request",
        "discovery",
        "cache",
        "parse",
        "context",
        "process",
        "doctor",
        "inspect",
        "maintenance",
        "internal",
    ]
    file_path: AbsolutePath | None
    backend: NonEmpty1024 | None


WORKER_DIAGNOSTIC_V1_ERROR_CODES = (
    "INVALID_REQUEST",
    "CONTRACT_VERSION_MISMATCH",
    "WORK_DIR_NOT_FOUND",
    "WORK_DIR_NOT_DIRECTORY",
    "DISCOVERY_ENTRY_UNREADABLE",
    "FILE_TOO_LARGE",
    "PARSER_START_FAILED",
    "PARSER_TIMEOUT",
    "PARSER_INVALID_PAYLOAD",
    "PARSER_FAILED",
    "WORKER_HANDSHAKE_FAILED",
    "WORKER_VERSION_MISMATCH",
    "WORKER_BUILD_CHANGED",
    "SOURCE_VERSION_CHANGED",
    "CACHE_OPEN_FAILED",
    "CACHE_WRITE_FAILED",
    "SCAN_ALREADY_RUNNING",
    "REQUEST_IN_PROGRESS",
    "REQUEST_ID_CONFLICT",
    "RUN_NOT_FOUND",
    "RUN_CORRUPT",
    "CONTEXT_BUDGET_INVALID",
    "NOT_IMPLEMENTED",
    "RUST_CORE_CRASHED",
    "INTERNAL_ERROR",
)

WORKER_DIAGNOSTIC_V1_STAGES = (
    "request",
    "discovery",
    "cache",
    "parse",
    "context",
    "process",
    "doctor",
    "inspect",
    "internal",
)


class WorkerDiagnosticV1(ContractModel):
    """frozen ai_daily_worker_v1 diagnostic。

    wire 形状与 Diagnostic 相同，但 ErrorCode/DiagnosticStage 集合是冻结的
    v1 集合；scanner-side 新 code/stage 绝不进入。
    """

    error_code: Literal[
        "INVALID_REQUEST",
        "CONTRACT_VERSION_MISMATCH",
        "WORK_DIR_NOT_FOUND",
        "WORK_DIR_NOT_DIRECTORY",
        "DISCOVERY_ENTRY_UNREADABLE",
        "FILE_TOO_LARGE",
        "PARSER_START_FAILED",
        "PARSER_TIMEOUT",
        "PARSER_INVALID_PAYLOAD",
        "PARSER_FAILED",
        "WORKER_HANDSHAKE_FAILED",
        "WORKER_VERSION_MISMATCH",
        "WORKER_BUILD_CHANGED",
        "SOURCE_VERSION_CHANGED",
        "CACHE_OPEN_FAILED",
        "CACHE_WRITE_FAILED",
        "SCAN_ALREADY_RUNNING",
        "REQUEST_IN_PROGRESS",
        "REQUEST_ID_CONFLICT",
        "RUN_NOT_FOUND",
        "RUN_CORRUPT",
        "CONTEXT_BUDGET_INVALID",
        "NOT_IMPLEMENTED",
        "RUST_CORE_CRASHED",
        "INTERNAL_ERROR",
    ]
    message: NonEmpty4096
    retryable: bool
    stage: Literal[
        "request",
        "discovery",
        "cache",
        "parse",
        "context",
        "process",
        "doctor",
        "inspect",
        "internal",
    ]
    file_path: AbsolutePath | None
    backend: NonEmpty1024 | None


class ContextSummary(ContractModel):
    source_file_count: NonNegativeInt
    success_count: NonNegativeInt
    timeout_count: NonNegativeInt
    included_file_count: NonNegativeInt
    omitted_file_count: NonNegativeInt
    error_file_count: NonNegativeInt
    input_chars: NonNegativeInt
    output_chars: NonNegativeInt
    total_duration_ms: NonNegativeInt
    discovery_duration_ms: NonNegativeInt
    parse_duration_ms: NonNegativeInt
    compression_duration_ms: NonNegativeInt

    @model_validator(mode="after")
    def validate_counts(self) -> "ContextSummary":
        classified = self.success_count + self.timeout_count + self.error_file_count
        if classified > self.source_file_count:
            raise ValueError("classified file counts exceed source_file_count")
        return self


class EngineStatus(str, Enum):
    OK = "ok"
    PARTIAL = "partial"
    ERROR = "error"


ContextStatus = Literal["ok", "partial", "error"]


class ContextEnvelope(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    request_id: RequestId
    engine_version: NonEmpty4096
    engine_build: NonEmpty4096
    status: ContextStatus
    file_context: str
    summary: ContextSummary
    scan_run_id: PositiveInt | None
    context_run_id: PositiveInt | None
    warnings: Annotated[list[Diagnostic], Field(max_length=100_000)]
    error: Diagnostic | None

    @model_validator(mode="after")
    def validate_status(self) -> "ContextEnvelope":
        if self.status in {"ok", "partial"}:
            if not self.file_context:
                raise ValueError("successful context must not be empty")
            if self.scan_run_id is None or self.context_run_id is None:
                raise ValueError("successful context requires both run ids")
            if self.error is not None:
                raise ValueError("successful context cannot contain an error")
        if self.status == "partial" and not self.warnings:
            raise ValueError("partial context requires a warning")
        if self.status == "error":
            if self.file_context:
                raise ValueError("error context must be empty")
            if self.error is None:
                raise ValueError("error context requires a diagnostic")
        return self


def build_rust_core_crashed_envelope(
    *,
    request_id: str,
    duration_ms: int,
) -> ContextEnvelope:
    """在进程层没有可信 Rust envelope 时构造唯一的稳定错误。"""
    return ContextEnvelope(
        contract="ai_daily_context",
        protocol_version=1,
        request_id=request_id,
        engine_version="unknown",
        engine_build="unknown",
        status="error",
        file_context="",
        summary=ContextSummary(
            source_file_count=0,
            success_count=0,
            timeout_count=0,
            included_file_count=0,
            omitted_file_count=0,
            error_file_count=0,
            input_chars=0,
            output_chars=0,
            total_duration_ms=duration_ms,
            discovery_duration_ms=0,
            parse_duration_ms=0,
            compression_duration_ms=0,
        ),
        scan_run_id=None,
        context_run_id=None,
        warnings=[],
        error=Diagnostic(
            error_code="RUST_CORE_CRASHED",
            message="Rust scanner process did not return a trusted envelope",
            retryable=False,
            stage="process",
            file_path=None,
            backend=None,
        ),
    )


class TransportErrorResponse(ContractModel):
    contract: Literal["ai_daily_transport"]
    protocol_version: Literal[1]
    status: Literal["error"]
    error: WorkerDiagnosticV1

    @model_validator(mode="after")
    def validate_request_error(self) -> "TransportErrorResponse":
        if (
            self.error.error_code != "INVALID_REQUEST"
            or self.error.stage != "request"
            or self.error.file_path is not None
            or self.error.backend is not None
        ):
            raise ValueError("transport error must describe an invalid request")
        return self


class VersionResponse(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    binary_name: Literal["ai-daily-scanner"]
    engine_version: NonEmpty1024
    engine_build: NonEmpty1024
    target_triple: NonEmpty1024
    supported_commands: list[
        Literal["version", "doctor", "build-context", "inspect-run"]
    ]
    office_worker_contract_version: NonEmpty1024
    python_worker_contract_version: NonEmpty1024

    @model_validator(mode="after")
    def validate_commands(self) -> "VersionResponse":
        expected = ["version", "doctor", "build-context", "inspect-run"]
        if self.supported_commands != expected:
            raise ValueError("supported_commands must use the frozen order")
        return self


class DoctorRequest(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    request_id: RequestId
    scan_db_path: AbsolutePath
    adapters: AdapterPaths


class DoctorCheck(ContractModel):
    name: NonEmpty4096
    status: Literal["ok", "warning", "error"]
    message: NonEmpty4096


class DoctorResponse(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    request_id: RequestId
    status: Literal["ok", "partial", "error"]
    engine_version: NonEmpty4096
    engine_build: NonEmpty4096
    checks: Annotated[list[DoctorCheck], Field(max_length=256)]
    warnings: Annotated[list[Diagnostic], Field(max_length=256)]
    error: Diagnostic | None

    @model_validator(mode="after")
    def validate_status(self) -> "DoctorResponse":
        if self.status in {"ok", "partial"} and self.error:
            raise ValueError("successful doctor response cannot contain an error")
        if self.status == "partial" and not self.warnings:
            raise ValueError("partial doctor response requires a warning")
        if self.status == "error" and self.error is None:
            raise ValueError("error doctor response requires a diagnostic")
        return self


class WorkerVersionResponse(ContractModel):
    contract: Literal["ai_daily_worker"]
    protocol_version: Literal[1]
    worker_kind: Literal["office", "python_document"]
    worker_contract_version: NonEmpty1024
    worker_version: NonEmpty1024
    worker_build: NonEmpty1024
    supported_backends: Annotated[
        list[NonEmpty1024],
        Field(min_length=1, max_length=256),
    ]
    supported_extensions: Annotated[
        list[Extension],
        Field(min_length=1, max_length=256),
    ]

    @model_validator(mode="after")
    def validate_canonical_sets(self) -> "WorkerVersionResponse":
        if self.supported_backends != sorted(set(self.supported_backends)):
            raise ValueError("supported_backends must be sorted and unique")
        if self.supported_extensions != sorted(set(self.supported_extensions)):
            raise ValueError("supported_extensions must be sorted and unique")
        return self


class OfficeLimits(ContractModel):
    kind: Literal["office"]
    excel_max_sheets: ExcelSheetBudget
    excel_max_rows: RowBudget
    excel_max_columns: ColumnBudget
    docx_max_paragraphs: ParagraphBudget
    docx_max_tables: CollectionBudget
    docx_table_max_rows: RowBudget
    docx_table_max_cols: ColumnBudget
    pptx_max_slides: CollectionBudget
    pptx_include_notes: bool
    document_excerpt_max_chars: CharBudget


class PdfLimits(ContractModel):
    kind: Literal["pdf"]
    max_pages: PdfPageBudget
    excerpt_max_chars: CharBudget


class SharePointTextLimits(ContractModel):
    kind: Literal["sharepoint_text"]
    excerpt_max_chars: CharBudget


WorkerParserLimits = Annotated[
    OfficeLimits | PdfLimits | SharePointTextLimits,
    Field(discriminator="kind"),
]

WORKER_ROUTES: dict[str, tuple[set[str], str, str]] = {
    "rust_office_oxide_v1": (
        {".docx", ".pptx"},
        "rust_office_process",
        "office",
    ),
    "rust_xlsx_bounded_v1": ({".xlsx"}, "rust_office_process", "office"),
    "python_office_v1": (
        {".docx", ".pptx", ".xls", ".xlsx"},
        "python_document_process",
        "office",
    ),
    "pdf_text_v1": ({".pdf"}, "python_document_process", "pdf"),
    "python_sharepoint_text_v1": (
        {".doc", ".ppt"},
        "python_document_process",
        "sharepoint_text",
    ),
}
WorkerBackend = Literal[
    "rust_office_oxide_v1",
    "rust_xlsx_bounded_v1",
    "python_office_v1",
    "pdf_text_v1",
    "python_sharepoint_text_v1",
]


class WorkerParseRequest(ContractModel):
    contract: Literal["ai_daily_worker"]
    protocol_version: Literal[1]
    request_id: RequestId
    file_path: AbsolutePath
    file_type: Extension
    backend: WorkerBackend
    remaining_timeout_ms: TimeoutMilliseconds
    max_file_size_bytes: Annotated[int, Field(ge=1, le=4_294_967_296)]
    parser_limits: WorkerParserLimits
    expected_source_version: SourceVersion

    @model_validator(mode="after")
    def validate_route(self) -> "WorkerParseRequest":
        file_types, _, limit_kind = WORKER_ROUTES[self.backend]
        if self.file_type not in file_types:
            raise ValueError("worker backend does not support file type")
        if self.parser_limits.kind != limit_kind:
            raise ValueError("worker backend and parser limits do not match")
        return self


class WorkerParseResponse(ContractModel):
    contract: Literal["ai_daily_worker"]
    protocol_version: Literal[1]
    request_id: RequestId
    status: Literal["ok", "error"]
    file_path: AbsolutePath
    file_type: Extension
    content: str
    parser_backend: WorkerBackend
    worker_lane: Literal["rust_office_process", "python_document_process"]
    truncated: bool
    warnings: Annotated[list[WorkerDiagnosticV1], Field(max_length=256)]
    error: WorkerDiagnosticV1 | None
    duration_ms: NonNegativeInt
    worker_contract_version: NonEmpty1024
    worker_version: NonEmpty1024
    worker_build: NonEmpty1024
    observed_source_version: SourceVersion

    @model_validator(mode="after")
    def validate_status_and_route(self) -> "WorkerParseResponse":
        if self.status == "ok" and self.error is not None:
            raise ValueError("successful worker response cannot contain an error")
        if self.status == "error" and (self.content or self.error is None):
            raise ValueError("worker error requires empty content and a diagnostic")
        file_types, lane, _ = WORKER_ROUTES[self.parser_backend]
        if self.file_type not in file_types or self.worker_lane != lane:
            raise ValueError("worker response route is inconsistent")
        return self


class InspectRunRequest(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    request_id: RequestId
    scan_db_path: AbsolutePath
    scan_run_id: PositiveInt
    include_content: bool


class StageMetric(ContractModel):
    stage: Literal["discovery", "cache", "parse", "context"]
    item_count: NonNegativeInt
    duration_ms: NonNegativeInt


class ExtensionMetric(ContractModel):
    extension: Extension
    file_count: NonNegativeInt
    parse_duration_ms: NonNegativeInt
    success_count: NonNegativeInt
    error_count: NonNegativeInt
    timeout_count: NonNegativeInt


class FileAudit(ContractModel):
    relative_path: RelativePath
    file_identity: NonEmpty4096
    source_version: SourceVersion
    parse_status: Literal["success", "error", "timeout", "not_parsed"]
    parser_backend: NonEmpty4096
    worker_lane: Literal[
        "rust_core",
        "rust_office_process",
        "python_document_process",
        "not_parsed",
    ]
    cache_status: Literal["fresh", "miss"]
    cache_miss_reason: Literal[
        "",
        "new_file",
        "error_cache",
        "source_version_changed",
        "parser_profile_changed",
    ]
    truncated: bool
    content_sha256: Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
    parse_duration_ms: NonNegativeInt
    failure_class: Annotated[str, Field(max_length=1024)]
    fallback_backend: Annotated[str, Field(max_length=1024)]
    fallback_reason_code: Annotated[str, Field(max_length=1024)]


class ContextDecision(ContractModel):
    relative_path: RelativePath
    action: Literal["keep", "compress", "metadata_only", "omit", "error"]
    reason: NonEmpty4096
    priority: NonNegativeInt
    input_chars: NonNegativeInt
    output_chars: NonNegativeInt
    truncated: bool
    error_code: Annotated[str, Field(max_length=1024)]


class InspectRunResponse(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    request_id: RequestId
    scan_run_id: PositiveInt
    context_run_id: PositiveInt | None
    status: Literal["ok", "error"]
    run_status: Literal[
        "running",
        "success",
        "partial",
        "error",
        "abandoned",
    ] | None
    summary: ContextSummary
    stage_metrics: Annotated[list[StageMetric], Field(max_length=32)]
    extension_metrics: Annotated[list[ExtensionMetric], Field(max_length=256)]
    files: Annotated[list[FileAudit], Field(max_length=1_000_000)]
    decisions: Annotated[list[ContextDecision], Field(max_length=1_000_000)]
    warnings: Annotated[list[Diagnostic], Field(max_length=100_000)]
    error: Diagnostic | None

    @model_validator(mode="after")
    def validate_status(self) -> "InspectRunResponse":
        if self.status == "ok" and (self.run_status is None or self.error):
            raise ValueError("successful inspection requires run status and no error")
        if self.status == "error" and self.error is None:
            raise ValueError("failed inspection requires a diagnostic")
        return self


# ---------------------------------------------------------------------------
# v2 observation / maintenance / upgrade wire surface (types + fixtures only;
# behavior lands in later tasks).
# ---------------------------------------------------------------------------

SourceGuardKind = Literal[
    "windows_file_id_change_time_v1",
    "unix_inode_ctime_v1",
    "content_sha256_v1",
    "unavailable",
]
ParseCacheStatus = Literal["fresh", "miss", "snapshot", "not_applicable"]
ParseTransport = Literal[
    "session",
    "one_shot",
    "rust_in_process",
    "snapshot",
    "not_applicable",
]
PdfClassificationStatus = Literal[
    "text_in_parse_window",
    "no_text_in_parse_window",
    "not_classified_by_budget",
    "unknown",
    "error",
]
ClassificationCacheStatus = Literal["fresh", "miss", "snapshot", "not_eligible"]
ClassificationTransport = Literal["session", "one_shot", "snapshot", "not_applicable"]
ReuseKind = Literal["context_snapshot", "parse_cache", "none"]
Sha256Hex = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]


class CacheRetentionPolicy(ContractModel):
    policy_version: Literal["cache_retention_v1"]
    parse_cache_max_bytes: NonNegativeInt
    classification_cache_max_bytes: NonNegativeInt
    context_artifacts_max_bytes: NonNegativeInt
    terminal_audit_max_bytes: NonNegativeInt
    terminal_run_max_count: NonNegativeInt
    terminal_run_max_age_days: NonNegativeInt
    opportunistic_gc_budget_ms: NonNegativeInt


class VersionResponseV2(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    response_version: Literal[2]
    binary_name: Literal["ai-daily-scanner"]
    engine_version: NonEmpty1024
    engine_build: NonEmpty1024
    target_triple: NonEmpty1024
    supported_commands: list[
        Literal[
            "version",
            "doctor",
            "build-context",
            "inspect-run",
            "maintenance",
            "upgrade-db",
        ]
    ]
    office_worker_contract_version: Literal["ai_daily_worker_v1"]
    python_worker_contract_version: Literal["ai_daily_worker_v1"]
    accepted_scanner_profile_versions: list[
        Literal["scanner_profile_v1", "scanner_profile_v2"]
    ]
    inspect_response_versions: list[Literal[1, 2]]
    classifier_contract_versions: list[Literal["ai_daily_pdf_classifier_v1"]]
    session_contract_versions: list[Literal["ai_daily_python_session_v1"]]
    maintenance_contract_versions: list[Literal["ai_daily_scanner_maintenance_v1"]]
    upgrade_contract_versions: list[Literal["ai_daily_scanner_upgrade_v1"]]
    source_guard_policy: Literal["source_guard_v2"]
    max_source_files_per_run: Literal[1000000]
    cache_retention_policy: CacheRetentionPolicy

    @model_validator(mode="after")
    def validate_canonical_arrays(self) -> "VersionResponseV2":
        expected_commands = [
            "version",
            "doctor",
            "build-context",
            "inspect-run",
            "maintenance",
            "upgrade-db",
        ]
        if self.supported_commands != expected_commands:
            raise ValueError("supported_commands must use the frozen order")
        if self.accepted_scanner_profile_versions != [
            "scanner_profile_v1",
            "scanner_profile_v2",
        ]:
            raise ValueError("accepted_scanner_profile_versions must be canonical")
        if self.inspect_response_versions != [1, 2]:
            raise ValueError("inspect_response_versions must be canonical")
        return self


class PdfClassificationAuditV1(ContractModel):
    status: PdfClassificationStatus
    # spec Part 3: the three page fields are nullable u64 — 0 is a legitimate
    # value (snapshot/not_eligible rows report run/result pages as 0).
    page_count: NonNegativeInt | None
    classification_cache_status: ClassificationCacheStatus
    classification_cache_miss_reason: Annotated[str, Field(max_length=1024)]
    result_examined_pages: NonNegativeInt | None
    run_inspected_pages: NonNegativeInt | None
    nominal_charged_pages: NonNegativeInt
    duration_ms: NonNegativeInt
    transport: ClassificationTransport
    attempt_count: Annotated[int, Field(ge=0, le=3)]
    classifier_build: Sha256Hex
    classifier_profile_hash: Sha256Hex

    @model_validator(mode="after")
    def validate_cache_miss_reason(self) -> "PdfClassificationAuditV1":
        if self.classification_cache_status == "miss" and not self.classification_cache_miss_reason:
            raise ValueError("classification cache miss requires a miss reason")
        if self.classification_cache_status != "miss" and self.classification_cache_miss_reason:
            raise ValueError("non-miss classification cache must have an empty miss reason")
        return self


# ---------------------------------------------------------------------------
# ai_daily_pdf_classifier_v1 wire（spec Part 7.1）：独立于共享 worker v1
# ---------------------------------------------------------------------------

PythonOperationErrorCode = Literal[
    "INVALID_REQUEST",
    "PARSER_START_FAILED",
    "PARSER_TIMEOUT",
    "PARSER_INVALID_PAYLOAD",
    "PARSER_FAILED",
    "SOURCE_VERSION_CHANGED",
    "INTERNAL_ERROR",
]
PythonOperationStage = Literal["request", "parse", "process"]
PdfClassifierStatus = Literal[
    "text_in_parse_window",
    "no_text_in_parse_window",
    "unknown",
    "error",
]


class PythonOperationDiagnosticV1(ContractModel):
    error_code: PythonOperationErrorCode
    message: Annotated[str, Field(min_length=1, max_length=4096)]
    retryable: bool
    stage: PythonOperationStage
    file_path: AbsolutePath | None
    backend: Annotated[str | None, Field(min_length=1, max_length=1024)] = None


class PdfClassifierRequestV1(ContractModel):
    contract: Literal["ai_daily_pdf_classifier"]
    protocol_version: Literal[1]
    request_id: RequestId
    file_path: AbsolutePath
    source_version: SourceVersion
    max_pages: Annotated[int, Field(ge=1, le=10_000)]
    policy_version: Literal["pdf_text_presence_v1"]


class PdfClassifierResultV1(ContractModel):
    status: PdfClassifierStatus
    page_count: PositiveInt | None
    result_examined_pages: PositiveInt | None
    diagnostic: PythonOperationDiagnosticV1 | None

    @model_validator(mode="after")
    def validate_status_invariants(self) -> "PdfClassifierResultV1":
        if self.status in {"text_in_parse_window", "no_text_in_parse_window"}:
            if self.diagnostic is not None:
                raise ValueError("text/no-text result must not carry a diagnostic")
            if self.page_count is None or self.result_examined_pages is None:
                raise ValueError("text/no-text result requires page counts")
        else:
            if self.diagnostic is None:
                raise ValueError("unknown/error result requires a diagnostic")
            if self.diagnostic.retryable != (self.status == "unknown"):
                raise ValueError("unknown must be retryable and error must not be")
        return self


class PdfClassifierResponseV1(ContractModel):
    contract: Literal["ai_daily_pdf_classifier"]
    protocol_version: Literal[1]
    request_id: RequestId
    status: Literal["ok", "error"]
    result: PdfClassifierResultV1 | None
    error: PythonOperationDiagnosticV1 | None

    @model_validator(mode="after")
    def validate_status_invariants(self) -> "PdfClassifierResponseV1":
        if self.status == "ok":
            if self.result is None or self.error is not None:
                raise ValueError("ok classifier response requires a result and no error")
        else:
            if self.result is not None or self.error is None:
                raise ValueError("error classifier response requires an error and no result")
        return self


class ClassifierVersionResponseV1(ContractModel):
    contract: Literal["ai_daily_pdf_classifier"]
    protocol_version: Literal[1]
    classifier_contract_version: Literal["ai_daily_pdf_classifier_v1"]
    classifier_build: Sha256Hex
    policy_version: Literal["pdf_text_presence_v1"]
    python_implementation: NonEmpty1024
    python_version: NonEmpty1024
    unicode_data_version: NonEmpty1024
    pypdfium2_version: NonEmpty1024
    pdfium_version: NonEmpty1024
    target_triple: NonEmpty1024


class FileAuditV2(ContractModel):
    relative_path: RelativePath
    file_identity: NonEmpty4096
    source_version: SourceVersion
    source_guard_kind: SourceGuardKind
    source_guard_sha256: Sha256Hex | None
    parse_status: Literal["success", "error", "timeout", "not_parsed"]
    parser_backend: NonEmpty4096
    worker_lane: Literal[
        "rust_core",
        "rust_office_process",
        "python_document_process",
        "not_parsed",
    ]
    parse_cache_status: ParseCacheStatus
    cache_miss_reason: Annotated[str, Field(max_length=1024)]
    truncated: bool
    content_sha256: Sha256Hex
    parse_duration_ms: NonNegativeInt
    failure_class: Annotated[str, Field(max_length=1024)]
    fallback_backend: Annotated[str, Field(max_length=1024)]
    fallback_reason_code: Annotated[str, Field(max_length=1024)]
    parse_transport: ParseTransport
    parse_attempt_count: NonNegativeInt
    final_diagnostic: Diagnostic | None
    pdf_classification: PdfClassificationAuditV1 | None

    @model_validator(mode="after")
    def validate_source_guard_and_cache(self) -> "FileAuditV2":
        if self.source_guard_kind == "unavailable":
            if self.source_guard_sha256 is not None:
                raise ValueError("unavailable source guard must have a null hash")
        elif self.source_guard_sha256 is None:
            raise ValueError("source guard kind requires a sha256")
        if self.parse_cache_status == "miss" and not self.cache_miss_reason:
            raise ValueError("parse cache miss requires a miss reason")
        if self.parse_cache_status != "miss" and self.cache_miss_reason:
            raise ValueError("non-miss parse cache must have an empty miss reason")
        if self.parse_status in {"error", "timeout"} and self.final_diagnostic is None:
            raise ValueError("error/timeout file audit requires a final diagnostic")
        if self.parse_status in {"success", "not_parsed"} and self.final_diagnostic is not None:
            raise ValueError("success/not_parsed file audit must not carry a final diagnostic")
        return self


class ExecutionMetricsV2(ContractModel):
    discovery_observed_file_count: NonNegativeInt
    source_guard_content_hash_file_count: NonNegativeInt
    source_guard_unavailable_count: NonNegativeInt
    source_guard_bytes_read: NonNegativeInt
    candidate_file_count: NonNegativeInt
    admitted_file_count: NonNegativeInt
    classification_slot_count: NonNegativeInt
    confirmed_run_inspected_pages_total: NonNegativeInt
    unobserved_classification_attempt_count: NonNegativeInt
    nominal_charged_pages_total: NonNegativeInt
    extraction_slot_count: NonNegativeInt
    pdfplumber_invocations: NonNegativeInt
    snapshot_hit: bool
    parse_cache_lookup_count: NonNegativeInt
    classification_cache_lookup_count: NonNegativeInt
    parse_cache_all_hit: bool | None
    classification_cache_all_hit: bool | None
    stage_deadline_exhausted_count: Annotated[int, Field(ge=0, le=1)]
    session_restart_count: NonNegativeInt
    session_fallback_count: NonNegativeInt
    classify_attempt_count: NonNegativeInt
    parse_attempt_count: NonNegativeInt
    reserved_chars: NonNegativeInt
    rendered_chars: NonNegativeInt
    worker_handshake_ms: NonNegativeInt
    discovery_ms: NonNegativeInt
    snapshot_lookup_ms: NonNegativeInt
    current_run_audit_write_ms: NonNegativeInt
    terminal_precommit_ms: NonNegativeInt
    deadline_precommit_elapsed_ms: NonNegativeInt
    envelope_rebuild_ms: NonNegativeInt
    terminal_rows_written: NonNegativeInt
    peak_worker_rss_bytes: NonNegativeInt | None

    @model_validator(mode="after")
    def validate_all_hit_nullability(self) -> "ExecutionMetricsV2":
        if self.parse_cache_lookup_count == 0 and self.parse_cache_all_hit is not None:
            raise ValueError("parse_cache_all_hit must be null when no parse lookup occurred")
        if self.parse_cache_lookup_count > 0 and self.parse_cache_all_hit is None:
            raise ValueError("parse_cache_all_hit is required after a parse lookup")
        if self.classification_cache_lookup_count == 0 and self.classification_cache_all_hit is not None:
            raise ValueError(
                "classification_cache_all_hit must be null when no classification lookup occurred"
            )
        if self.classification_cache_lookup_count > 0 and self.classification_cache_all_hit is None:
            raise ValueError(
                "classification_cache_all_hit is required after a classification lookup"
            )
        return self

    def is_error_sentinel(self) -> bool:
        """Inspect v2 `status=error` 的固定 sentinel（spec Part 5.3）。"""
        numerics = [
            self.discovery_observed_file_count,
            self.source_guard_content_hash_file_count,
            self.source_guard_unavailable_count,
            self.source_guard_bytes_read,
            self.candidate_file_count,
            self.admitted_file_count,
            self.classification_slot_count,
            self.confirmed_run_inspected_pages_total,
            self.unobserved_classification_attempt_count,
            self.nominal_charged_pages_total,
            self.extraction_slot_count,
            self.pdfplumber_invocations,
            self.parse_cache_lookup_count,
            self.classification_cache_lookup_count,
            self.stage_deadline_exhausted_count,
            self.session_restart_count,
            self.session_fallback_count,
            self.classify_attempt_count,
            self.parse_attempt_count,
            self.reserved_chars,
            self.rendered_chars,
            self.worker_handshake_ms,
            self.discovery_ms,
            self.snapshot_lookup_ms,
            self.current_run_audit_write_ms,
            self.terminal_precommit_ms,
            self.deadline_precommit_elapsed_ms,
            self.envelope_rebuild_ms,
            self.terminal_rows_written,
        ]
        return (
            not self.snapshot_hit
            and all(value == 0 for value in numerics)
            and self.parse_cache_all_hit is None
            and self.classification_cache_all_hit is None
            and self.peak_worker_rss_bytes is None
        )


class InspectRunResponseV2(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    response_version: Literal[2]
    request_id: RequestId
    scan_run_id: PositiveInt
    context_run_id: PositiveInt | None
    status: Literal["ok", "error"]
    run_status: Literal[
        "running",
        "success",
        "partial",
        "error",
        "abandoned",
    ] | None
    summary: ContextSummary
    stage_metrics: Annotated[list[StageMetric], Field(max_length=32)]
    extension_metrics: Annotated[list[ExtensionMetric], Field(max_length=256)]
    files: Annotated[list[FileAuditV2], Field(max_length=1_000_000)]
    decisions: Annotated[list[ContextDecision], Field(max_length=1_000_000)]
    warnings: Annotated[list[Diagnostic], Field(max_length=100_000)]
    error: Diagnostic | None
    artifact_id: PositiveInt | None
    reused_from_context_run_id: PositiveInt | None
    reuse_kind: ReuseKind
    execution_metrics: ExecutionMetricsV2

    @model_validator(mode="after")
    def validate_status_and_reuse(self) -> "InspectRunResponseV2":
        if self.status == "ok":
            if self.run_status is None or self.error:
                raise ValueError("successful inspection requires run status and no error")
            if self.run_status in {"success", "partial"} and self.artifact_id is None:
                raise ValueError("successful inspect v2 run requires artifact_id")
            if self.run_status == "error" and self.artifact_id is not None:
                raise ValueError("error run inspect v2 response must have a null artifact_id")
        if self.status == "error":
            if self.error is None:
                raise ValueError("failed inspection requires a diagnostic")
            if (
                self.artifact_id is not None
                or self.reused_from_context_run_id is not None
                or self.reuse_kind != "none"
                or self.files
                or self.decisions
                or not self.execution_metrics.is_error_sentinel()
            ):
                raise ValueError("error inspection must carry the empty sentinel shape")
        if self.reuse_kind == "context_snapshot" and self.reused_from_context_run_id is None:
            raise ValueError("context_snapshot reuse requires reused_from_context_run_id")
        if self.reuse_kind == "context_snapshot" and not self.execution_metrics.snapshot_hit:
            raise ValueError("context_snapshot reuse requires snapshot_hit")
        if self.reuse_kind != "context_snapshot" and self.reused_from_context_run_id is not None:
            raise ValueError("reused_from_context_run_id is only allowed for context_snapshot reuse")
        if self.reuse_kind == "parse_cache" and (
            self.execution_metrics.parse_cache_lookup_count == 0
            or self.execution_metrics.parse_cache_all_hit is not True
            or self.execution_metrics.snapshot_hit
        ):
            raise ValueError(
                "parse_cache reuse requires a parse lookup all-hit and no snapshot"
            )
        return self


class MaintenanceRequestV1(ContractModel):
    contract: Literal["ai_daily_scanner_maintenance"]
    protocol_version: Literal[1]
    request_id: RequestId
    scan_db_path: AbsolutePath
    mode: Literal["gc", "incremental_vacuum"]
    dry_run: bool


class MaintenanceSizeV1(ContractModel):
    parse_cache_logical_bytes: NonNegativeInt
    classification_cache_logical_bytes: NonNegativeInt
    context_artifacts_logical_bytes: NonNegativeInt
    terminal_audit_logical_bytes: NonNegativeInt
    database_file_bytes: NonNegativeInt
    wal_file_bytes: NonNegativeInt
    shm_file_bytes: NonNegativeInt
    total_physical_bytes: NonNegativeInt
    freelist_bytes: NonNegativeInt
    auto_vacuum_mode: Literal["none", "full", "incremental"]


class MaintenanceDeletedV1(ContractModel):
    parse_cache_rows: NonNegativeInt
    classification_cache_rows: NonNegativeInt
    context_artifacts_rows: NonNegativeInt
    context_artifact_files_rows: NonNegativeInt
    context_artifact_decisions_rows: NonNegativeInt
    scan_runs_rows: NonNegativeInt
    scan_run_attempts_rows: NonNegativeInt
    run_diagnostics_rows: NonNegativeInt
    scan_file_results_rows: NonNegativeInt
    scan_stage_metrics_rows: NonNegativeInt
    scan_extension_metrics_rows: NonNegativeInt
    context_runs_rows: NonNegativeInt
    context_decisions_rows: NonNegativeInt
    file_inventory_rows: NonNegativeInt


class MaintenanceVacuumV1(ContractModel):
    mode: Literal["gc", "incremental_vacuum"]
    status: Literal["not_requested", "skipped_dry_run", "ok", "error"]
    pages_changed: NonNegativeInt


class MaintenanceResponseV1(ContractModel):
    contract: Literal["ai_daily_scanner_maintenance"]
    protocol_version: Literal[1]
    request_id: RequestId
    status: Literal["ok", "error"]
    cache_retention_policy: CacheRetentionPolicy
    before: MaintenanceSizeV1
    after: MaintenanceSizeV1
    after_complete: bool
    deleted: MaintenanceDeletedV1
    pre_integrity_check: Literal["ok", "failed"]
    post_integrity_check: Literal["not_run", "ok", "failed"]
    vacuum: MaintenanceVacuumV1
    warnings: Annotated[list[Diagnostic], Field(max_length=257)]
    error: Diagnostic | None

    @model_validator(mode="after")
    def validate_status(self) -> "MaintenanceResponseV1":
        if self.status == "ok":
            if (
                self.error
                or self.pre_integrity_check != "ok"
                or self.post_integrity_check == "failed"
                or self.vacuum.status == "error"
                or not self.after_complete
            ):
                raise ValueError("ok maintenance response violates status invariants")
        if self.status == "error" and self.error is None:
            raise ValueError("error maintenance response requires a diagnostic")
        return self


class UpgradeDatabaseRequestV1(ContractModel):
    contract: Literal["ai_daily_scanner_upgrade"]
    protocol_version: Literal[1]
    request_id: RequestId
    scan_db_path: AbsolutePath
    apply: bool


class UpgradeDatabaseResponseV1(ContractModel):
    contract: Literal["ai_daily_scanner_upgrade"]
    protocol_version: Literal[1]
    request_id: RequestId
    status: Literal["ok", "partial", "error"]
    source_user_version: PositiveInt | None
    target_user_version: Literal[2]
    apply: bool
    schema_migrated: bool
    auto_vacuum_converted: bool
    legacy_parse_cache_rows_detected: NonNegativeInt
    invalidated_parse_cache_rows: NonNegativeInt
    pre_integrity_check: Literal["not_run", "ok", "failed"]
    post_integrity_check: Literal["not_run", "ok", "failed"]
    warnings: Annotated[list[Diagnostic], Field(max_length=257)]
    error: Diagnostic | None

    @model_validator(mode="after")
    def validate_status(self) -> "UpgradeDatabaseResponseV1":
        if self.invalidated_parse_cache_rows > self.legacy_parse_cache_rows_detected:
            raise ValueError("invalidated_parse_cache_rows exceeds detected rows")
        if self.status in {"ok", "partial"} and self.error:
            raise ValueError("successful upgrade response cannot contain an error")
        if self.status == "partial" and not self.warnings:
            raise ValueError("partial upgrade response requires a warning")
        if self.status == "error" and self.error is None:
            raise ValueError("error upgrade response requires a diagnostic")
        return self


SCHEMA_MODELS: dict[str, type[ContractModel]] = {
    "build-context-request-v1.schema.json": BuildContextRequest,
    "context-envelope-v1.schema.json": ContextEnvelope,
    "diagnostic-v1.schema.json": Diagnostic,
    "doctor-request-v1.schema.json": DoctorRequest,
    "doctor-response-v1.schema.json": DoctorResponse,
    "inspect-run-request-v1.schema.json": InspectRunRequest,
    "inspect-run-response-v1.schema.json": InspectRunResponse,
    "scanner-profile-normalized-v1.schema.json": NormalizedScannerProfileV1,
    "scanner-profile-request-v1.schema.json": RawScannerProfileV1,
    "transport-error-v1.schema.json": TransportErrorResponse,
    "version-response-v1.schema.json": VersionResponse,
    "worker-diagnostic-v1.schema.json": WorkerDiagnosticV1,
    "worker-parse-request-v1.schema.json": WorkerParseRequest,
    "worker-parse-response-v1.schema.json": WorkerParseResponse,
    "worker-version-response-v1.schema.json": WorkerVersionResponse,
}


def validate_contract_payload(
    schema_name: str,
    payload: dict[str, Any],
    *,
    related_payloads: list[dict[str, Any]] | None = None,
) -> ContractModel:
    """按 schema 名解析 DTO，并执行 JSON Schema 之外的关系门禁。"""
    try:
        model_type = SCHEMA_MODELS[schema_name]
    except KeyError as exc:
        raise ValueError(f"unknown scanner contract schema: {schema_name}") from exc
    parsed = model_type.model_validate(payload)

    related: dict[str, ContractModel] = {}
    for item in related_payloads or []:
        role = item.get("role")
        related_schema = item.get("schema")
        related_payload = item.get("payload")
        if not isinstance(role, str) or not isinstance(related_payload, dict):
            raise ValueError("invalid related contract payload")
        try:
            related_type = SCHEMA_MODELS[str(related_schema)]
        except KeyError as exc:
            raise ValueError("unknown related contract schema") from exc
        related[role] = related_type.model_validate(related_payload)

    request = related.get("request")
    if isinstance(parsed, ContextEnvelope) and isinstance(
        request,
        BuildContextRequest,
    ):
        if parsed.request_id != request.request_id:
            raise ValueError("context response request_id mismatch")

    if isinstance(parsed, WorkerParseResponse) and isinstance(
        request,
        WorkerParseRequest,
    ):
        echoed = (
            parsed.request_id == request.request_id
            and parsed.file_path == request.file_path
            and parsed.file_type == request.file_type
            and parsed.parser_backend == request.backend
            and parsed.observed_source_version == request.expected_source_version
        )
        if not echoed:
            raise ValueError("worker response does not echo its request")
        handshake = related.get("handshake")
        if isinstance(handshake, WorkerVersionResponse):
            identity = (
                parsed.worker_contract_version == handshake.worker_contract_version
                and parsed.worker_version == handshake.worker_version
                and parsed.worker_build == handshake.worker_build
            )
            expected_kind = (
                "office"
                if request.backend
                in {"rust_office_oxide_v1", "rust_xlsx_bounded_v1"}
                else "python_document"
            )
            supported = (
                handshake.worker_kind == expected_kind
                and request.backend in handshake.supported_backends
                and request.file_type in handshake.supported_extensions
            )
            if not identity or not supported:
                raise ValueError("worker response identity changed after handshake")

    return parsed


__all__ = [
    "AdapterPaths",
    "BuildContextRequest",
    "CacheRetentionPolicy",
    "ContextEnvelope",
    "ContextProfileV2",
    "ContextSummary",
    "ClassifierVersionResponseV1",
    "Diagnostic",
    "DoctorCheck",
    "DoctorRequest",
    "DoctorResponse",
    "EngineStatus",
    "ExecutionMetricsV2",
    "FileAuditV2",
    "InspectRunRequest",
    "InspectRunResponse",
    "InspectRunResponseV2",
    "MaintenanceDeletedV1",
    "MaintenanceRequestV1",
    "MaintenanceResponseV1",
    "MaintenanceSizeV1",
    "MaintenanceVacuumV1",
    "NormalizedScannerProfileV1",
    "NormalizedScannerProfileV2",
    "PdfClassificationAuditV1",
    "PdfClassifierRequestV1",
    "PdfClassifierResponseV1",
    "PdfClassifierResultV1",
    "PythonOperationDiagnosticV1",
    "RawScannerProfileV1",
    "RawScannerProfileV2",
    "ScannerProfile",
    "TransportErrorResponse",
    "UpgradeDatabaseRequestV1",
    "UpgradeDatabaseResponseV1",
    "VersionResponse",
    "VersionResponseV2",
    "WORKER_DIAGNOSTIC_V1_ERROR_CODES",
    "WORKER_DIAGNOSTIC_V1_STAGES",
    "WorkerDiagnosticV1",
    "WorkerParseRequest",
    "WorkerParseResponse",
    "WorkerParserLimits",
    "WorkerVersionResponse",
    "build_rust_core_crashed_envelope",
    "validate_contract_payload",
]
