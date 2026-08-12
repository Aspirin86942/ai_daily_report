"""Windows-first native scanner 的严格领域合同。"""

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


class ScannerSettings(ContractModel):
    """调用方可调的 scanner 叶子；默认值和路由策略由 Rust 唯一拥有。"""

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
    fallback_after_timeout: bool | None = None
    legacy_office_enabled: bool | None = None
    pptx_include_notes: bool | None = None
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
    max_candidate_files: Annotated[int, Field(ge=1, le=1_000_000)] | None = None
    max_pdf_text_extractions: Annotated[int, Field(ge=0, le=100_000)] | None = None
    max_total_pdf_classification_pages: Annotated[int, Field(ge=0, le=10_000_000)] | None = None
    pdf_classification_timeout_ms: Annotated[int, Field(ge=100, le=60_000)] | None = None
    total_deadline_ms: Annotated[int, Field(ge=5_000, le=3_600_000)] | None = None
    worker_max_requests: Annotated[int, Field(ge=1, le=10_000)] | None = None
    worker_idle_ttl_ms: Annotated[int, Field(ge=1_000, le=600_000)] | None = None
    worker_rss_limit_bytes: Annotated[int, Field(ge=67_108_864, le=8_589_934_592)] | None = None

    @model_validator(mode="before")
    @classmethod
    def reject_explicit_nulls(cls, value: Any) -> Any:
        if isinstance(value, dict):
            null_fields = [key for key, item in value.items() if item is None]
            if null_fields:
                raise ValueError(
                    "scanner settings fields cannot be null: "
                    + ", ".join(sorted(null_fields))
                )
        return value


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
    backend: Literal["light_text_v2"]
    read_head_bytes: ReadBudget
    read_tail_bytes: ReadBudget
    max_chars: CharBudget
    excerpt_max_chars: CharBudget


