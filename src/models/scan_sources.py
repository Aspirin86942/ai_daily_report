"""Native excerpts and their coverage, before any model request is built."""

from __future__ import annotations

from datetime import datetime, timedelta
from hashlib import sha256
from typing import Annotated, Any, Literal

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from .scanner_contract import RelativePath

Sha256 = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]


class ScanSource(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)

    source_id: Annotated[str, Field(pattern=r"^src_[0-9a-f]{64}$")]
    relative_path: RelativePath
    extension: str
    size_bytes: Annotated[int, Field(ge=0)]
    modified_at_utc: str
    observed_at_utc: str
    content_sha256: Sha256 | None
    excerpt_sha256: Sha256
    excerpt: str
    parser_backend: str
    worker_lane: Literal["rust_core", "rust_office_process_v2", "python_document_process_v2", "not_parsed", "local_ocr_preflight"]
    parse_status: Literal["success", "error", "timeout", "not_parsed"]
    parse_truncated: bool
    excerpt_truncated: bool
    coverage_limits: dict[str, Any]
    reason: str | None

    @field_validator("modified_at_utc", "observed_at_utc")
    @classmethod
    def require_utc(cls, value: str) -> str:
        if datetime.fromisoformat(value).utcoffset() != timedelta(0):
            raise ValueError("source times must be UTC-aware")
        return value

    @model_validator(mode="after")
    def validate_extracted_evidence(self) -> ScanSource:
        if sha256(self.excerpt.encode("utf-8")).hexdigest() != self.excerpt_sha256:
            raise ValueError("excerpt checksum mismatch")
        if self.excerpt and (self.parse_status != "success" or self.content_sha256 is None):
            raise ValueError("usable excerpt requires stable successful source evidence")
        expected = "src_" + sha256(self.relative_path.replace("\\", "/").encode("utf-8")).hexdigest()
        if expected != self.source_id:
            raise ValueError("source identity does not match its relative path")
        return self
