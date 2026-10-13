"""Bounded parser positions, shared by source evidence and worker payloads."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator


class AuditModel(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)


class SourceRange(AuditModel):
    """Half-open positions: page/row/paragraph/slide numbers start at one."""

    unit: Literal["pdf_page", "sheet_row", "paragraph", "table_row", "slide",
                  "text_char", "csv_record", "byte"]
    start: int = Field(ge=0)
    end: int = Field(ge=0)
    label: str | None = Field(default=None, max_length=512)
    column_start: int | None = Field(default=None, ge=1)
    column_end: int | None = Field(default=None, ge=1)

    @model_validator(mode="after")
    def check_bounds(self):
        if self.end <= self.start:
            raise ValueError("source ranges must contain at least one unit")
        if self.unit not in {"text_char", "byte"} and self.start == 0:
            raise ValueError("document positions start at one")
        if (self.column_start is None) != (self.column_end is None):
            raise ValueError("column boundaries must be supplied together")
        if self.column_start is not None and self.column_end <= self.column_start:
            raise ValueError("column range must contain at least one column")
        if self.column_start is not None and self.unit not in {"sheet_row", "table_row"}:
            raise ValueError("columns are only defined for table and sheet rows")
        return self


class ContentUnit(AuditModel):
    unit_id: str = Field(min_length=1, max_length=128)
    kind: Literal["heading", "paragraph", "table_header", "table_row", "unit",
                  "total", "slide", "text", "placeholder"]
    text_start: int = Field(ge=0)
    text_end: int = Field(ge=0)
    ranges: list[SourceRange] = Field(default_factory=list, max_length=128)

    @model_validator(mode="after")
    def check_bounds(self):
        if self.text_end <= self.text_start:
            raise ValueError("content unit must contain text")
        return self


class ParsedContentAudit(AuditModel):
    """Unknown positions are null; a verified empty document may have an empty list."""

    version: Literal[1] = 1
    read_ranges: list[SourceRange] | None = Field(default=None, max_length=10000)
    content_units: list[ContentUnit] | None = Field(default=None, max_length=10000)
    usable_chars: int | None = Field(default=None, ge=0)
    encoding: Literal["utf-8", "utf-8-sig", "gb18030"] | None = None
    detected_format: Literal["xls", "xlsx", "docx", "pptx", "pdf", "text", "csv"] | None = None

    @model_validator(mode="after")
    def check_metadata_size(self):
        if len(self.model_dump_json().encode("utf-8")) > 256 * 1024:
            raise ValueError("parser audit exceeds its metadata byte budget")
        if self.content_units and len({item.unit_id for item in self.content_units}) != len(self.content_units):
            raise ValueError("content unit identities must be distinct")
        end = 0
        for unit in self.content_units or []:
            if unit.text_start < end:
                raise ValueError("parser units must have ordered nonoverlapping offsets")
            end = unit.text_end
        return self

    def validate_content(self, content: str) -> None:
        if self.usable_chars is not None and self.usable_chars > len(content):
            raise ValueError("usable text cannot exceed parser content")
        if any(unit.text_end > len(content) for unit in self.content_units or []):
            raise ValueError("content units must stay within parser text")
