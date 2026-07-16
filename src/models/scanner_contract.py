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
    scanner_profile: RawScannerProfileV1
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


class ContextEnvelope(ContractModel):
    contract: Literal["ai_daily_context"]
    protocol_version: Literal[1]
    request_id: RequestId
    engine_version: NonEmpty4096
    engine_build: NonEmpty4096
    status: Literal["ok", "partial", "error"]
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
    error: Diagnostic

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
    warnings: Annotated[list[Diagnostic], Field(max_length=256)]
    error: Diagnostic | None
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
    "ContextEnvelope",
    "ContextSummary",
    "Diagnostic",
    "DoctorCheck",
    "DoctorRequest",
    "DoctorResponse",
    "EngineStatus",
    "InspectRunRequest",
    "InspectRunResponse",
    "NormalizedScannerProfileV1",
    "RawScannerProfileV1",
    "TransportErrorResponse",
    "VersionResponse",
    "WorkerParseRequest",
    "WorkerParseResponse",
    "WorkerParserLimits",
    "WorkerVersionResponse",
    "build_rust_core_crashed_envelope",
    "validate_contract_payload",
]
