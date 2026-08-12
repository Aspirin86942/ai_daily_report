"""Worker v2 operation payloads shared by the Python worker implementation."""

from __future__ import annotations

from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator


ParserBackend = Literal[
    "rust_office_oxide_v2",
    "rust_xlsx_bounded_v2",
    "python_office_v2",
    "python_pdf_text_v2",
    "python_sharepoint_text_v2",
]
WorkerLane = Literal["rust_office_process_v2", "python_document_process_v2"]


class WorkerModel(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)


class WorkerDiagnostic(WorkerModel):
    error_code: Annotated[str, Field(min_length=1, max_length=1024)]
    message: Annotated[str, Field(min_length=1, max_length=4096)]
    retryable: bool
    stage: Annotated[str, Field(min_length=1, max_length=1024)]
    file_path: str | None
    backend: Annotated[str | None, Field(min_length=1, max_length=1024)]


class ClassifyRequest(WorkerModel):
    file_path: Annotated[str, Field(min_length=3, max_length=32_767)]
    source_version: Annotated[str, Field(pattern=r"^mtime_ns=[0-9]+:size=[0-9]+$")]
    max_pages: Annotated[int, Field(ge=1, le=10_000)]
    policy_version: Literal["pdf_text_presence_v1"]

    @model_validator(mode="after")
    def validate_path(self) -> "ClassifyRequest":
        if not (
            self.file_path.startswith("\\\\")
            or (len(self.file_path) >= 3 and self.file_path[1:3] in {":\\", ":/"})
        ):
            raise ValueError("file_path must be an absolute Windows path")
        return self


class ClassifyResult(WorkerModel):
    status: Literal[
        "text_in_parse_window",
        "no_text_in_parse_window",
        "unknown",
        "error",
    ]
    page_count: Annotated[int, Field(ge=0)] | None
    result_examined_pages: Annotated[int, Field(ge=0)] | None
    diagnostic: WorkerDiagnostic | None

    @model_validator(mode="after")
    def validate_status(self) -> "ClassifyResult":
        if self.status in {"text_in_parse_window", "no_text_in_parse_window"}:
            if self.diagnostic is not None:
                raise ValueError("text/no-text result must not carry a diagnostic")
            if (
                self.page_count is None
                or self.result_examined_pages is None
                or self.page_count == 0
                or self.result_examined_pages == 0
                or self.result_examined_pages > self.page_count
            ):
                raise ValueError("text/no-text result page counts are inconsistent")
        else:
            if self.diagnostic is None:
                raise ValueError("unknown/error result requires a diagnostic")
            if self.diagnostic.retryable != (self.status == "unknown"):
                raise ValueError("unknown must be retryable and error must not be")
        return self


class OfficeLimits(WorkerModel):
    kind: Literal["office"]
    excel_max_sheets: Annotated[int, Field(ge=1, le=1024)]
    excel_max_rows: Annotated[int, Field(ge=1, le=1_048_576)]
    excel_max_columns: Annotated[int, Field(ge=1, le=16_384)]
    docx_max_paragraphs: Annotated[int, Field(ge=1, le=1_000_000)]
    docx_max_tables: Annotated[int, Field(ge=1, le=100_000)]
    docx_table_max_rows: Annotated[int, Field(ge=1, le=1_048_576)]
    docx_table_max_cols: Annotated[int, Field(ge=1, le=16_384)]
    pptx_max_slides: Annotated[int, Field(ge=1, le=100_000)]
    pptx_include_notes: bool
    document_excerpt_max_chars: Annotated[int, Field(ge=1, le=10_000_000)]


class PdfLimits(WorkerModel):
    kind: Literal["pdf"]
    max_pages: Annotated[int, Field(ge=1, le=10_000)]
    excerpt_max_chars: Annotated[int, Field(ge=1, le=10_000_000)]


class SharePointTextLimits(WorkerModel):
    kind: Literal["sharepoint_text"]
    excerpt_max_chars: Annotated[int, Field(ge=1, le=10_000_000)]


ParserLimits = Annotated[
    OfficeLimits | PdfLimits | SharePointTextLimits,
    Field(discriminator="kind"),
]

_ROUTES: dict[str, tuple[set[str], WorkerLane, str]] = {
    "rust_office_oxide_v2": ({".docx", ".pptx"}, "rust_office_process_v2", "office"),
    "rust_xlsx_bounded_v2": ({".xlsx"}, "rust_office_process_v2", "office"),
    "python_office_v2": (
        {".docx", ".pptx", ".xls", ".xlsx"},
        "python_document_process_v2",
        "office",
    ),
    "python_pdf_text_v2": ({".pdf"}, "python_document_process_v2", "pdf"),
    "python_sharepoint_text_v2": (
        {".doc", ".ppt"},
        "python_document_process_v2",
        "sharepoint_text",
    ),
}


class ParseRequest(WorkerModel):
    file_path: Annotated[str, Field(min_length=3, max_length=32_767)]
    file_type: Annotated[str, Field(pattern=r"^\.[a-z0-9]{1,31}$")]
    backend: ParserBackend
    remaining_timeout_ms: Annotated[int, Field(ge=1, le=3_600_000)]
    max_file_size_bytes: Annotated[int, Field(ge=1, le=4_294_967_296)]
    parser_limits: ParserLimits
    expected_source_version: Annotated[
        str,
        Field(pattern=r"^mtime_ns=[0-9]+:size=[0-9]+$"),
    ]

    @model_validator(mode="after")
    def validate_route_and_path(self) -> "ParseRequest":
        if not (
            self.file_path.startswith("\\\\")
            or (len(self.file_path) >= 3 and self.file_path[1:3] in {":\\", ":/"})
        ):
            raise ValueError("file_path must be an absolute Windows path")
        extensions, _, limit_kind = _ROUTES[self.backend]
        if self.file_type not in extensions or self.parser_limits.kind != limit_kind:
            raise ValueError("parse request route is inconsistent")
        return self


class ParseResult(WorkerModel):
    file_path: Annotated[str, Field(min_length=3, max_length=32_767)]
    file_type: Annotated[str, Field(pattern=r"^\.[a-z0-9]{1,31}$")]
    content: str
    parser_backend: ParserBackend
    worker_lane: WorkerLane
    truncated: bool
    duration_ms: Annotated[int, Field(ge=0)]
    observed_source_version: Annotated[
        str,
        Field(pattern=r"^mtime_ns=[0-9]+:size=[0-9]+$"),
    ]

    @model_validator(mode="after")
    def validate_route_and_path(self) -> "ParseResult":
        if not (
            self.file_path.startswith("\\\\")
            or (len(self.file_path) >= 3 and self.file_path[1:3] in {":\\", ":/"})
        ):
            raise ValueError("file_path must be an absolute Windows path")
        extensions, lane, _ = _ROUTES[self.parser_backend]
        if self.file_type not in extensions or self.worker_lane != lane:
            raise ValueError("parse result route is inconsistent")
        return self


class WorkerOperationError(Exception):
    """Expected operation failure represented by the outer worker-v2 envelope."""

    def __init__(
        self,
        *,
        error_code: str,
        message: str,
        retryable: bool,
        file_path: str | None,
        backend: str | None,
    ) -> None:
        super().__init__(message)
        self.error_code = error_code
        self.message = message
        self.retryable = retryable
        self.file_path = file_path
        self.backend = backend