class OfficeParseProfile(ContractModel):
    primary_backend: NonEmpty1024
    fallback_enabled: bool
    fallback_order: Annotated[
        list[Literal["python_office_v2", "python_sharepoint_text_v2"]],
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
    priority_policy_version: Literal["budget_nominal_v2"]
    compression_policy_version: Literal["markdown_context_v2"]

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


class NormalizedScannerSettings(ContractModel):
    """Rust 归一化后的完整设置和编译期策略结果。"""

    report_mode: ReportMode
    discovery: DiscoveryProfile
    execution: ExecutionProfile
    parse: ParseProfile
    context: ContextProfile
    admission_policy_version: Literal["budget_admission_v2"]
    classifier_policy_version: Literal["pdf_text_presence_v1"]
    max_candidate_files: Annotated[int, Field(ge=1, le=1_000_000)]
    max_pdf_text_extractions: Annotated[int, Field(ge=0, le=100_000)]
    max_total_pdf_classification_pages: Annotated[int, Field(ge=0, le=10_000_000)]
    pdf_classification_timeout_ms: Annotated[int, Field(ge=100, le=60_000)]
    total_deadline_ms: Annotated[int, Field(ge=5_000, le=3_600_000)]
    worker_max_requests: Annotated[int, Field(ge=1, le=10_000)]
    worker_idle_ttl_ms: Annotated[int, Field(ge=1_000, le=600_000)]
    worker_rss_limit_bytes: Annotated[int, Field(ge=67_108_864, le=8_589_934_592)]

    @model_validator(mode="after")
    def validate_mode_context(self) -> "NormalizedScannerSettings":
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
    scanner_settings: ScannerSettings
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
        "SCANNER_DB_SCHEMA_MISMATCH",
        "DIAGNOSTICS_AGGREGATED",
        "SNAPSHOT_REUSE_PROJECTED_AS_FRESH",
        "PARSE_CACHE_NOT_APPLICABLE_PROJECTED_AS_MISS",
        "CACHE_MISS_REASON_PROJECTED_AS_NEW_FILE",
        "SOURCE_GUARD_NOT_PROJECTED",
        "WORKER_RSS_UNAVAILABLE",
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
        "rust_office_process_v2",
        "python_document_process_v2",
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


# ---------------------------------------------------------------------------
# Complete scanner evidence and worker contracts.
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

_PARSE_CACHE_MISS_REASONS_V2 = frozenset(
    {
        "new_file",
        "source_version_changed",
        "parser_identity_changed",
        "entry_absent_or_evicted",
    }
)
_CLASSIFICATION_CACHE_MISS_REASONS_V2 = frozenset(
    {
        "new_file",
        "source_version_changed",
        "classifier_identity_changed",
        "entry_absent_or_evicted",
    }
)
_BODY_PARSER_LANES = {
    "light_text_v2": "rust_core",
    "rust_office_oxide_v2": "rust_office_process_v2",
    "rust_xlsx_bounded_v2": "rust_office_process_v2",
    "python_pdf_text_v2": "python_document_process_v2",
    "python_office_v2": "python_document_process_v2",
    "python_sharepoint_text_v2": "python_document_process_v2",
}


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
        if self.classification_cache_status == "miss":
            if (
                self.classification_cache_miss_reason
                not in _CLASSIFICATION_CACHE_MISS_REASONS_V2
            ):
                raise ValueError(
                    "classification cache miss reason is not in the v2 allowlist"
                )
        if self.classification_cache_status != "miss" and self.classification_cache_miss_reason:
            raise ValueError("non-miss classification cache must have an empty miss reason")

        def require_zero_execution(transport: ClassificationTransport) -> None:
            if (
                self.run_inspected_pages != 0
                or self.duration_ms != 0
                or self.transport != transport
                or self.attempt_count != 0
            ):
                raise ValueError(
                    "non-executing classification provenance must have zero execution"
                )

        if self.status in {"text_in_parse_window", "no_text_in_parse_window"}:
            if self.page_count is None or self.result_examined_pages is None:
                raise ValueError("text/no-text classification requires page provenance")
            if self.page_count == 0:
                raise ValueError("text/no-text classification page_count must be positive")
            if self.nominal_charged_pages == 0:
                raise ValueError("classified PDF requires a positive nominal page charge")
            window_pages = min(self.page_count, self.nominal_charged_pages)
            if self.status == "text_in_parse_window":
                if not 1 <= self.result_examined_pages <= window_pages:
                    raise ValueError("text classification pages must fit the parse window")
            elif self.result_examined_pages != window_pages:
                raise ValueError("no-text classification must examine the complete window")
            if self.classification_cache_status == "fresh":
                require_zero_execution("not_applicable")
            elif self.classification_cache_status == "snapshot":
                require_zero_execution("snapshot")
            elif self.classification_cache_status == "miss":
                if self.transport not in {"session", "one_shot"} or self.attempt_count == 0:
                    raise ValueError(
                        "executed classification miss requires a started transport"
                    )
            else:
                raise ValueError("text/no-text classification cannot be not_eligible")
        elif self.status in {"unknown", "error"}:
            if (
                self.classification_cache_status != "miss"
                or self.transport not in {"session", "one_shot"}
                or self.attempt_count == 0
                or self.nominal_charged_pages == 0
            ):
                raise ValueError(
                    "unknown/error classification requires an executed miss"
                )
            if (
                self.result_examined_pages is not None
                and self.result_examined_pages > self.nominal_charged_pages
            ):
                raise ValueError(
                    "typed failure result pages exceed the nominal page charge"
                )
            if (
                self.page_count is not None
                and self.result_examined_pages is not None
                and self.result_examined_pages
                > min(self.page_count, self.nominal_charged_pages)
            ):
                raise ValueError("typed failure result pages exceed the parse window")
        else:
            if (
                self.page_count is not None
                or self.result_examined_pages != 0
                or self.nominal_charged_pages != 0
                or self.classification_cache_status != "not_eligible"
            ):
                raise ValueError(
                    "not_classified_by_budget requires the not_eligible zero-page shape"
                )
            require_zero_execution("not_applicable")
        if self.run_inspected_pages is not None:
            max_run_pages = self.attempt_count * self.nominal_charged_pages
            if self.run_inspected_pages > max_run_pages:
                raise ValueError(
                    "run inspected pages exceed the started attempt page budget"
                )
            if (
                self.classification_cache_status == "miss"
                and self.result_examined_pages is not None
                and self.run_inspected_pages < self.result_examined_pages
            ):
                raise ValueError(
                    "observable run pages cannot be smaller than result pages"
                )
        return self


def _validate_v2_parse_provenance(
    *,
    parse_status: str,
    parser_backend: str,
    worker_lane: str,
    parse_cache_status: str,
    classification_status: str | None,
) -> None:
    metadata = parser_backend == "pdf_metadata_v2" and worker_lane == "rust_core"
    not_parsed = parser_backend == "not_parsed" and worker_lane == "not_parsed"
    body_parser = _BODY_PARSER_LANES.get(parser_backend) == worker_lane
    if not (metadata or not_parsed or body_parser):
        raise ValueError("parser backend and worker lane are not a frozen v2 route")

    if classification_status == "no_text_in_parse_window":
        if parse_status != "success" or not metadata:
            raise ValueError(
                "no-text classification requires metadata-only success provenance"
            )
    elif classification_status in {"unknown", "error"}:
        allowed_status = (
            parse_status in {"error", "timeout"}
            if classification_status == "unknown"
            else parse_status == "error"
        )
        if (
            not allowed_status
            or not not_parsed
            or parse_cache_status != "not_applicable"
        ):
            raise ValueError(
                "classifier failure must map to not_parsed provenance and Error/Timeout"
            )
    elif classification_status == "not_classified_by_budget":
        if parse_status != "not_parsed" or not not_parsed:
            raise ValueError(
                "classification budget exclusion must map to not_parsed provenance"
            )
    if metadata and classification_status != "no_text_in_parse_window":
        raise ValueError(
            "pdf_metadata_v2 is only valid for no-text classification provenance"
        )

    if parse_cache_status in {"fresh", "miss"}:
        if not body_parser:
            raise ValueError("fresh/miss parse provenance requires a body parser")
    elif parse_cache_status == "snapshot":
        if parse_status == "success" and not (body_parser or metadata):
            raise ValueError("snapshot success requires parser or metadata provenance")
        if parse_status == "not_parsed" and not not_parsed:
            raise ValueError("snapshot NotParsed requires not_parsed provenance")
    elif parse_status == "success":
        if not metadata:
            raise ValueError(
                "not_applicable Success requires pdf_metadata_v2/rust_core"
            )
    elif not not_parsed:
        raise ValueError(
            "not_applicable NotParsed/Error/Timeout requires not_parsed provenance"
        )


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
        "rust_office_process_v2",
        "python_document_process_v2",
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
    parse_attempt_count: Annotated[int, Field(ge=0, le=3)]
    final_diagnostic: Diagnostic | None
    pdf_classification: PdfClassificationAuditV1 | None

    @model_validator(mode="after")
    def validate_source_guard_and_cache(self) -> "FileAuditV2":
        if self.source_guard_kind == "unavailable":
            if self.source_guard_sha256 is not None:
                raise ValueError("unavailable source guard must have a null hash")
        elif self.source_guard_sha256 is None:
            raise ValueError("source guard kind requires a sha256")
        if self.parse_cache_status == "miss":
            if self.cache_miss_reason not in _PARSE_CACHE_MISS_REASONS_V2:
                raise ValueError("parse cache miss reason is not in the v2 allowlist")
        if self.parse_cache_status != "miss" and self.cache_miss_reason:
            raise ValueError("non-miss parse cache must have an empty miss reason")
        if self.parse_cache_status == "fresh":
            if (
                self.parse_status != "success"
                or self.parse_transport != "not_applicable"
                or self.parse_attempt_count != 0
                or self.parse_duration_ms != 0
            ):
                raise ValueError(
                    "fresh parse-cache provenance must have zero execution"
                )
        elif self.parse_cache_status == "snapshot":
            if (
                self.parse_status in {"error", "timeout"}
                or self.parse_transport != "snapshot"
                or self.parse_attempt_count != 0
                or self.parse_duration_ms != 0
            ):
                raise ValueError("snapshot parse provenance must have zero execution")
        elif self.parse_cache_status == "miss":
            if (
                self.parse_status not in {"success", "error", "timeout"}
                or self.parse_transport
                not in {"session", "one_shot", "rust_in_process"}
                or self.parse_attempt_count == 0
            ):
                raise ValueError("parse miss requires a started parser transport")
        elif (
            self.parse_transport != "not_applicable"
            or self.parse_attempt_count != 0
            or self.parse_duration_ms != 0
        ):
            raise ValueError("not_applicable parse provenance must have zero execution")
        _validate_v2_parse_provenance(
            parse_status=self.parse_status,
            parser_backend=self.parser_backend,
            worker_lane=self.worker_lane,
            parse_cache_status=self.parse_cache_status,
            classification_status=(
                self.pdf_classification.status
                if self.pdf_classification is not None
                else None
            ),
        )
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

class ScannerEvidence(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    request_id: RequestId
    scan_run_id: PositiveInt
    context_run_id: PositiveInt | None
    run_status: Literal[
        "running",
        "success",
        "partial",
        "error",
        "abandoned",
    ]
    summary: ContextSummary
    stage_metrics: Annotated[list[StageMetric], Field(max_length=32)]
    extension_metrics: Annotated[list[ExtensionMetric], Field(max_length=256)]
    files: Annotated[list[FileAuditV2], Field(max_length=1_000_000)]
    decisions: Annotated[list[ContextDecision], Field(max_length=1_000_000)]
    warnings: Annotated[list[Diagnostic], Field(max_length=100_000)]
    artifact_id: PositiveInt | None
    reused_from_context_run_id: PositiveInt | None
    reuse_kind: ReuseKind
    execution_metrics: ExecutionMetricsV2

    @model_validator(mode="after")
    def validate_status_and_reuse(self) -> "ScannerEvidence":
        if self.run_status in {"success", "partial"} and self.artifact_id is None:
            raise ValueError("successful scanner evidence requires artifact_id")
        if self.run_status == "error" and self.artifact_id is not None:
            raise ValueError("error scanner evidence must have a null artifact_id")
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


SCHEMA_MODELS: dict[str, type[ContractModel]] = {
    "build-context-request-v1.schema.json": BuildContextRequest,
    "context-envelope-v1.schema.json": ContextEnvelope,
    "diagnostic-v1.schema.json": Diagnostic,
    "doctor-request-v1.schema.json": DoctorRequest,
    "doctor-response-v1.schema.json": DoctorResponse,
    "normalized-scanner-settings.schema.json": NormalizedScannerSettings,
    "scanner-settings.schema.json": ScannerSettings,
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

    return parsed


__all__ = [
    "AdapterPaths",
    "BuildContextRequest",
    "ContextEnvelope",
    "ContextProfile",
    "ContextSummary",
    "Diagnostic",
    "DoctorCheck",
    "DoctorRequest",
    "DoctorResponse",
    "EngineStatus",
    "ExecutionMetricsV2",
    "FileAuditV2",
    "ScannerEvidence",
    "NormalizedScannerSettings",
    "PdfClassificationAuditV1",
    "ScannerSettings",
    "build_rust_core_crashed_envelope",
    "validate_contract_payload",
]
